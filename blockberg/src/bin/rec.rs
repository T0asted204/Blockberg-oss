//! blockberg-rec: records one candle per minute for each item on the record list, forever.
//! Runs without the app. Data dir: $BLOCKBERG_DATA or ~/.local/share/blockberg.
//!
//!   blockberg-rec [--listen ADDR]                          main recorder (the desktop). With --listen it also answers the standby.
//!   blockberg-rec --standby --peer ADDR                    standby (the laptop): keeps only the last 10 minutes while the main
//!                                                          recorder is up; if it goes down, keeps everything and keeps recording; when it
//!                                                          is back, sends what it collected so the main files fill in the gap.
//!   blockberg-rec --import-history FILE                    one-off: turn the app's saved price snapshots into candles.
//!
//! The standby and the main recorder talk over a tiny TCP protocol, protected by a shared secret in ~/.config/blockberg/rec_secret:
//!   STATUS\n                         -> OK <now> <last successful price fetch>\n
//!   MERGE <secret> <ITEM> <count>\n  + count * 48 bytes of candles -> OK <new minutes>\n
#[path = "../bb.rs"]
mod bb;
#[path = "../candles.rs"]
mod candles;

use std::{collections::HashMap, io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf, sync::{atomic::{AtomicI64, Ordering}, Arc, Mutex}, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};
use candles::*;
use tokio::{io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader}, net::{TcpListener, TcpStream}, time::timeout};

fn unix() -> i64 { SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0) }

/// Held while a candle file is appended to, merged or pruned, so the three never interleave.
static FILE_LOCK: Mutex<()> = Mutex::new(());
fn locked<T>(f: impl FnOnce() -> T) -> T { let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner()); f() }

const KEEP_SECS: i64 = 600;          // what the standby keeps while the main recorder is healthy
const DOWN_AFTER: u32 = 3;           // failed checks (10 s apart) before the standby takes over
const SYNC_EVERY: u64 = 300;         // while healthy, the standby also sends its last minutes this often, to fill small gaps
const COMPACT_AFTER: i64 = 30 * 86_400;   // minute candles kept this long, then rolled up into hours
const STALE_FETCH: i64 = 240;        // main recorder reachable but its price feed silent this long counts as down

fn secret_path() -> PathBuf {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/blockberg/rec_secret")).unwrap_or_else(|| PathBuf::from("rec_secret"))
}

/// The shared secret: the main recorder creates it on first start; the standby needs a copy.
fn secret(create: bool) -> Option<String> {
    let p = secret_path();
    if let Ok(s) = std::fs::read_to_string(&p) { let s = s.trim().to_string(); if s.len() >= 16 { return Some(s); } }
    if !create { return None; }
    let mut b = [0u8; 24];
    std::fs::File::open("/dev/urandom").ok().and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b).ok())?;
    let s: String = b.iter().map(|x| format!("{x:02x}")).collect();
    if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&p).ok()?;
    f.write_all(s.as_bytes()).ok()?;
    Some(s)
}

fn valid_id(id: &str) -> bool { !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_' || b == b':' || b == b'-') }

fn same(a: &str, b: &str) -> bool { a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0 }

// ---------------------------------------------------------------- the main recorder's server

async fn serve(mut sock: TcpStream, secret: String, last_fetch: Arc<AtomicI64>) {
    let (rd, mut wr) = sock.split();
    let mut rd = BufReader::new(rd.take(1024 * 1024 * 64));
    let mut line = String::new();
    if timeout(Duration::from_secs(10), rd.read_line(&mut line)).await.map_or(true, |r| r.is_err()) || line.len() > 300 { return; }
    let parts: Vec<&str> = line.split_whitespace().collect();
    match parts.as_slice() {
        ["STATUS"] => { let _ = wr.write_all(format!("OK {} {}\n", unix(), last_fetch.load(Ordering::Relaxed)).as_bytes()).await; }
        ["MERGE", sec, id, n] => {
            let n: usize = n.parse().unwrap_or(usize::MAX);
            if !same(sec, &secret) { let _ = wr.write_all(b"ERR secret\n").await; return; }
            if !valid_id(id) || n > 600_000 { let _ = wr.write_all(b"ERR bad request\n").await; return; }
            let mut buf = vec![0u8; n * REC];
            if timeout(Duration::from_secs(120), rd.read_exact(&mut buf)).await.map_or(true, |r| r.is_err()) { let _ = wr.write_all(b"ERR short\n").await; return; }
            let cs: Vec<Candle> = buf.chunks_exact(REC).filter_map(Candle::decode).filter(|c| c.t > 0 && c.t % 60 == 0 && c.ask[0].is_finite() && c.bid[0].is_finite()).collect();
            let id = id.to_string();
            let added = tokio::task::spawn_blocking(move || locked(|| merge_into_file(&id, &cs))).await;
            match added { Ok(Ok(k)) => { let _ = wr.write_all(format!("OK {k}\n").as_bytes()).await; } _ => { let _ = wr.write_all(b"ERR write\n").await; } }
        }
        ["LIST"] => { let _ = wr.write_all(format!("OK {}\n", load_list().join(",")).as_bytes()).await; }
        _ => { let _ = wr.write_all(b"ERR what\n").await; }
    }
}

// ---------------------------------------------------------------- the standby's side

/// (peer's clock, peer's last successful price fetch), or None when it cannot be reached.
async fn peer_status(addr: &str) -> Option<(i64, i64)> {
    let r = timeout(Duration::from_secs(4), async {
        let mut s = TcpStream::connect(addr).await.ok()?;
        s.write_all(b"STATUS\n").await.ok()?;
        let mut out = String::new();
        BufReader::new(s).read_line(&mut out).await.ok()?;
        let p: Vec<&str> = out.split_whitespace().collect();
        if p.first() != Some(&"OK") { return None; }
        Some((p.get(1)?.parse().ok()?, p.get(2)?.parse().ok()?))
    }).await;
    r.ok().flatten()
}

/// The main recorder's item list, so the standby backs up exactly the same items.
async fn peer_list(addr: &str) -> Option<Vec<String>> {
    let r = timeout(Duration::from_secs(4), async {
        let mut s = TcpStream::connect(addr).await.ok()?;
        s.write_all(b"LIST\n").await.ok()?;
        let mut out = String::new();
        BufReader::new(s.take(1024 * 64)).read_line(&mut out).await.ok()?;
        let body = out.trim().strip_prefix("OK ")?;
        Some(body.split(',').filter(|x| valid_id(x)).map(String::from).collect::<Vec<_>>())
    }).await;
    r.ok().flatten().filter(|l| !l.is_empty())
}

async fn peer_push(addr: &str, secret: &str, id: &str, cs: &[Candle]) -> bool {
    let r = timeout(Duration::from_secs(120), async {
        let mut s = TcpStream::connect(addr).await.ok()?;
        let mut msg = format!("MERGE {secret} {id} {}\n", cs.len()).into_bytes();
        for c in cs { msg.extend_from_slice(&c.encode()); }
        s.write_all(&msg).await.ok()?;
        let mut out = String::new();
        BufReader::new(s).read_line(&mut out).await.ok()?;
        out.starts_with("OK").then_some(())
    }).await;
    matches!(r, Ok(Some(())))
}

/// Send everything newer than `since` (0 = everything) to the main recorder. True when every item went through.
async fn push_all(addr: &str, secret: &str, since: i64) -> bool {
    let mut ok = true;
    for id in locked(list_ids) {
        let cs = locked(|| load_since(&id, since));
        if cs.is_empty() { continue; }
        if !peer_push(addr, secret, &id, &cs).await { ok = false; }
    }
    ok
}

// ---------------------------------------------------------------- recording (both modes)

fn flush(cur: &mut HashMap<String, Candle>) {
    for (id, c) in cur.drain() { let _ = locked(|| append(&id, &c)); }
}

#[derive(PartialEq, Clone, Copy)]
enum Mode { Main, Standby, Collect }

fn arg(name: &str) -> Option<String> { let a: Vec<String> = std::env::args().collect(); a.iter().position(|x| x == name).and_then(|i| a.get(i + 1)).cloned() }
fn flag(name: &str) -> bool { std::env::args().any(|x| x == name) }

#[tokio::main]
async fn main() {
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);

    // one-off: import the app's saved snapshots
    if let Some(file) = arg("--import-history") {
        let v: serde_json::Value = match std::fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str(&t).ok()) { Some(v) => v, None => { eprintln!("cannot read {file}"); return; } };
        let (mut items, mut minutes) = (0, 0);
        for (id, rows) in v.as_object().into_iter().flatten() {
            let snaps: Vec<(f64, f64, f64)> = rows.as_array().into_iter().flatten().filter_map(|r| Some((r[0].as_f64()?, r[1].as_f64()?, r[2].as_f64()?))).collect();
            if snaps.is_empty() || !valid_id(id) { continue; }
            if let Ok(k) = merge_into_file(id, &from_snapshots(&snaps)) { items += 1; minutes += k; }
        }
        println!("imported {items} items, {minutes} new minutes");
        return;
    }

    let standby = flag("--standby");
    let peer = arg("--peer");
    if standby && peer.is_none() { eprintln!("--standby needs --peer HOST:PORT"); return; }

    let pidfile = dir.join(if standby { "standby.pid" } else { "recorder.pid" });
    if let Some(pid) = std::fs::read_to_string(&pidfile).ok().and_then(|s| s.trim().parse::<u32>().ok()) {
        if pid != std::process::id() && std::path::Path::new(&format!("/proc/{pid}")).exists() {
            eprintln!("blockberg-rec already running (pid {pid})");
            return;
        }
    }
    let _ = std::fs::write(&pidfile, std::process::id().to_string());

    let secret = match secret(!standby) { Some(s) => s, None => { eprintln!("missing {}: copy it from the main recorder's machine", secret_path().display()); return; } };
    let last_fetch = Arc::new(AtomicI64::new(0));
    if let Some(addr) = arg("--listen") {
        match TcpListener::bind(&addr).await {
            Ok(l) => {
                eprintln!("listening for the standby on {addr}");
                let (sec, lf) = (secret.clone(), last_fetch.clone());
                tokio::spawn(async move { loop { if let Ok((s, _)) = l.accept().await { tokio::spawn(serve(s, sec.clone(), lf.clone())); } } });
            }
            Err(e) => eprintln!("cannot listen on {addr}: {e} (the standby will not be able to reach this recorder)"),
        }
    }

    let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).build().unwrap();
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal");
    let mut cur: HashMap<String, Candle> = HashMap::new();
    let (mut list, mut list_at) = (load_list(), Instant::now());
    let mut last_ok = Instant::now();
    let mut mode = if standby { Mode::Standby } else { Mode::Main };
    let (mut fails, mut last_sync, mut last_prune_minute) = (0u32, Instant::now(), 0i64);
    let mut last_compact = Instant::now() - Duration::from_secs(86_400 - 600);
    let mut list_sync = Instant::now() - Duration::from_secs(3600);
    eprintln!("blockberg-rec started ({}): {} items, data in {}", match mode { Mode::Main => "main", _ => "standby" }, list.len(), dir.display());
    loop {
        match bb::fetch_bazaar(&client).await {
            Ok(products) => {
                last_ok = Instant::now();
                last_fetch.store(unix(), Ordering::Relaxed);
                if list_at.elapsed() > Duration::from_secs(60) { list = load_list(); list_at = Instant::now(); }
                let now = unix();
                let minute = now - now.rem_euclid(60);
                // every bazaar item, so any item's chart has history (the list only says which ones the app cares about most)
                let _ = &list;
                for (id, p) in products.iter() {
                    if !valid_id(id) { continue; }
                    let (a, b) = (p.ask() as f32, p.bid() as f32);
                    if a <= 0.0 && b <= 0.0 { continue; }
                    match cur.get_mut(id.as_str()) {
                        Some(c) if c.t == minute => c.tick(a, b),
                        Some(c) => { let _ = locked(|| append(id, c)); *c = Candle::new(minute, a, b); }
                        None => { cur.insert(id.clone(), Candle::new(minute, a, b)); }
                    }
                }
            }
            Err(e) => { if last_ok.elapsed() > Duration::from_secs(120) { eprintln!("price feed down: {e}"); last_ok = Instant::now(); } }
        }

        if let Some(addr) = peer.as_deref().filter(|_| standby) {
            let now = unix();
            let healthy = matches!(peer_status(addr).await, Some((_, fetched)) if now - fetched <= STALE_FETCH);
            if healthy { fails = 0; } else { fails += 1; }
            if healthy && list_sync.elapsed() > Duration::from_secs(60) {
                list_sync = Instant::now();
                if let Some(l) = peer_list(addr).await { if l != list { let _ = std::fs::write(list_path(), serde_json::to_string(&l).unwrap_or_default()); list = l; list_at = Instant::now(); eprintln!("item list updated from the main recorder: {} items", list.len()); } }
            }
            if mode == Mode::Standby && fails >= DOWN_AFTER {
                mode = Mode::Collect;
                eprintln!("main recorder is down: keeping everything from now on, and recording");
            }
            if healthy && mode == Mode::Collect {
                // it is back: give it everything we kept, then go back to holding 10 minutes
                if push_all(addr, &secret, 0).await {
                    eprintln!("main recorder is back: sent what was collected, back to standby");
                    mode = Mode::Standby; last_sync = Instant::now();
                } else { eprintln!("main recorder is back but did not take all of it: will try again"); }
            } else if healthy && mode == Mode::Standby && last_sync.elapsed() > Duration::from_secs(SYNC_EVERY) {
                push_all(addr, &secret, now - 900).await;   // fills any small hole in the main recorder's files
                last_sync = Instant::now();
            }
            if mode == Mode::Standby {
                let minute = now - now.rem_euclid(60);
                if minute != last_prune_minute {
                    last_prune_minute = minute;
                    locked(|| for id in list_ids() { let _ = prune_before(&id, now - KEEP_SECS); });
                }
            }
        }

        // once a day: minutes older than 30 days become hourly candles (all items at 1-minute forever would be ~35 GB a year)
        if mode == Mode::Main && last_compact.elapsed() > Duration::from_secs(86_400) {
            last_compact = Instant::now();
            let cutoff = unix() - COMPACT_AFTER;
            let ids = list_ids();
            let saved: usize = tokio::task::spawn_blocking(move || ids.iter().map(|id| locked(|| compact_before(id, cutoff, 3600)).unwrap_or(0)).sum()).await.unwrap_or(0);
            if saved > 0 { eprintln!("compacted old minutes into hours: {saved} records saved"); }
        }

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(10)) => {}
            _ = tokio::signal::ctrl_c() => { flush(&mut cur); break; }
            _ = sigterm.recv() => { flush(&mut cur); break; }
        }
    }
    let _ = std::fs::remove_file(&pidfile);
}
