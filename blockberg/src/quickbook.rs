//! "Quicker orderbook updates": the spare account (the first non-main profile) reads live order books in game for the
//! items the main account is trading, so requotes and relists react within seconds instead of waiting for Hypixel's
//! ~20 s API refresh. Turned on in Settings. The spare agent is started when this is on and the main agent is up, and
//! stopped when it is turned off or when the main agent has been gone for a minute (so a short Wi-Fi drop leaves it be).
use super::*;
use std::{collections::HashMap, io::{BufRead, BufReader, Write}, net::TcpStream, sync::{Arc, Mutex}};

/// A book older than this is not used (the API's own copy is newer or as good by then).
pub const BOOK_FRESH: f64 = 20.0;
const MAIN_GRACE: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct QbShared {
    pub on: bool,
    pub want: Vec<String>,
    /// one item to read as fast as possible (a seconds chart is open on it)
    pub focus: Option<String>,
    pub addr: String,
    pub connected: bool,
    /// product -> (unix secs read, bids, asks), each level (price, amount, orders)
    pub books: HashMap<String, (f64, Vec<(f64, u64, u64)>, Vec<(f64, u64, u64)>)>,
    pub last_err: String,
    pub err_said: String,
}
pub type Qb = Arc<Mutex<QbShared>>;

/// The spare account's profile entry and its agent service unit, if there is one.
pub fn spare() -> Option<(bb::profile::Entry, String)> {
    let e = bb::profile::all().into_iter().find(|e| e.name != "main")?;
    let unit = format!("bblauncher-agent@{}-{}", e.name, e.agent.rsplit(':').next().unwrap_or(""));
    Some((e, unit))
}

fn flag_path() -> std::path::PathBuf { bb::profile::shared_data_dir().join("quick_book_active") }
/// True while the main server is using the spare agent for books (the spare's own server must then not put it to sleep).
pub fn spare_in_use() -> bool {
    std::fs::metadata(flag_path()).and_then(|m| m.modified()).ok()
        .and_then(|t| t.elapsed().ok()).map_or(false, |a| a < Duration::from_secs(120))
}

/// Background thread: keeps a connection to the spare agent while `on`, tells it what to watch, collects the books.
pub fn spawn(qb: Qb) {
    if cfg!(test) { return; }
    std::thread::spawn(move || loop {
        let (on, addr) = { let g = qb.lock().unwrap(); (g.on, g.addr.clone()) };
        if !on || addr.is_empty() { std::thread::sleep(Duration::from_secs(1)); continue; }
        let Ok(stream) = TcpStream::connect(&addr) else { std::thread::sleep(Duration::from_secs(3)); continue };
        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
        let mut wr = match stream.try_clone() { Ok(w) => w, Err(_) => continue };
        let mut rd = BufReader::new(stream);
        qb.lock().unwrap().connected = true;
        let mut sent: Option<(Vec<String>, Option<String>)> = None;
        loop {
            let (on, want, focus) = { let g = qb.lock().unwrap(); (g.on, g.want.clone(), g.focus.clone()) };
            let (want, focus) = if on { (want, focus) } else { (vec![], None) };
            let now_w = (want.clone(), focus.clone());
            if sent.as_ref() != Some(&now_w) {
                if wr.write_all(format!("{}\n", json!({"cmd": "bookwatch", "products": want, "focus": focus})).as_bytes()).is_err() { break; }
                sent = Some(now_w);
            }
            if !on { break; }
            let mut line = String::new();
            match rd.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                    let lv = |x: &Value| -> Vec<(f64, u64, u64)> { x.as_array().into_iter().flatten().filter_map(|l| Some((l[0].as_f64()?, l[1].as_f64()? as u64, l[2].as_f64()? as u64))).collect() };
                    match v["type"].as_str() {
                        Some("book") => if let Some(p) = v["product"].as_str() { qb.lock().unwrap().books.insert(p.to_string(), (v["t"].as_f64().unwrap_or(0.0), lv(&v["bids"]), lv(&v["asks"]))); },
                        Some("book_err") => qb.lock().unwrap().last_err = v["msg"].as_str().unwrap_or("").to_string(),
                        _ => {}
                    }
                }
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(_) => break,
            }
        }
        qb.lock().unwrap().connected = false;
    });
}

/// Put fresh live books over the API's copy of those products.
pub fn overlay(qb: &Qb, products: &mut HashMap<String, Product>) {
    let now = now_secs();
    let g = qb.lock().unwrap();
    for (id, (t, bids, asks)) in g.books.iter() {
        if now - t > BOOK_FRESH { continue; }
        let Some(p) = products.get_mut(id) else { continue };
        let to = |v: &Vec<(f64, u64, u64)>| v.iter().map(|&(price, amount, orders)| Level { price, amount, orders }).collect::<Vec<_>>();
        if !bids.is_empty() { p.sell_summary = to(bids); }
        if !asks.is_empty() { p.buy_summary = to(asks); }
    }
}

/// Client side: take the server's live books from its state snapshot.
pub fn from_snapshot(qb: &Qb, v: &Value) {
    let Some(m) = v["books"].as_object() else { return };
    let lv = |x: &Value| -> Vec<(f64, u64, u64)> { x.as_array().into_iter().flatten().filter_map(|l| Some((l[0].as_f64()?, l[1].as_f64()? as u64, l[2].as_f64()? as u64))).collect() };
    let mut g = qb.lock().unwrap();
    for (k, b) in m { g.books.insert(k.clone(), (b[0].as_f64().unwrap_or(0.0), lv(&b[1]), lv(&b[2]))); }
}

impl Gui {
    /// Items worth watching live: what the auto flips and icebergs are trading, then the item on screen.
    fn qb_want(&self) -> Vec<String> {
        let mut v: Vec<String> = vec![];
        for f in self.flips.iter().filter(|f| f.state == autoflip::FlipState::Running) { if !v.contains(&f.product) { v.push(f.product.clone()); } }
        for j in self.jobs.iter().filter(|j| j.state.live()) { if !v.contains(&j.product) { v.push(j.product.clone()); } }
        if let Some(s) = &self.selected { if !v.contains(s) { v.push(s.clone()); } }
        v.truncate(8);
        v
    }

    pub fn qb_tick(&mut self) {
        if !self.is_server() || self.mock || !bb::profile::is_main() { return; }
        let Some((entry, unit)) = spare() else { return };
        let want = self.qb_want();
        let main_up = self.agent_up;
        if main_up { self.qb_main_down = None; } else if self.qb_main_down.is_none() { self.qb_main_down = Some(Instant::now()); }
        let main_gone = self.qb_main_down.map_or(false, |t| t.elapsed() > MAIN_GRACE);
        let run = self.quick_book && !main_gone && (main_up || self.qb_started);
        {
            let mut g = self.qb.lock().unwrap();
            g.on = run; g.want = want; g.addr = entry.agent.clone();
            g.focus = self.qb_focus.as_ref().filter(|f| f.1.elapsed() < Duration::from_secs(15)).map(|f| f.0.clone());
        }
        if run {
            let _ = std::fs::write(flag_path(), "1");
            let connected = self.qb.lock().unwrap().connected;
            if !self.qb_started || (!connected && self.qb_start_at.elapsed() > Duration::from_secs(150)) {
                self.qb_started = true; self.qb_start_at = Instant::now();
                if !connected {
                    self.say(ORANGE, &format!("quicker orderbook updates: starting the {} agent to read live order books", entry.name));
                    let unit = unit.clone();
                    if !cfg!(test) { std::thread::spawn(move || { let _ = std::process::Command::new("systemctl").args(["--user", "start", &unit]).status(); }); }
                }
            }
        } else if self.qb_started {
            self.qb_started = false;
            let _ = std::fs::remove_file(flag_path());
            self.qb.lock().unwrap().books.clear();
            let why = if main_gone { "the main agent has been offline for a minute" } else { "the setting was turned off" };
            self.say(ORANGE, &format!("quicker orderbook updates: stopping the {} agent ({why})", entry.name));
            if !cfg!(test) { std::thread::spawn(move || { std::thread::sleep(Duration::from_secs(2)); let _ = std::process::Command::new("systemctl").args(["--user", "stop", &unit]).status(); }); }
        }
        if run { quickbook::overlay(&self.qb, &mut self.products); }
        let err = { let mut g = self.qb.lock().unwrap(); if g.last_err != g.err_said { g.err_said = g.last_err.clone(); Some(g.last_err.clone()) } else { None } };
        if let Some(e) = err.filter(|e| e.contains("NO_COOKIE")) {
            let m = format!("Quicker orderbook updates can't work yet: the {} account has no Booster Cookie, so it can't open the bazaar. Eat a Booster Cookie on it (it tries again every 5 minutes).", entry.name);
            self.say(RED, &m); let _ = self.toast_new(2, m); let _ = e;
        }
    }

    /// For the state snapshot: how old each live book is, for the status line.
    pub fn qb_status(&self) -> Value {
        let g = self.qb.lock().unwrap();
        let now = now_secs();
        let books: serde_json::Map<String, Value> = g.books.iter().filter(|(_, b)| now - b.0 <= BOOK_FRESH).map(|(k, b)| (k.clone(), json!([b.0, b.1, b.2]))).collect();
        json!({"focus": self.qb_focus.as_ref().filter(|f| f.1.elapsed() < Duration::from_secs(15)).map(|f| f.0.clone()), "on": self.quick_book, "connected": g.connected, "live": books.len(), "err": g.last_err, "books": books})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fresh_books_replace_api_levels_and_stale_ones_do_not() {
        let qb: Qb = Default::default();
        let mut prods: HashMap<String, Product> = HashMap::new();
        let mk = || Product { sell_summary: vec![Level { price: 10.0, amount: 1, orders: 1 }], buy_summary: vec![Level { price: 12.0, amount: 1, orders: 1 }], quick_status: Default::default() };
        prods.insert("A".into(), mk()); prods.insert("B".into(), mk());
        { let mut g = qb.lock().unwrap();
          g.books.insert("A".into(), (now_secs(), vec![(10.5, 5, 1)], vec![(11.5, 3, 2)]));
          g.books.insert("B".into(), (now_secs() - 60.0, vec![(9.0, 5, 1)], vec![])); }
        overlay(&qb, &mut prods);
        assert_eq!((prods["A"].bid(), prods["A"].ask()), (10.5, 11.5));
        assert_eq!((prods["B"].bid(), prods["B"].ask()), (10.0, 12.0));
    }
}
