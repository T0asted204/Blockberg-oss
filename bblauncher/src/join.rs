//! "Get to SkyBlock" once the agent is up. Tries `/play skyblock` straight away; only when Hypixel says it cannot be used
//! from where we landed ("Use /lobby first!") does it go to the lobby and try again. Each server hop kills the agent for a few
//! seconds (Minescript restarts it on the next world), so the extra hop is avoided whenever it is not needed.
use std::{io::{BufRead, BufReader, Read, Write}, net::TcpStream, sync::{atomic::{AtomicBool, AtomicUsize, Ordering}, Arc}, thread, time::{Duration, Instant}};

#[derive(Default)]
pub struct Watch { pub need_lobby: AtomicBool, pub hops: AtomicUsize }

/// Copies the game's output through, noting the lines that matter.
pub fn pump<R: Read + Send + 'static>(out: R, w: Arc<Watch>, pid: u32) {
    thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if line.contains("Use /lobby first") || line.contains("cannot join SkyBlock") { w.need_lobby.store(true, Ordering::Relaxed); }
            if line.contains("minescript-world-listener") && line.contains("Exited world") { w.hops.fetch_add(1, Ordering::Relaxed); }
            // the connection to Hypixel is gone and the game sits on the title screen: stop it so the supervisor starts a fresh one at once
            if line.contains("Client disconnected with reason") {
                eprintln!("bblauncher: lost the connection to the server, stopping the client so it can be restarted");
                thread::spawn(move || { thread::sleep(Duration::from_secs(2)); let _ = std::process::Command::new("kill").arg(pid.to_string()).status(); });
            }
            println!("{line}");
        }
    });
}

fn say(port: u16, text: &str) -> bool {
    let Ok(mut c) = TcpStream::connect(("127.0.0.1", port)) else { return false };
    let line = serde_json::json!({"cmd": "say", "text": text}).to_string() + "\n";
    let ok = c.write_all(line.as_bytes()).is_ok();
    let _ = c.set_read_timeout(Some(Duration::from_secs(3)));
    let mut buf = [0u8; 256]; let _ = c.read(&mut buf);   // wait for the "sent" reply so the line is really out before we close
    eprintln!("bblauncher: sent '{text}' through the agent");
    ok
}

fn wait_port(port: u16, secs: u64) -> bool {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(secs) {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() { return true; }
        thread::sleep(Duration::from_secs(2));
    }
    false
}

pub fn skyblock(port: u16, w: Arc<Watch>) {
    thread::spawn(move || {
        if !wait_port(port, 1200) { return; }
        thread::sleep(Duration::from_secs(6));   // let the first world finish loading
        say(port, "/play skyblock");
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(15) && !w.need_lobby.load(Ordering::Relaxed) { thread::sleep(Duration::from_millis(500)); }
        if w.need_lobby.swap(false, Ordering::Relaxed) {
            eprintln!("bblauncher: Hypixel wants the lobby first");
            let hops = w.hops.load(Ordering::Relaxed);
            say(port, "/lobby");
            // a real server hop restarts the agent: wait for it; if we were already in a lobby nothing moves
            let t = Instant::now();
            while t.elapsed() < Duration::from_secs(10) && w.hops.load(Ordering::Relaxed) == hops { thread::sleep(Duration::from_millis(500)); }
            if w.hops.load(Ordering::Relaxed) > hops { thread::sleep(Duration::from_secs(3)); wait_port(port, 90); }
            thread::sleep(Duration::from_secs(6));
            say(port, "/play skyblock");
        }
    });
}
