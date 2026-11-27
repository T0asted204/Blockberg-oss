//! Blockberg as a server and a client.
//!
//! * **Server** (`blockberg-gui --server`): no window. It owns everything that decides or remembers: the connection to the
//!   agent, the iceberg jobs, claims, the action queue and the journal. It listens on 127.0.0.1:7780 and every
//!   connection has to present the shared secret in `~/.config/blockberg/server_secret`.
//! * **Client** (the normal app when a server is running): shows the server's state and sends it commands. Closing,
//!   rebuilding or crashing the client changes nothing on the server.
//! * **Standalone** (no server running): the app does it all itself, as before.
//!
//! Restarts: the server writes a journal of what it asked the agent to do and what came back. After a restart it reads
//! its saved jobs, takes over any order it finds in the real order menu, and says which actions were unfinished.
//! "Restart server" is a queued job: it waits for what is running to finish, lets nothing new start, then restarts.
use super::*;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{atomic::{AtomicBool, AtomicUsize, Ordering}, Arc, Mutex, OnceLock};
use tokio::{io::{AsyncBufReadExt, AsyncWriteExt, BufReader}, net::{TcpListener, TcpStream}};

pub fn server_addr() -> String { crate::bb::profile::entry().server }
/// Held for as long as a real engine (standalone app or server) runs, so two of them never control the agent at once.
pub fn lock_addr() -> String { crate::bb::profile::entry().lock }

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode { Standalone, Server, Client }

// ---------------------------------------------------------------- secret

pub fn config_dir() -> PathBuf { std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/blockberg")).unwrap_or_else(|| PathBuf::from(".")) }
pub fn secret_path() -> PathBuf { config_dir().join("server_secret") }

/// The shared secret. `create`: make one (random, readable only by you) if there is none yet.
pub fn load_secret(create: bool) -> Option<String> {
    if let Ok(s) = std::fs::read_to_string(secret_path()) { let s = s.trim().to_string(); if s.len() >= 16 { return Some(s); } }
    if !create { return None; }
    let mut buf = [0u8; 24];
    std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf)).ok()?;
    let s: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    let _ = std::fs::create_dir_all(config_dir());
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(secret_path()).ok()?;
        f.write_all(s.as_bytes()).ok()?;
    }
    Some(s)
}

fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Is a Blockberg server accepting connections on this machine?
pub fn server_running() -> bool {
    std::net::TcpStream::connect_timeout(&server_addr().parse().unwrap(), Duration::from_millis(300)).is_ok()
}

/// Claim the "one engine" lock. The returned listener must stay alive.
pub fn take_engine_lock() -> Option<std::net::TcpListener> { std::net::TcpListener::bind(lock_addr()).ok() }

// ---------------------------------------------------------------- journal

static JOURNAL_FILE: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

fn journal_path() -> PathBuf { std::env::var_os("HOME").map(|_| crate::bb::profile::data_dir().join("journal.jsonl")).unwrap_or_else(|| PathBuf::from("journal.jsonl")) }

/// Commands that change something in the game: each is written to the journal before it is sent.
fn changes_state(cmd: &str) -> bool {
    matches!(cmd, "buyorder" | "selloffer" | "requote" | "instabuy" | "instasell" | "claimorder" | "claim" | "cancel" | "fliporder" | "requote_sell" | "rejoin" | "tradesellqty" | "tradesell" | "sackget" | "bulksell" | "pipeline" | "pickup" | "insert_sacks" | "sellinv")
}

pub fn init_journal() {
    let p = journal_path();
    if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
    if std::fs::metadata(&p).map_or(false, |m| m.len() > 4_000_000) { let _ = std::fs::rename(&p, p.with_extension("old")); }
    let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) else { return };
    let _ = JOURNAL_FILE.set(Mutex::new(f));
    let _ = bb::JOURNAL.set(Box::new(|v: &Value| {
        let cmd = v["cmd"].as_str().unwrap_or("");
        if changes_state(cmd) { journal_event(json!({"ev": "send", "cmd": cmd, "product": v["product"], "side": v["side"], "qty": v["qty"], "price": v["price"]})); }
    }));
}

pub fn journal_event(mut v: Value) {
    let Some(f) = JOURNAL_FILE.get() else { return };
    v["t"] = json!(now_secs());
    if let Ok(mut f) = f.lock() { let _ = writeln!(f, "{v}"); let _ = f.flush(); }
}

/// What the agent answered: this is what closes a "send" in the journal.
pub fn journal_reply(v: &Value) {
    let ty = v["type"].as_str().unwrap_or("");
    if matches!(ty, "placed" | "requoted" | "requoted_sell" | "rejoin" | "fill" | "claimed" | "flipped" | "error" | "orders" | "sacks" | "pickup" | "insert_sacks" | "sellinv") {
        journal_event(json!({"ev": "reply", "type": ty, "cmd": v["cmd"], "product": v["product"], "ok": ty != "error", "msg": v["msg"], "info": v["info"], "timing": v["timing"]}));
    }
}

/// Actions the previous run asked the agent for and never saw an answer to (the server stopped in the middle of them).
pub fn journal_unresolved() -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(journal_path()) else { return vec![] };
    journal_unresolved_from(&text)
}

pub fn journal_unresolved_from(text: &str) -> Vec<String> {
    let mut pending: Vec<(String, String)> = vec![];   // (cmd, description) in order
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        match v["ev"].as_str().unwrap_or("") {
            "boot" | "restart" => pending.clear(),   // everything before the last start was settled by that start
            "send" => pending.push((v["cmd"].as_str().unwrap_or("").to_string(), format!("{} {} {}", v["cmd"].as_str().unwrap_or(""), v["side"].as_str().unwrap_or(""), pretty_s(v["product"].as_str().unwrap_or(""))).trim().to_string())),
            "reply" => {
                let want: &[&str] = match v["type"].as_str().unwrap_or("") {
                    "placed" => &["buyorder", "selloffer"], "requoted" => &["requote"], "requoted_sell" => &["requote_sell"], "claimed" => &["claimorder"], "flipped" => &["fliporder"], "pickup" => &["pickup"], "insert_sacks" => &["insert_sacks"], "sellinv" => &["sellinv"],
                    "fill" => &["instabuy", "instasell", "tradesellqty", "tradesell"], "error" => &[], _ => &[],
                };
                if v["type"] == "error" { if let Some(c) = v["cmd"].as_str() { if let Some(i) = pending.iter().position(|p| p.0 == c) { pending.remove(i); } } }
                else if let Some(i) = pending.iter().position(|p| want.contains(&p.0.as_str())) { pending.remove(i); }
                // the balance that follows a claim / cancel closes those
                else if v["type"] == "orders" { pending.retain(|p| p.0 != "orders"); }
            }
            _ => {}
        }
    }
    pending.into_iter().map(|p| p.1).collect()
}

// ---------------------------------------------------------------- the server's socket

pub struct Shared { pub snap: Mutex<(u64, String)>, pub clients: AtomicUsize }

pub async fn server_task(secret: String, shared: Arc<Shared>, tx: mpsc::UnboundedSender<Msg>) {
    let listener = match TcpListener::bind(server_addr()).await { Ok(l) => l, Err(e) => { eprintln!("blockberg-server: cannot listen on {}: {e}", server_addr()); std::process::exit(2); } };
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let (secret, shared, tx) = (secret.clone(), shared.clone(), tx.clone());
        tokio::spawn(async move { serve_conn(stream, secret, shared, tx).await; });
    }
}

async fn serve_conn(stream: TcpStream, secret: String, shared: Arc<Shared>, tx: mpsc::UnboundedSender<Msg>) {
    let _ = stream.set_nodelay(true);
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    let first = match tokio::time::timeout(Duration::from_secs(3), lines.next_line()).await { Ok(Ok(Some(l))) => l, _ => return };
    let ok = serde_json::from_str::<Value>(&first).ok().map_or(false, |v| v["hello"] == "blockberg" && same_secret(v["secret"].as_str().unwrap_or(""), &secret));
    if !ok { let _ = wr.write_all(b"{\"t\":\"hello\",\"ok\":false,\"why\":\"wrong secret\"}\n").await; return; }
    if wr.write_all(b"{\"t\":\"hello\",\"ok\":true,\"proto\":1}\n").await.is_err() { return; }
    shared.clients.fetch_add(1, Ordering::Relaxed);
    let sh = shared.clone();
    let writer = tokio::spawn(async move {
        let mut last = 0u64;
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let (seq, text) = sh.snap.lock().map(|g| g.clone()).unwrap_or((0, String::new()));
            if seq != last && !text.is_empty() {
                last = seq;
                if wr.write_all(text.as_bytes()).await.is_err() || wr.write_all(b"\n").await.is_err() { break; }
            }
        }
    });
    while let Ok(Some(line)) = lines.next_line().await {
        if let Ok(v) = serde_json::from_str::<Value>(&line) { let _ = tx.send(Msg::Op(v)); }
    }
    writer.abort();
    shared.clients.fetch_sub(1, Ordering::Relaxed);
}

// ---------------------------------------------------------------- the client's socket

/// Bumped when the window switches account: the link drops and reconnects to the new account's server.
pub static LINK_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub async fn client_task(secret: String, tx: mpsc::UnboundedSender<Msg>, mut ops: mpsc::UnboundedReceiver<Value>, up: Arc<AtomicBool>) {
    loop {
        up.store(false, Ordering::Relaxed);
        let _ = tx.send(Msg::Link("connecting to the server…".into()));
        let Ok(stream) = TcpStream::connect(server_addr()).await else { tokio::time::sleep(Duration::from_secs(1)).await; continue };
        let _ = stream.set_nodelay(true);
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        if wr.write_all(format!("{}\n", json!({"hello": "blockberg", "secret": secret})).as_bytes()).await.is_err() { continue; }
        match tokio::time::timeout(Duration::from_secs(4), lines.next_line()).await {
            Ok(Ok(Some(l))) => {
                let v: Value = serde_json::from_str(&l).unwrap_or(Value::Null);
                if v["ok"] != true { let _ = tx.send(Msg::Link(format!("server refused the connection: {}", v["why"].as_str().unwrap_or("?")))); tokio::time::sleep(Duration::from_secs(5)).await; continue; }
            }
            _ => { tokio::time::sleep(Duration::from_secs(1)).await; continue; }
        }
        // whatever was queued while we were away is stale: never send it now
        while ops.try_recv().is_ok() {}
        up.store(true, Ordering::Relaxed);
        let _ = tx.send(Msg::Link("connected".into()));
        let epoch = LINK_EPOCH.load(Ordering::Relaxed);
        let mut watch = tokio::time::interval(Duration::from_millis(200));
        loop {
            tokio::select! {
                _ = watch.tick() => { if LINK_EPOCH.load(Ordering::Relaxed) != epoch { break; } }
                line = lines.next_line() => match line {
                    Ok(Some(l)) => { if let Ok(v) = serde_json::from_str::<Value>(&l) { let _ = tx.send(Msg::Snap(v)); } }
                    _ => break,
                },
                op = ops.recv() => match op {
                    Some(v) => { if wr.write_all(format!("{v}\n").as_bytes()).await.is_err() { break; } }
                    None => return,
                },
            }
        }
        up.store(false, Ordering::Relaxed);
        let _ = tx.send(Msg::Link("server connection lost: reconnecting…".into()));
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// ---------------------------------------------------------------- state on the wire

const TAGS: &[&str] = &["ICE", "DONE", "STOP", "PAUSE", "NEXT", "NOW", "WAIT", "REQ", "HOLD", "THEN", "SEND", "CLAIM", "FLOW", "FAIL", "POLL", "SENT", "RSTRT"];
fn tag(s: &str) -> &'static str { TAGS.iter().copied().find(|t| *t == s).unwrap_or("?") }
fn rgb(c: Color32) -> Value { json!([c.r(), c.g(), c.b()]) }
fn from_rgb(v: &Value) -> Color32 { Color32::from_rgb(v[0].as_u64().unwrap_or(200) as u8, v[1].as_u64().unwrap_or(200) as u8, v[2].as_u64().unwrap_or(200) as u8) }
fn ago(secs: f64) -> Instant { Instant::now().checked_sub(Duration::from_secs_f64(secs.max(0.0))).unwrap_or_else(Instant::now) }
fn age_opt(t: Option<Instant>) -> Value { t.map_or(Value::Null, |t| json!(t.elapsed().as_secs_f64())) }
fn kind_str(k: Kind) -> &'static str { match k { Kind::Instant => "instant", Kind::Limit => "limit", Kind::Npc => "npc" } }
fn kind_from(s: &str) -> Kind { match s { "limit" => Kind::Limit, "npc" => Kind::Npc, _ => Kind::Instant } }

impl Gui {
    pub fn is_client(&self) -> bool { self.mode == Mode::Client }
    pub fn is_server(&self) -> bool { self.mode == Mode::Server }
    /// May this process write the engine's files (jobs, sacks) and drive the agent?
    pub fn is_engine(&self) -> bool { self.mode != Mode::Client }

    /// Client: send a command to the server. `false` (and a notification) when the server is not reachable.
    pub fn remote_op(&mut self, v: Value) -> bool {
        let up = self.link_up.load(Ordering::Relaxed);
        if up { if let Some(tx) = &self.op_tx { return tx.send(v).is_ok(); } }
        let _ = self.toast_new(2, "Not sent: the app is not connected to the server");
        false
    }

    pub fn snapshot_json(&self) -> Value {
        let trades: Vec<Value> = self.trades.iter().map(|t| json!({
            "time": t.time, "side": t.side, "id": t.id, "qty": t.qty, "price": t.price, "kind": kind_str(t.kind),
            "status": match &t.status { Status::Working => "working", Status::Resting => "resting", Status::Filled => "filled", Status::Rejected(_) => "rejected" },
            "reason": if let Status::Rejected(m) = &t.status { m.as_str() } else { "" },
            "sig": {"chat": t.sig.chat, "orders": t.sig.orders, "inv": t.sig.inv, "sacks": t.sig.sacks, "purse": t.sig.purse},
        })).collect();
        let jobs: Vec<Value> = self.jobs.iter().map(|j| json!({
            "id": j.id, "product": j.product, "total": j.total, "slice": j.slice, "min": j.min_price, "max": j.max_price, "offset": j.offset, "requote": j.requote_secs,
            "filled": j.filled, "state": j.state.label(), "cur_trade": j.cur_trade, "cur_qty": j.cur_qty, "cur_price": j.cur_price, "cur_filled": j.cur_filled,
            "placed_age": j.placed_at.elapsed().as_secs_f64(), "state_age": j.state_at.elapsed().as_secs_f64(),
            "retry_in": j.retry_at.map(|t| t.saturating_duration_since(Instant::now()).as_secs_f64()),
            "stopping": j.stopping, "fails": j.fails, "requotes": j.requotes, "slices_done": j.slices_done, "run_secs": j.run_secs, "note": j.note, "auto_claim": j.auto_claim,
        })).collect();
        let queue: Vec<Value> = self.queue_rows().iter().map(|r| json!({"d": r.depth, "tag": r.tag, "text": r.text, "detail": r.detail, "c": rgb(r.color), "when": r.when, "est": r.est})).collect();
        let acts: Vec<Value> = self.cmd_tx.log.lock().map(|l| l.iter().map(|a| json!({
            "id": a.id, "auto": a.auto, "cmd": a.cmd, "label": a.label, "age": a.sent.elapsed().as_secs_f64(),
            "done_age": a.done.as_ref().map(|d| d.0.elapsed().as_secs_f64()), "err": a.done.as_ref().and_then(|d| d.1.clone()),
        })).collect()).unwrap_or_default();
        let toasts: Vec<Value> = self.toasts.iter().map(|t| json!({"id": t.id, "text": t.text, "kind": t.kind, "age": self.ui_time - t.born, "done_age": t.done.map(|d| self.ui_time - d)})).collect();
        let log: Vec<Value> = self.log.iter().rev().take(150).rev().map(|(c, s)| json!([rgb(*c), s])).collect();
        let chat: Vec<Value> = self.chat.iter().rev().take(100).rev().map(|l| json!({"ts": l.ts, "segs": l.segs.iter().map(|(c, s)| json!([rgb(*c), s])).collect::<Vec<_>>()})).collect();
        let orders: Vec<Value> = self.orders.iter().map(|o| json!({"side": o.side, "product": o.product, "amount": o.amount, "price": o.price, "filled": o.filled, "total": o.total, "claimable": o.claimable, "to_claim": o.to_claim})).collect();
        let inv_all: Vec<Value> = self.inv_all.iter().map(|i| json!({"id": i.id, "count": i.count, "fits": i.fits})).collect();
        let sacks: Vec<Value> = self.sacks.iter().map(|s| json!({"name": s.name, "copies": s.copies, "cap_each": s.cap_each, "items": s.items})).collect();
        json!({
            "t": "state", "proto": 1, "agent_ctl": true,
            "agent_up": self.agent_up, "halted": self.halted, "purse": self.purse, "bank": self.bank, "auto_claim": self.auto_claim, "auto_coins": self.auto_coins, "ice_auto_remove": self.ice_auto_remove, "auto_stash": self.auto_stash, "auto_lost_found": self.auto_lf, "auto_sack": self.auto_sack, "auto_sleep": self.auto_sleep, "quick_book": self.quick_book, "qb": self.qb_status(),
            "spent_today": self.spent_today, "npc_sold": self.npc_sold_today(), "restart": if self.restart_req { json!({"waiting": self.restart_waiting()}) } else { Value::Null },
            "uptime": self.boot.elapsed().as_secs_f64(), "clients": self.srv.as_ref().map_or(0, |s| s.clients.load(Ordering::Relaxed)),
            "orders": orders, "orders_age": age_opt(self.orders_at), "inventory": self.inventory.iter().map(|(k, v)| (k.clone(), json!([v.0, v.1]))).collect::<serde_json::Map<_, _>>(),
            "inv_all": inv_all, "inv_slots": self.inv_slots.iter().map(|s| json!({"slot": s.slot, "id": s.id, "count": s.count})).collect::<Vec<_>>(), "inv_free": self.inv_free, "inv_all_age": age_opt(self.inv_all_at),
            "sack_api": bb::SACK_API.lock().ok().and_then(|g| g.as_ref().map(|(_, m)| json!(m))).unwrap_or(Value::Null),
            "sacks": sacks, "sacks_at": self.sacks_at, "sacks_scanning": self.sacks_scanning,
            "holdings": self.holdings.iter().map(|(k, v)| (k.clone(), json!([v.0, v.1]))).collect::<serde_json::Map<_, _>>(),
            "positions": self.positions.iter().map(|(k, v)| (k.clone(), json!([v.0, v.1]))).collect::<serde_json::Map<_, _>>(),
            "trades": trades, "jobs": jobs, "flips": self.flips.iter().map(|j| j.to_json()).collect::<Vec<_>>(), "flip_log": self.flip_log.iter().rev().take(100).rev().map(|e| e.to_json()).collect::<Vec<_>>(), "flip_cycles": self.flip_cycles.iter().map(|c| c.to_json()).collect::<Vec<_>>(), "next_job_id": self.next_job_id, "queue": queue, "acts": acts, "toasts": toasts, "log": log, "chat": chat,
            "wf": {"running": self.wf_running, "steps": self.wf_steps, "result": self.wf_result},
            "inflight": self.inflight.iter().collect::<Vec<_>>(),
        })
    }

    /// Client: take the server's state as our own.
    pub fn apply_snapshot(&mut self, v: &Value) {
        if v["t"] != "state" { return; }
        self.agent_up = v["agent_up"].as_bool().unwrap_or(false);
        self.halted = v["halted"].as_bool().unwrap_or(false);
        self.purse = v["purse"].as_f64();
        if let Some(b) = v["bank"].as_f64() { self.bank = Some(b); }
        self.auto_claim = v["auto_claim"].as_bool().unwrap_or(true);
        self.ice_auto_remove = v["ice_auto_remove"].as_bool().unwrap_or(true);
        // an older server does not know this setting: then the switch is shown off, and turning it on explains why it cannot work
        self.server_has_sack = v["auto_sack"].is_boolean();
        if self.server_has_sack { self.auto_sack = v["auto_sack"].as_bool().unwrap_or(false); }
        self.server_has_qb = v["quick_book"].is_boolean();
        quickbook::from_snapshot(&self.qb, &v["qb"]);
        quickbook::overlay(&self.qb, &mut self.products);
        if self.server_has_qb { self.quick_book = v["quick_book"].as_bool().unwrap_or(false); }
        self.server_has_sleep = v["auto_sleep"].is_boolean();
        if self.server_has_sleep { self.auto_sleep = v["auto_sleep"].as_bool().unwrap_or(false); }
        self.server_has_agent_ctl = v["agent_ctl"].as_bool().unwrap_or(false);
        self.server_has_coins = v["auto_coins"].is_boolean();
        if self.server_has_coins { self.auto_coins = v["auto_coins"].as_bool().unwrap_or(true); }
        self.server_has_pickup = v["auto_stash"].is_boolean();
        if self.server_has_pickup { self.auto_stash = v["auto_stash"].as_bool().unwrap_or(false); self.auto_lf = v["auto_lost_found"].as_bool().unwrap_or(false); } else { self.auto_stash = false; self.auto_lf = false; }
        self.spent_today = v["spent_today"].as_f64().unwrap_or(0.0);
        if let Some(n) = v["npc_sold"].as_f64() { self.npc_sold = n; self.npc_sold_day = Self::utc_day(); }
        self.server_restart = v["restart"].is_object().then(|| v["restart"]["waiting"].as_u64().unwrap_or(0));
        self.server_uptime = v["uptime"].as_f64().unwrap_or(0.0);
        self.orders = v["orders"].as_array().map(|a| a.iter().map(|o| OrderRow { side: o["side"].as_str().unwrap_or("").into(), product: o["product"].as_str().unwrap_or("").into(), amount: o["amount"].as_u64(), price: o["price"].as_f64(),
            filled: o["filled"].as_u64().unwrap_or(0), total: o["total"].as_u64().unwrap_or(0), claimable: o["claimable"].as_bool().unwrap_or(false), to_claim: o["to_claim"].as_u64().unwrap_or(0) }).collect()).unwrap_or_default();
        self.orders_at = v["orders_age"].as_f64().map(ago);
        self.inventory = v["inventory"].as_object().map(|m| m.iter().map(|(k, x)| (k.clone(), (x[0].as_u64().unwrap_or(0), x[1].as_u64().unwrap_or(0)))).collect()).unwrap_or_default();
        self.inv_slots = v["inv_slots"].as_array().map(|a| a.iter().filter_map(|x| Some(crate::iceberg::InvSlot { slot: x["slot"].as_u64()? as u8, id: x["id"].as_str()?.to_string(), count: x["count"].as_u64().unwrap_or(1) })).collect()).unwrap_or_default();
        self.inv_all = v["inv_all"].as_array().map(|a| a.iter().map(|i| InvItem { id: i["id"].as_str().unwrap_or("").into(), count: i["count"].as_u64().unwrap_or(0), fits: i["fits"].as_u64().unwrap_or(0) }).collect()).unwrap_or_default();
        self.inv_free = v["inv_free"].as_u64();
        self.inv_all_at = v["inv_all_age"].as_f64().map(ago);
        self.sacks = v["sacks"].as_array().map(|a| a.iter().map(|s| SackInfo { name: s["name"].as_str().unwrap_or("").into(), copies: s["copies"].as_u64().unwrap_or(1), cap_each: s["cap_each"].as_u64().unwrap_or(0),
            items: s["items"].as_array().map(|it| it.iter().map(|p| (p[0].as_str().unwrap_or("").to_string(), p[1].as_u64().unwrap_or(0))).collect()).unwrap_or_default() }).collect()).unwrap_or_default();
        if let Some(m) = v["sack_api"].as_object() {
            let map = m.iter().filter_map(|(k, x)| x.as_u64().map(|n| (k.clone(), n))).collect();
            if let Ok(mut g) = bb::SACK_API.lock() { *g = Some((Instant::now(), map)); }
        }
        self.sacks_at = v["sacks_at"].as_f64();
        self.sacks_scanning = v["sacks_scanning"].as_bool().unwrap_or(false);
        self.holdings = v["holdings"].as_object().map(|m| m.iter().map(|(k, x)| (k.clone(), (x[0].as_u64().unwrap_or(0), x[1].as_u64()))).collect()).unwrap_or_default();
        self.positions = v["positions"].as_object().map(|m| m.iter().map(|(k, x)| (k.clone(), (x[0].as_i64().unwrap_or(0), x[1].as_f64().unwrap_or(0.0)))).collect()).unwrap_or_default();
        self.trades = v["trades"].as_array().map(|a| a.iter().map(|t| Trade {
            time: t["time"].as_str().unwrap_or("").into(), side: if t["side"] == "SELL" { "SELL" } else { "BUY" }, id: t["id"].as_str().unwrap_or("").into(), qty: t["qty"].as_u64().unwrap_or(0),
            price: t["price"].as_f64().unwrap_or(0.0), kind: kind_from(t["kind"].as_str().unwrap_or("")),
            status: match t["status"].as_str().unwrap_or("") { "resting" => Status::Resting, "filled" => Status::Filled, "rejected" => Status::Rejected(t["reason"].as_str().unwrap_or("").into()), _ => Status::Working },
            sig: Sig { chat: t["sig"]["chat"].as_bool().unwrap_or(false), orders: t["sig"]["orders"].as_bool().unwrap_or(false), inv: t["sig"]["inv"].as_bool(), sacks: t["sig"]["sacks"].as_bool(), purse: t["sig"]["purse"].as_bool() },
        }).collect()).unwrap_or_default();
        self.inflight = v["inflight"].as_array().map(|a| a.iter().filter_map(|x| x.as_u64().map(|n| n as usize)).collect()).unwrap_or_default();
        self.jobs = v["jobs"].as_array().map(|a| a.iter().map(|j| {
            let mut job = IceJob::new(j["id"].as_u64().unwrap_or(0) as u32, j["product"].as_str().unwrap_or("").into(), j["total"].as_u64().unwrap_or(0), j["slice"].as_u64().unwrap_or(0),
                j["min"].as_f64().unwrap_or(0.0), j["max"].as_f64().unwrap_or(0.0), j["offset"].as_f64().unwrap_or(0.0), j["requote"].as_u64().unwrap_or(300));
            job.filled = j["filled"].as_u64().unwrap_or(0); job.state = iceberg::JobState::from_label(j["state"].as_str().unwrap_or(""));
            job.cur_trade = j["cur_trade"].as_u64().map(|n| n as usize); job.cur_qty = j["cur_qty"].as_u64().unwrap_or(0); job.cur_price = j["cur_price"].as_f64().unwrap_or(0.0); job.cur_filled = j["cur_filled"].as_u64().unwrap_or(0);
            job.placed_at = ago(j["placed_age"].as_f64().unwrap_or(0.0)); job.state_at = ago(j["state_age"].as_f64().unwrap_or(0.0));
            job.retry_at = j["retry_in"].as_f64().map(|s| Instant::now() + Duration::from_secs_f64(s));
            job.stopping = j["stopping"].as_bool().unwrap_or(false); job.fails = j["fails"].as_u64().unwrap_or(0) as u32; job.requotes = j["requotes"].as_u64().unwrap_or(0) as u32;
            job.slices_done = j["slices_done"].as_u64().unwrap_or(0) as u32; job.run_secs = j["run_secs"].as_f64().unwrap_or(0.0); job.note = j["note"].as_str().unwrap_or("").into(); job.auto_claim = j["auto_claim"].as_bool().unwrap_or(false);
            job
        }).collect()).unwrap_or_default();
        self.flip_cycles = v["flip_cycles"].as_array().map(|a| a.iter().map(crate::autoflip::FlipCycle::from_json).collect()).unwrap_or_default();
        self.flip_log = v["flip_log"].as_array().map(|a| a.iter().map(crate::autoflip::FlipEvent::from_json).collect()).unwrap_or_default();
        self.flips = v["flips"].as_array().map(|a| a.iter().map(crate::autoflip::FlipJob::from_json).collect()).unwrap_or_default();
        self.next_job_id = v["next_job_id"].as_u64().unwrap_or(1) as u32;
        self.remote_queue = v["queue"].as_array().map(|a| a.iter().map(|r| iceberg::QRow { depth: r["d"].as_u64().unwrap_or(0) as u8, tag: tag(r["tag"].as_str().unwrap_or("")), text: r["text"].as_str().unwrap_or("").into(),
            detail: r["detail"].as_str().unwrap_or("").into(), color: from_rgb(&r["c"]), when: r["when"].as_f64(), est: r["est"].as_f64() }).collect()).unwrap_or_default();
        if let Ok(mut log) = self.cmd_tx.log.lock() {
            *log = v["acts"].as_array().map(|a| a.iter().map(|x| ActRec { id: x["id"].as_u64().unwrap_or(0), auto: x["auto"].as_bool().unwrap_or(false), cmd: x["cmd"].as_str().unwrap_or("").into(), label: x["label"].as_str().unwrap_or("").into(),
                sent: ago(x["age"].as_f64().unwrap_or(0.0)), done: x["done_age"].as_f64().map(|d| (ago(d), x["err"].as_str().map(|s| s.to_string()))) }).collect()).unwrap_or_default();
        }
        // the server's notifications, plus any this app made itself (their ids start far above the server's)
        let now = self.ui_time;
        let mut toasts: Vec<fx::Toast> = self.toasts.iter().filter(|t| t.id >= (1 << 40)).cloned().collect();
        toasts.extend(v["toasts"].as_array().map(|a| a.iter().map(|t| fx::Toast { id: t["id"].as_u64().unwrap_or(0), text: t["text"].as_str().unwrap_or("").into(), kind: t["kind"].as_u64().unwrap_or(0) as u8,
            born: now - t["age"].as_f64().unwrap_or(0.0), done: t["done_age"].as_f64().map(|d| now - d) }).collect::<Vec<_>>()).unwrap_or_default());
        self.toasts = toasts;
        self.log = v["log"].as_array().map(|a| a.iter().map(|l| (from_rgb(&l[0]), l[1].as_str().unwrap_or("").to_string())).collect()).unwrap_or_default();
        self.chat = v["chat"].as_array().map(|a| a.iter().map(|l| ChatLine { ts: l["ts"].as_str().unwrap_or("").into(), segs: l["segs"].as_array().map(|s| s.iter().map(|p| (from_rgb(&p[0]), p[1].as_str().unwrap_or("").to_string())).collect()).unwrap_or_default() }).collect()).unwrap_or_default();
        self.wf_running = v["wf"]["running"].as_bool().unwrap_or(false);
        self.wf_steps = v["wf"]["steps"].as_array().map(|a| a.iter().map(|s| (s[0].as_str().unwrap_or("").to_string(), s[1].as_bool(), s[2].as_str().unwrap_or("").to_string())).collect()).unwrap_or_default();
        self.wf_result = v["wf"]["result"].as_str().map(|s| s.to_string());
        self.snap_at = Some(Instant::now());
    }

    // ---------------------------------------------------------------- server side

    /// Publish the state to connected apps (at most ~4 times a second, only when something changed).
    pub fn publish_snapshot(&mut self) {
        let Some(shared) = self.srv.clone() else { return };
        if self.snap_pub.elapsed() < Duration::from_millis(250) { return; }
        self.snap_pub = Instant::now();
        if shared.clients.load(Ordering::Relaxed) == 0 { return; }
        let mut v = self.snapshot_json();
        // the figures that tick every moment (countdowns, ages) must not make every snapshot look different
        let text = { let mut c = v.clone(); for k in ["queue", "uptime", "jobs", "flips", "flip_log", "flip_cycles", "acts", "toasts", "orders_age", "inv_all_age"] { c[k] = Value::Null; } c.to_string() };
        let quiet = text == self.snap_last && self.snap_forced.elapsed() < Duration::from_secs(2);
        if quiet { return; }
        self.snap_last = text; self.snap_forced = Instant::now();
        v["seq"] = json!(self.snap_seq + 1);
        self.snap_seq += 1;
        let text = v.to_string();
        let seq = self.snap_seq;
        let _ = shared.snap.lock().map(|mut s| { *s = (seq, text); });
    }

    /// A command from a connected app.
    pub fn handle_op(&mut self, v: Value) {
        let op = v["op"].as_str().unwrap_or("");
        if self.draining && !matches!(op, "restart" | "screen") && !(op == "agent" && v["msg"]["cmd"] == "reload") {
            let _ = self.toast_new(2, "The server is restarting: that was not started. Try again in a moment.");
            return;
        }
        match op {
            "agent" => {
                let m = v["msg"].clone();
                // a read that is already waiting for the agent is not queued again (they would only pile up behind a long job)
                if matches!(m["cmd"].as_str(), Some("invall" | "balance")) {
                    let c = m["cmd"].as_str().unwrap_or("");
                    if self.cmd_tx.log.lock().map_or(false, |l| l.iter().any(|a| a.done.is_none() && a.cmd == c)) { return; }
                }
                if m["cmd"] == "sackscan" { self.sacks_scanning = true; }
                if m["cmd"] == "cancel" && m["orders"].is_array() {
                    for o in m["orders"].as_array().unwrap() {
                        let (side, prod, price) = (o["side"].as_str().unwrap_or(""), o["product"].as_str().unwrap_or(""), o["price"].as_f64());
                        self.orders.retain(|x| !(x.side == side && x.product == prod && x.price == price));
                    }
                } else if m["cmd"] == "cancel" {   // the order list shows it gone at once; the real re-read follows when the cancels have finished
                    let (side, prod, price) = (m["side"].as_str().unwrap_or(""), m["product"].as_str().unwrap_or(""), m["price"].as_f64());
                    self.orders.retain(|o| !(o.side == side && o.product == prod && o.price == price));
                }
                let _ = self.cmd_tx.send(m);
            }
            "order" => {
                let cmd: &'static str = match v["cmd"].as_str().unwrap_or("") { "instabuy" => "instabuy", "instasell" => "instasell", "buyorder" => "buyorder", "selloffer" => "selloffer", _ => return };
                self.source_sacks = v["from_sacks"].as_bool().unwrap_or(false);
                self.dispatch(Order { cmd, id: v["id"].as_str().unwrap_or("").into(), qty: v["qty"].as_u64().unwrap_or(0), unit: v["unit"].as_f64().unwrap_or(0.0), kind: kind_from(v["kind"].as_str().unwrap_or("")) });
            }
            "npc_sell" => { self.npc_sell_now(v["id"].as_str().unwrap_or(""), v["mode"].as_u64().unwrap_or(0) as u8, v["qty"].as_u64().unwrap_or(1), v["unit"].as_f64().unwrap_or(0.0), v["from_sacks"].as_bool().unwrap_or(false)); }
            "ice_add" => {
                let mut j = IceJob::new(0, v["product"].as_str().unwrap_or("").into(), v["total"].as_u64().unwrap_or(0), v["slice"].as_u64().unwrap_or(0), v["min"].as_f64().unwrap_or(0.0),
                    v["max"].as_f64().unwrap_or(0.0), v["offset"].as_f64().unwrap_or(0.1), v["requote"].as_u64().unwrap_or(300));
                j.auto_claim = v["auto_claim"].as_bool().unwrap_or(false);
                j.on_outbid = v["on_outbid"].as_bool().unwrap_or(false); j.outbid_gap = v["outbid_gap"].as_u64().unwrap_or(30);
                if j.total > 0 && j.slice > 0 && j.max_price > 0.0 { self.ice_add(j); }
            }
            "ice_edit" => {
                let e = crate::iceberg::IceEdit { id: v["id"].as_u64().unwrap_or(0) as u32, total: v["total"].as_u64().unwrap_or(0), slice: v["slice"].as_u64().unwrap_or(0), min: v["min"].as_f64().unwrap_or(0.0), max: v["max"].as_f64().unwrap_or(0.0),
                    offset: v["offset"].as_f64().unwrap_or(0.1), requote_secs: v["requote"].as_u64().unwrap_or(300), auto_claim: v["auto_claim"].as_bool().unwrap_or(false), on_outbid: v["on_outbid"].as_bool().unwrap_or(false),
                    outbid_gap: v["outbid_gap"].as_u64().unwrap_or(30), requote_now: v["now"].as_bool().unwrap_or(false) };
                if let Err(m) = self.ice_edit(&e) { self.say(RED, format!("iceberg edit refused: {m}")); }
            }
            "flip_add" => { let j = crate::autoflip::FlipJob::from_json(&v); if j.total > 0 && (j.max_price > 0.0 || j.sell_only) && !j.product.is_empty() { self.flip_add(j); } }
            "flip" => { let id = v["id"].as_u64().unwrap_or(0) as u32; let what = v["do"].as_str().unwrap_or("").to_string(); self.flip_op(id, &what); }
            "ice" => {
                let id = v["id"].as_u64().unwrap_or(0) as u32;
                if let Some(i) = self.jobs.iter().position(|j| j.id == id) {
                    match v["do"].as_str().unwrap_or("") { "pause" => self.ice_pause(i), "resume" => self.ice_resume(i), "stop" => self.ice_stop(i), "remove" => self.ice_remove(i), _ => {} }
                }
            }
            "halt" => { self.halted = v["v"].as_bool().unwrap_or(true); self.say(if self.halted { RED } else { GREEN }, if self.halted { "TRADING HALTED" } else { "trading resumed" }); }
            "set" => match v["k"].as_str().unwrap_or("") {
                "auto_claim" => self.auto_claim = v["v"].as_bool().unwrap_or(true),
                "ice_auto_remove" => self.ice_auto_remove = v["v"].as_bool().unwrap_or(true),
                "auto_stash" => { self.auto_stash = v["v"].as_bool().unwrap_or(false); self.stash_next = Instant::now() + Duration::from_secs(20 * 60); self.save_layout(); }
                "quick_book" => { self.quick_book = v["v"].as_bool().unwrap_or(false); self.save_layout(); }
                "auto_sleep" => { self.auto_sleep = v["v"].as_bool().unwrap_or(false); self.last_activity = Instant::now(); self.save_layout(); }
                "auto_coins" => { self.auto_coins = v["v"].as_bool().unwrap_or(true); self.coins_next = Instant::now() + Duration::from_secs(10 * 60); self.save_layout(); }
                "auto_sack" => { self.auto_sack = v["v"].as_bool().unwrap_or(false); self.save_layout(); }
                "auto_lost_found" => { self.auto_lf = v["v"].as_bool().unwrap_or(false); self.lf_next = Instant::now() + Duration::from_secs(20 * 60); self.save_layout(); }
                _ => {}
            },
            "qb_focus" => { self.qb_focus = v["v"].as_str().map(|s| (s.to_string(), Instant::now())); }
            "clear_blotter" => self.clear_blotter(),
            "screen" => {
                let s = match v["v"].as_str().unwrap_or("") { "inventory" => Screen::Inventory, "queue" => Screen::Queue, "portfolio" => Screen::Portfolio, _ => Screen::Main };
                if s != self.screen { self.screen = Screen::Main; self.open_screen(s); if s == Screen::Main { self.screen = Screen::Main; } }
            }
            "wf_run" => { self.wf_base = v["base"].as_str().unwrap_or("SEEDS").into(); self.wf_ench = v["ench"].as_str().unwrap_or("ENCHANTED_SEEDS").into(); self.wf_units = v["units"].as_u64().unwrap_or(160); self.run_workflow(); }
            "restart" => self.request_restart(),
            "activity" => self.note_activity(),
            "agent_service" => self.agent_service_local(v["do"].as_str().unwrap_or("")),
            _ => {}
        }
    }

    /// Starts or stops the desktop Minecraft that carries the agent (the `bblauncher-agent` user service). Never touches it in mock mode.
    pub fn agent_service_local(&mut self, action: &str) {
        if !matches!(action, "start" | "stop") { return; }
        if cfg!(test) { return; }   // tests must never touch the real agent service
        if self.mock { let _ = self.toast_new(2, "Mock mode: the agent service is not touched"); return; }
        let unit = if crate::bb::profile::is_main() { "bblauncher-agent".to_string() } else { let e = crate::bb::profile::entry(); format!("bblauncher-agent@{}-{}", e.name, e.agent.rsplit(':').next().unwrap_or("")) };
        let exists = std::process::Command::new("systemctl").args(["--user", "cat", &unit]).output().map_or(false, |o| o.status.success());
        if !exists { let _ = self.toast_new(2, "There is no bblauncher-agent service on this machine (the agent runs somewhere else)"); return; }
        let (a, msg) = if action == "stop" { ("stop", "Disconnecting the agent: Minecraft is being stopped…") } else { ("start", "Connecting the agent: Minecraft is starting and will join SkyBlock (about a minute)…") };
        self.say(ORANGE, msg);
        let _ = self.toast_new(0, msg);
        let a = a.to_string();
        let unit = unit.clone();
        std::thread::spawn(move || { let _ = std::process::Command::new("systemctl").args(["--user", &a, &unit]).status(); });
    }

    pub fn request_restart(&mut self) {
        if !self.is_server() || self.restart_req { return; }
        self.restart_req = true; self.draining = true;
        self.say(ORANGE, "restart queued: it runs as soon as what is running now has finished; nothing new starts until then");
        let _ = self.toast_new(0, "Server restart queued: waiting for the current work to finish");
    }

    /// How many things are still running that the restart has to wait for.
    pub fn restart_waiting(&self) -> usize {
        let acts = self.cmd_tx.log.lock().map_or(0, |l| l.iter().filter(|a| a.done.is_none()).count());
        self.inflight.len() + self.claiming.len() + acts + self.jobs.iter().filter(|j| matches!(j.state, iceberg::JobState::Placing | iceberg::JobState::Requoting | iceberg::JobState::Adopting)).count()
    }

    /// Once everything that was running has finished, save and restart (the service manager starts us again).
    pub fn restart_check(&mut self) {
        if !self.restart_req || self.restart_waiting() > 0 { return; }
        self.ice_save();
        journal_event(json!({"ev": "restart", "why": "requested"}));
        self.say(GREEN, "restarting the server now");
        std::thread::sleep(Duration::from_millis(400));
        std::process::exit(0);
    }
}

#[cfg(test)]
mod journal_tests {
    use super::*;
    #[test]
    fn journal_ignores_state_events() {
        let text = [r#"{"ev":"send","cmd":"claimorder","product":"COAL","side":"BUY","t":1}"#,
            r#"{"ev":"ice","id":1,"product":"COAL","state":"Paused","note":"x","t":2}"#,
            r#"{"ev":"agent","up":false,"was_up":true,"t":3}"#, r#"{"ev":"dropped","count":1,"t":4}"#].join("\n");
        let open = journal_unresolved_from(&text);
        assert_eq!(open.len(), 1, "only the unanswered claim is open; ice/agent/dropped lines are ignored");
        assert!(open[0].starts_with("claimorder"));
    }
}
