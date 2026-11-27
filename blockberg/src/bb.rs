//! Shared plumbing for the GUI: Hypixel bazaar feed, agent link, simulated agent.
#![allow(dead_code)]
use std::{collections::HashMap, sync::{Arc, Mutex}, time::{Duration, Instant}};

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::mpsc,
};

pub const BAZAAR_URL: &str = "https://api.hypixel.net/v2/skyblock/bazaar";
pub const DEFAULT_AGENT: &str = "127.0.0.1:7777";

/// One Blockberg per Minecraft account ("profile"): its own server, agent, jobs, journal, networth and layout. The default
/// profile "main" keeps the original locations; any other (BLOCKBERG_PROFILE=name) lives under profiles/<name> and is listed in
/// ~/.config/blockberg/agents.json: {"agents":[{"name":"alt1","server":"127.0.0.1:7782","lock":"127.0.0.1:7783","agent":"127.0.0.1:7787"}]}.
/// Prices, candles and alerts are shared between profiles.
pub mod profile {
    use std::path::PathBuf;
    #[derive(Clone, Debug, PartialEq)]
    pub struct Entry { pub name: String, pub server: String, pub lock: String, pub agent: String }
    fn home() -> PathBuf { std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")) }
    /// The window can switch account while it runs (the app is only a viewer on that account's server).
    static OVERRIDE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    pub fn set_name(n: &str) { if let Ok(mut g) = OVERRIDE.lock() { *g = Some(n.to_string()); } }
    pub fn name() -> String {
        if let Some(n) = OVERRIDE.lock().ok().and_then(|g| g.clone()) { return n; }
        std::env::var("BLOCKBERG_PROFILE").ok().filter(|s| !s.trim().is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')).unwrap_or_else(|| "main".into())
    }
    pub fn is_main() -> bool { name() == "main" }
    pub fn main_entry() -> Entry { Entry { name: "main".into(), server: "127.0.0.1:7780".into(), lock: "127.0.0.1:7781".into(), agent: super::DEFAULT_AGENT.into() } }
    /// Every profile: main first, then those in agents.json.
    pub fn all() -> Vec<Entry> {
        let mut v = vec![main_entry()];
        if let Some(t) = std::fs::read_to_string(home().join(".config/blockberg/agents.json")).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) {
            for a in t["agents"].as_array().into_iter().flatten() {
                let g = |k: &str| a[k].as_str().unwrap_or("").to_string();
                let e = Entry { name: g("name"), server: g("server"), lock: g("lock"), agent: g("agent") };
                if !e.name.is_empty() && e.name != "main" && !e.server.is_empty() && !e.lock.is_empty() && !e.agent.is_empty() { v.push(e); }
            }
        }
        v
    }
    /// This process's profile. An unknown name falls back to main's addresses only for "main"; others must be listed.
    pub fn entry() -> Entry {
        let n = name();
        all().into_iter().find(|e| e.name == n).unwrap_or_else(|| { eprintln!("blockberg: profile '{n}' is not listed in ~/.config/blockberg/agents.json"); std::process::exit(2) })
    }
    /// Data every account shares (the main profile's folder), e.g. the saved auto flip settings.
    pub fn shared_data_dir() -> PathBuf { home().join(".local/share/blockberg") }
    pub fn data_dir() -> PathBuf { let d = home().join(".local/share/blockberg"); if is_main() { d } else { d.join("profiles").join(name()) } }
    /// The data folder of any profile on this machine (the main profile's is the base folder).
    pub fn data_dir_of(name: &str) -> PathBuf { let d = home().join(".local/share/blockberg"); if name == "main" { d } else { d.join("profiles").join(name) } }
    pub fn config_dir() -> PathBuf { let d = home().join(".config/blockberg"); if is_main() { d } else { d.join("profiles").join(name()) } }
}
/// Coins an NPC will pay you per day in total (500M since Oct 2025; resets at 00:00 UTC).
pub const NPC_SELL_CAP: f64 = 500_000_000.0;
pub const MAX_QTY_PER_ORDER: u64 = 71_680;
/// No daily spend limit any more (the purse is the only limit); kept as a constant so the old checks stay inert.
pub const DAILY_SPEND_CAP: f64 = f64::INFINITY;
pub const CONFIRM_ABOVE: f64 = 500_000.0;

#[derive(Deserialize, Clone, Debug)]
pub struct Level {
    pub amount: u64,
    #[serde(rename = "pricePerUnit")]
    pub price: f64,
    pub orders: u64,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Quick {
    pub sell_price: f64,
    pub sell_moving_week: u64,
    pub buy_price: f64,
    pub buy_moving_week: u64,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Product {
    /// Buy orders: what you receive when you insta-sell (the "bid").
    pub sell_summary: Vec<Level>,
    /// Sell offers: what you pay when you insta-buy (the "ask").
    pub buy_summary: Vec<Level>,
    pub quick_status: Quick,
}

impl Product {
    pub fn ask(&self) -> f64 { self.buy_summary.first().map(|l| l.price).unwrap_or(self.quick_status.buy_price) }
    pub fn bid(&self) -> f64 { self.sell_summary.first().map(|l| l.price).unwrap_or(self.quick_status.sell_price) }
}

#[derive(Deserialize)]
struct BazaarResp { success: bool, products: HashMap<String, Product>, #[serde(rename = "lastUpdated", default)] last_updated: i64 }

pub async fn fetch_bazaar(client: &reqwest::Client) -> Result<HashMap<String, Product>> {
    let r: BazaarResp = client.get(BAZAAR_URL).send().await?.error_for_status()?.json().await?;
    anyhow::ensure!(r.success, "API returned success=false");
    Ok(r.products)
}

/// Same, plus Hypixel's own `lastUpdated` stamp (ms) so a fast poller can skip copies it has already seen.
pub async fn fetch_bazaar_stamped(client: &reqwest::Client) -> Result<(i64, HashMap<String, Product>)> {
    let r: BazaarResp = client.get(BAZAAR_URL).send().await?.error_for_status()?.json().await?;
    anyhow::ensure!(r.success, "API returned success=false");
    Ok((r.last_updated, r.products))
}

pub enum Msg {
    Prices(HashMap<String, Product>),
    PriceErr(String),
    AgentUp(bool),
    AgentLine(Value),
    /// Bank (and purse) from the Hypixel API, or a note on why it isn't available.
    Bank(Result<(f64, Option<f64>, String), String>),
    /// Server mode: a command from a connected app.
    Op(Value),
    /// Client mode: the server's latest state, and the state of the link to it.
    Snap(Value),
    Link(String),
}

pub async fn agent_task(addr: String, tx: mpsc::UnboundedSender<Msg>, mut cmds: mpsc::UnboundedReceiver<Value>) {
    loop {
        if let Ok(stream) = TcpStream::connect(&addr).await {
            let _ = tx.send(Msg::AgentUp(true));
            let (rd, mut wr) = stream.into_split();
            let mut lines = BufReader::new(rd).lines();
            let _ = wr.write_all(b"{\"cmd\":\"balance\"}\n").await;
            loop {
                tokio::select! {
                    line = lines.next_line() => match line {
                        Ok(Some(l)) => {
                            let v = serde_json::from_str(&l).unwrap_or_else(|_| json!({"type":"log","msg":l}));
                            let _ = tx.send(Msg::AgentLine(v));
                        }
                        _ => break,
                    },
                    cmd = cmds.recv() => match cmd {
                        Some(c) => {
                            let mut s = c.to_string(); s.push('\n');
                            if wr.write_all(s.as_bytes()).await.is_err() { break; }
                        }
                        None => return,
                    },
                }
            }
            let _ = tx.send(Msg::AgentUp(false));
        }
        while cmds.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Simulated in-game agent for demos without Minecraft. Mirrors the real agent's protocol:
/// resting limit orders that fill later, claimable orders, and fill-verification signals.
pub async fn demo_agent(tx: mpsc::UnboundedSender<Msg>, mut cmds: mpsc::UnboundedReceiver<Value>) {
    use std::sync::{Arc, Mutex};
    let purse = Arc::new(Mutex::new(2_000_000.0f64));
    let resting: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
    let bank = 25_000_000.0f64;
    let bal = |p: f64| json!({"type":"balance","purse":p,"bank":bank});
    let _ = tx.send(Msg::AgentUp(true));
    let _ = tx.send(Msg::AgentLine(bal(*purse.lock().unwrap())));
    while let Some(c) = cmds.recv().await {
        tokio::time::sleep(Duration::from_millis(600)).await;
        let cmd = c["cmd"].as_str().unwrap_or("").to_string();
        let (qty, price) = (c["qty"].as_u64().unwrap_or(0) as f64, c["price"].as_f64().unwrap_or(0.0));
        let product = c["product"].as_str().unwrap_or("?").to_string();
        let name = pretty(&product);
        let p = *purse.lock().unwrap();
        let all = json!({"chat":true,"purse":true,"inventory":true,"sacks":false});
        let reply = match cmd.as_str() {
            "balance" => bal(p),
            "instabuy" | "buyorder" if price * qty > p =>
                json!({"type":"error","msg":format!("not enough coins for {name} x{qty}")}),
            "instabuy" => {
                *purse.lock().unwrap() -= price * qty;
                json!({"type":"fill","msg":format!("[Bazaar] Bought {qty}x {name} for {:.1} coins!", price*qty),"cost":price*qty,"signals":all})
            }
            "instasell" => {
                *purse.lock().unwrap() += price * qty;
                json!({"type":"fill","msg":format!("[Bazaar] Sold {qty}x {name} for {:.1} coins!", price*qty),"signals":all})
            }
            "buyorder" | "selloffer" => {
                let side = if cmd == "buyorder" { "BUY" } else { "SELL" };
                if cmd == "buyorder" { *purse.lock().unwrap() -= price * qty; }
                let order = json!({"side":side,"product":name,"amount":qty as u64,"price":price,"filled":0,"total":qty as u64,"claimable":false,"to_claim":0});
                resting.lock().unwrap().push(order.clone());
                // it fills a few seconds later; fills of resting orders send a chat line
                let (tx2, rest2, name2, cmd2) = (tx.clone(), resting.clone(), name.clone(), cmd.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(8)).await;
                    let mut r = rest2.lock().unwrap();
                    if let Some(o) = r.iter_mut().find(|o| o["product"] == name2.as_str() && o["price"].as_f64() == Some(price)) {
                        o["filled"] = json!(qty as u64);
                        o["claimable"] = json!(true);
                        o["to_claim"] = json!(qty as u64);
                        drop(r);
                        let word = if cmd2 == "buyorder" { "Buy Order" } else { "Sell Offer" };
                        let _ = tx2.send(Msg::AgentLine(json!({"type":"bazaar","msg":format!("[Bazaar] Your {word} for {qty}x {name2} was filled!")})));
                    }
                });
                let word = if cmd == "buyorder" { "Buy Order" } else { "Sell Offer" };
                json!({"type":"placed","msg":format!("[Bazaar] {word} Setup! {qty}x {name} for {price:.1} coins."),"order":order,"price":price})
            }
            "orders" => json!({"type":"orders","orders":resting.lock().unwrap().clone()}),
            "requote" => {
                let (slice, remaining, maxq) = (c["slice"].as_u64().unwrap_or(0), c["remaining"].as_u64().unwrap_or(0), c["max_qty"].as_u64().unwrap_or(71_680));
                let mut r = resting.lock().unwrap();
                match r.iter().position(|o| o["side"] == "BUY" && o["product"] == name.as_str()) {
                    None => json!({"type":"error","msg":"no open BUY order found"}),
                    Some(i) => {
                        let o = r.remove(i);
                        let (filled, total) = (o["filled"].as_u64().unwrap_or(0), o["total"].as_u64().unwrap_or(0));
                        *purse.lock().unwrap() += o["price"].as_f64().unwrap_or(0.0) * (total - filled) as f64;
                        let q = slice.min(remaining.saturating_sub(filled)).min(maxq);
                        let np = ((o["price"].as_f64().unwrap_or(1.0) + 0.1) * 10.0).round() / 10.0;
                        let mut out = json!({"type":"requoted","product":product,"result":"cancelled","filled":filled,"total":total,"qty":q});
                        if q > 0 {
                            *purse.lock().unwrap() -= np * q as f64;
                            r.push(json!({"side":"BUY","product":name,"amount":q,"price":np,"filled":0,"total":q,"claimable":false,"to_claim":0}));
                            out["placed"] = json!("[Bazaar] Buy Order Setup!"); out["price"] = json!(np);
                        }
                        out
                    }
                }
            }
            "claimorder" => {
                let side = c["side"].as_str().unwrap_or("").to_uppercase();
                let mut r = resting.lock().unwrap();
                match r.iter().position(|o| o["side"] == side.as_str() && o["product"] == name.as_str() && o["claimable"] == true) {
                    Some(i) => {
                        let o = r.remove(i);
                        let v = o["price"].as_f64().unwrap_or(0.0) * o["amount"].as_f64().unwrap_or(0.0);
                        if side == "SELL" { *purse.lock().unwrap() += v; }
                        let sig = if side == "BUY" { json!({"chat":true,"orders":true,"inventory":true,"sacks":false,"purse":null}) }
                                  else { json!({"chat":true,"orders":true,"inventory":null,"sacks":null,"purse":true}) };
                        json!({"type":"claimed","msg":format!("[Bazaar] Claimed {}x {name} worth {v:.1} coins!", o["amount"]),
                               "product":product,"side":side,"price":price,"signals":sig,
                               "info":{"rounds":1,"reason":"order fully claimed","claimed_msgs":1}})
                    }
                    None => json!({"type":"error","msg":"nothing to claim yet"}),
                }
            }
            "fliporder" => {
                let side = c["side"].as_str().unwrap_or("").to_uppercase();
                let sp = c["sell_price"].as_f64().unwrap_or(0.0);
                let mut r = resting.lock().unwrap();
                match r.iter().position(|o| o["side"] == side.as_str() && o["product"] == name.as_str() && o["claimable"] == true) {
                    Some(i) => {
                        let o = r.remove(i);
                        let q = o["amount"].as_u64().unwrap_or(0);
                        r.push(json!({"side":"SELL","product":name,"amount":q,"price":sp,"filled":0,"total":q,"claimable":false,"to_claim":0}));
                        json!({"type":"flipped","msg":format!("[Bazaar] Order Flipped! {q}x {name} for {:.1} coins of total expected profit.", (sp - o["price"].as_f64().unwrap_or(0.0)) * q as f64),
                               "product":product,"side":side,"price":c["price"],"sell_price":sp,"qty":q})
                    }
                    None => json!({"type":"error","msg":"nothing filled yet to flip"}),
                }
            }
            "cancel" => {
                let side = c["side"].as_str().unwrap_or("").to_uppercase();
                let mut r = resting.lock().unwrap();
                match r.iter().position(|o| o["side"] == side.as_str() && o["product"] == name.as_str()) {
                    Some(i) => {
                        let o = r.remove(i);
                        if side == "BUY" { *purse.lock().unwrap() += o["price"].as_f64().unwrap_or(0.0) * o["amount"].as_f64().unwrap_or(0.0); }
                        json!({"type":"log","msg":format!("[Bazaar] Cancelled! Refunded from cancelling {side} Order!")})
                    }
                    None => json!({"type":"error","msg":"no matching order"}),
                }
            }
            "holdings" => json!({"type":"holdings","product":product,"inventory":0,"sacks":0}),
            "sackscan" => json!({"type":"sacks","sacks":[
                {"name":"Large Mining Sack","copies":8,"cap_each":20160,"items":[{"name":"Coal","stored":4000},{"name":"Cobblestone","stored":0},{"name":"Diamond","stored":120}]},
                {"name":"Small Agronomy Sack","copies":1,"cap_each":2240,"items":[{"name":"Seeds","stored":300},{"name":"Pumpkin","stored":0}]},
                {"name":"Large Enchanted Agronomy Sack","copies":1,"cap_each":20160,"items":[{"name":"Enchanted Seeds","stored":50},{"name":"Box of Seeds","stored":0},{"name":"Polished Pumpkin","stored":0}]}]}),
            "invall" => json!({"type":"invall","free_slots":20,"items":[
                {"id":"COAL","count":1280,"fits":1280+16*64},{"id":"SEEDS","count":640,"fits":16*64+384},{"id":"ENCHANTED_COAL","count":12,"fits":16*64+52}],
                "slots":[{"slot":0,"id":"COAL","count":64},{"slot":1,"id":"COAL","count":64},{"slot":2,"id":"SEEDS","count":64},{"slot":4,"id":"ENCHANTED_COAL","count":12},
                         {"slot":9,"id":"COAL","count":64},{"slot":10,"id":"COAL","count":64},{"slot":11,"id":"COAL","count":64},{"slot":12,"id":"SEEDS","count":64},{"slot":13,"id":"SEEDS","count":64},
                         {"slot":18,"id":"COAL","count":64},{"slot":19,"id":"ASPECT_OF_THE_END","count":1},{"slot":27,"id":"SKYBLOCK_MENU","count":1},{"slot":35,"id":"COAL","count":40}]}),
            "pipeline" => {
                let steps = c["steps"].as_array().cloned().unwrap_or_default();
                let start = *purse.lock().unwrap();
                for (i, st) in steps.iter().enumerate() {
                    tokio::time::sleep(Duration::from_millis(1100)).await;
                    let op = st["op"].as_str().unwrap_or("?");
                    let q = st["qty"].as_f64().unwrap_or(0.0);
                    let msg = match op {
                        "instabuy" => { *purse.lock().unwrap() -= q * 2.0; format!("[Bazaar] Bought {q}x {} for {:.1} coins!", pretty(st["product"].as_str().unwrap_or("")), q * 2.0) }
                        "wait" => "waited (compactor)".to_string(),
                        "sackget" => format!("moved {q}x {} from sacks", pretty(st["product"].as_str().unwrap_or(""))),
                        "tradesell" => { let n = (start_qty(&steps) / 160.0).floor(); *purse.lock().unwrap() += n * 480.0; format!("sold {n}x to the shop for {:.1} coins", n * 480.0) }
                        _ => "done".to_string(),
                    };
                    let _ = tx.send(Msg::AgentLine(json!({"type":"step","i":i,"op":op,"ok":true,"msg":msg})));
                }
                json!({"type":"pipeline_done","ok":true,"purse_start":start,"purse_end":*purse.lock().unwrap()})
            }
            "claim" => json!({"type":"log","msg":"no coins to claim"}),
            "subscribe_chat" | "version" => continue,   // the real agent sends nothing back for these
            _ => json!({"type":"error","msg":"unknown cmd"}),
        };
        let _ = tx.send(Msg::AgentLine(reply));
        let _ = tx.send(Msg::AgentLine(bal(*purse.lock().unwrap())));
    }
}

pub fn fmt_coins(x: f64) -> String {
    let a = x.abs();
    if a >= 1e9 { format!("{:.2}B", x / 1e9) }
    else if a >= 1e6 { format!("{:.2}M", x / 1e6) }
    else if a >= 1e4 { format!("{:.1}k", x / 1e3) }
    else { format!("{:.1}", x) }
}

/// Display-only shortening: "Enchanted" -> "Ench", "Block" -> "Blk" (whole words, any case style).
pub fn short(s: &str) -> String {
    s.split(' ').map(|w| match w { "Enchanted" => "Ench", "Block" => "Blk", "ENCHANTED" => "ENCH", "BLOCK" => "BLK", "enchanted" => "ench", "block" => "blk", o => o }).collect::<Vec<_>>().join(" ")
}
pub fn short_id(id: &str) -> String { id.split('_').map(|w| match w { "ENCHANTED" => "ENCH", "BLOCK" => "BLK", o => o }).collect::<Vec<_>>().join("_") }
pub fn pretty_s(id: &str) -> String { short(&pretty(id)) }

pub fn pretty(id: &str) -> String {
    id.split('_').map(|w| {
        let mut c = w.chars();
        c.next().map(|f| f.to_string() + &c.as_str().to_lowercase()).unwrap_or_default()
    }).collect::<Vec<_>>().join(" ")
}

fn start_qty(steps: &[Value]) -> f64 {
    steps.iter().find(|s| s["op"] == "instabuy").and_then(|s| s["qty"].as_f64()).unwrap_or(0.0)
}

/// Bank balance from Hypixel's public API (needs ~/.config/blockberg/hypixel_key and hypixel_uuid; the Banking API must be on in SkyBlock settings).
/// What the Hypixel API last said the sacks hold (item id -> count, non-zero only) and when we read it.
pub static SACK_API: std::sync::Mutex<Option<(std::time::Instant, std::collections::HashMap<String, u64>)>> = std::sync::Mutex::new(None);

pub async fn fetch_bank(client: &reqwest::Client) -> Result<(f64, Option<f64>, String), String> {
    let dir = profile::config_dir();
    let shared = std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".config/blockberg")).map_err(|e| e.to_string())?;
    let key = std::fs::read_to_string(shared.join("hypixel_key")).map_err(|_| "no key file".to_string())?.trim().to_string();   // one API key for all profiles
    let uuid = std::fs::read_to_string(dir.join("hypixel_uuid")).map_err(|_| "no uuid file".to_string())?.trim().replace('-', "");
    if key.is_empty() || uuid.is_empty() { return Err("empty key or uuid".into()); }
    let resp = client.get("https://api.hypixel.net/v2/skyblock/profiles").query(&[("uuid", uuid.as_str())]).header("API-Key", key).send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    if !status.is_success() || v["success"] != true { return Err(format!("{status}: {}", v["cause"].as_str().unwrap_or("request failed"))); }
    let profs = v["profiles"].as_array().ok_or("no profiles")?;
    let p = profs.iter().find(|p| p["selected"] == true).or(profs.first()).ok_or("no profiles")?;
    // A co-op has two accounts: the shared co-op bank (profile.banking, needs the Banking API on) and your personal one (member.profile.bank_account)
    let me = &p["members"][uuid.as_str()];
    if let Some(sc) = me["inventory"]["sacks_counts"].as_object() {
        let m = sc.iter().filter_map(|(k, v)| v.as_u64().filter(|n| *n > 0).map(|n| (k.clone(), n))).collect();
        if let Ok(mut g) = SACK_API.lock() { *g = Some((std::time::Instant::now(), m)); }
    }
    let coop = p["banking"]["balance"].as_f64();
    let personal = me["profile"]["bank_account"].as_f64();
    if coop.is_none() && personal.is_none() { return Err("no bank data (turn on SkyBlock menu > Settings > API Settings > Banking)".into()); }
    // In a co-op the shared account usually holds the money. If the API hides it, a personal-only total would be badly wrong.
    if coop.is_none() && p["members"].as_object().map_or(false, |m| m.len() > 1) {
        return Err(format!("the API hides the co-op bank (personal account is {:.0}); using the in-game /bank read instead", personal.unwrap_or(0.0)));
    }
    let total = coop.unwrap_or(0.0) + personal.unwrap_or(0.0);
    let detail = format!("co-op {} + personal {}", coop.map_or("unknown (Banking API off)".to_string(), |c| format!("{c:.0}")), personal.map_or("unknown".to_string(), |c| format!("{c:.0}")));
    Ok((total, me["currencies"]["coin_purse"].as_f64(), detail))
}

/// Tiny calculator for the command line: + - * / ^ % ( ), thousands commas, k/m/b/t suffixes, "x" for times, trailing "=".
/// Returns None when the text is not arithmetic (so item names and commands never get mistaken for it).
pub fn calc_eval(text: &str) -> Option<f64> {
    let t = text.trim().trim_end_matches('=').trim().to_lowercase().replace('×', "*").replace('÷', "/");
    if t.is_empty() { return None; }
    let has_op = t.chars().any(|c| "+-*/^%x".contains(c)) || text.trim_end().ends_with('=');
    if !has_op || !t.chars().all(|c| c.is_ascii_digit() || " .,+-*/^%()xkmbt".contains(c)) || !t.chars().any(|c| c.is_ascii_digit()) { return None; }
    let chars: Vec<char> = t.chars().filter(|c| *c != ' ' && *c != ',').collect();
    let mut p = Calc { c: chars, i: 0 };
    let v = p.expr()?;
    if p.i != p.c.len() || !v.is_finite() { return None; }
    Some(v)
}

struct Calc { c: Vec<char>, i: usize }
impl Calc {
    fn peek(&self) -> Option<char> { self.c.get(self.i).copied() }
    fn expr(&mut self) -> Option<f64> {
        let mut v = self.term()?;
        while let Some(o) = self.peek() { if o == '+' || o == '-' { self.i += 1; let r = self.term()?; if o == '+' { v += r } else { v -= r } } else { break } }
        Some(v)
    }
    fn term(&mut self) -> Option<f64> {
        let mut v = self.power()?;
        while let Some(o) = self.peek() {
            if o == '*' || o == 'x' || o == '/' || o == '%' {
                self.i += 1; let r = self.power()?;
                v = match o { '/' => { if r == 0.0 { return None } v / r }, '%' => { if r == 0.0 { return None } v % r }, _ => v * r };
            } else { break }
        }
        Some(v)
    }
    fn power(&mut self) -> Option<f64> {
        let b = self.unary()?;
        if self.peek() == Some('^') { self.i += 1; let e = self.power()?; return Some(b.powf(e)); }
        Some(b)
    }
    fn unary(&mut self) -> Option<f64> {
        match self.peek()? { '-' => { self.i += 1; Some(-self.unary()?) }, '+' => { self.i += 1; self.unary() }, _ => self.atom() }
    }
    fn atom(&mut self) -> Option<f64> {
        if self.peek()? == '(' { self.i += 1; let v = self.expr()?; if self.peek()? != ')' { return None } self.i += 1; return Some(v); }
        let st = self.i;
        while self.peek().map_or(false, |c| c.is_ascii_digit() || c == '.') { self.i += 1; }
        if st == self.i { return None; }
        let mut v: f64 = self.c[st..self.i].iter().collect::<String>().parse().ok()?;
        match self.peek() { Some('k') => { v *= 1e3; self.i += 1 }, Some('m') => { v *= 1e6; self.i += 1 }, Some('b') => { v *= 1e9; self.i += 1 }, Some('t') => { v *= 1e12; self.i += 1 }, _ => {} }
        Some(v)
    }
}

/// 10 -> "10", 1500000 -> "1,500,000 (1.50m)", 2.5 -> "2.5"
pub fn calc_fmt(v: f64) -> String {
    let body = if v.fract().abs() < 1e-9 && v.abs() < 1e15 {
        let n = v.round() as i64; let s = n.abs().to_string();
        let mut out = String::new();
        for (i, ch) in s.chars().enumerate() { if i > 0 && (s.len() - i) % 3 == 0 { out.push(','); } out.push(ch); }
        format!("{}{}", if n < 0 { "-" } else { "" }, out)
    } else { let t = format!("{:.4}", v); t.trim_end_matches('0').trim_end_matches('.').to_string() };
    let a = v.abs();
    let short = if a >= 1e12 { Some(format!("{:.2}t", v / 1e12)) } else if a >= 1e9 { Some(format!("{:.2}b", v / 1e9)) } else if a >= 1e6 { Some(format!("{:.2}m", v / 1e6)) } else if a >= 1e4 { Some(format!("{:.1}k", v / 1e3)) } else { None };
    match short { Some(s) => format!("{body} ({s})"), None => body }
}

/// One thing the app asked the agent to do, kept so the Queue screen can show it (sent, then done / failed).
#[derive(Clone)]
pub struct ActRec { pub id: u64, pub auto: bool, pub cmd: String, pub label: String, pub sent: Instant, pub done: Option<(Instant, Option<String>)> }
static ACT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// When (unix secs) the user last asked the agent for something (background "auto" commands and plain reads do not count).
pub static LAST_USER_CMD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Called with every command before it is sent (the server writes its journal here).
pub static JOURNAL: std::sync::OnceLock<Box<dyn Fn(&Value) + Send + Sync>> = std::sync::OnceLock::new();

/// The sender every part of the app uses to talk to the agent; it also notes the manual / background actions in a log.
#[derive(Clone)]
pub struct CmdTx { tx: mpsc::UnboundedSender<Value>, pub log: Arc<Mutex<Vec<ActRec>>>, remote_up: Option<Arc<std::sync::atomic::AtomicBool>> }

impl CmdTx {
    pub fn new(tx: mpsc::UnboundedSender<Value>) -> Self { Self { tx, log: Arc::new(Mutex::new(vec![])), remote_up: None } }
    /// The app's end of a connection to the Blockberg server: every agent command is forwarded to the server, which journals
    /// and sends it. Nothing is sent while the link is down (a stale command must never run later, after a reconnect).
    pub fn new_remote(op_tx: mpsc::UnboundedSender<Value>, up: Arc<std::sync::atomic::AtomicBool>) -> Self { Self { tx: op_tx, log: Arc::new(Mutex::new(vec![])), remote_up: Some(up) } }
    pub fn send(&self, v: Value) -> Result<(), mpsc::error::SendError<Value>> {
        if let Some(up) = &self.remote_up {
            if !up.load(std::sync::atomic::Ordering::Relaxed) { return Err(mpsc::error::SendError(v)); }
            return self.tx.send(serde_json::json!({"op": "agent", "msg": v}));
        }
        // An automatic read that is already waiting for the agent is not queued again: they only pile up behind a long action and then run one after the other
        // (21 order reads waiting for 10 minutes), each opening the bazaar menu again. One that has waited over a minute is sent anyway, in case its answer was lost.
        if v["auto"] == true && matches!(v["cmd"].as_str().unwrap_or(""), "orders" | "invall" | "balance") {
            let c = v["cmd"].as_str().unwrap_or("");
            if self.log.lock().map_or(false, |l| l.iter().any(|a| a.cmd == c && a.done.is_none() && a.sent.elapsed() < Duration::from_secs(60))) { return Ok(()); }
        }
        if let Some(j) = JOURNAL.get() { j(&v); }
        if v["auto"] != true && !matches!(v["cmd"].as_str().unwrap_or(""), "invall" | "invcount" | "balance" | "subscribe_chat" | "version" | "ping") {
            LAST_USER_CMD.store(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()), std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(label) = act_label(&v) {
            if let Ok(mut l) = self.log.lock() { l.push(ActRec { id: ACT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed), auto: v["auto"] == true, cmd: v["cmd"].as_str().unwrap_or("").to_string(), label, sent: Instant::now(), done: None }); }
        }
        self.tx.send(v)
    }
}

/// A readable name for the commands worth showing in the queue (placements already have their own rows there).
pub fn act_label(v: &Value) -> Option<String> {
    let cmd = v["cmd"].as_str()?;
    let who = || format!("{} {}", v["side"].as_str().unwrap_or(""), pretty_s(v["product"].as_str().unwrap_or("")));
    Some(match cmd {
        "orders" => "Read the order menu (refresh escrow)".to_string(),
        "claimorder" => format!("Claim {} from escrow", who().trim()),
        "claim" => "Claim coins".to_string(),
        "cancel" => format!("Cancel {} @ {}", who().trim(), v["price"].as_f64().map_or("?".to_string(), |p| format!("{p:.1}"))),
        "fliporder" => format!("Flip {} into a sell offer", who().trim()),
        "sackscan" => "Scan sacks (opens every sack once)".to_string(),
        "invall" => "Read the inventory".to_string(),
        "balance" if v["bank"] == "now" => "Read purse + bank".to_string(),
        "reload" => "Restart the agent".to_string(),
        "pickup" => "Pick up stash + Lost & Found".to_string(),
        "insert_sacks" => "Insert inventory into sacks".to_string(),
        "bank" => format!("Bank: {} {} ({})", v["action"].as_str().unwrap_or("deposit"), v["account"].as_str().unwrap_or("coop"), v["amount"].as_str().map(|s| s.to_string()).or(v["amount"].as_f64().map(|a| format!("{a:.0}"))).or(v["keep"].as_f64().map(|k| format!("all but {k:.0}"))).or(v["target"].as_f64().map(|t| format!("up to {t:.0}"))).unwrap_or_default()),
        "sellinv" => "List inventory on the bazaar".to_string(),
        "sackget" => format!("Take items out of sacks: {}", pretty_s(v["product"].as_str().unwrap_or(""))),
        _ => return None,
    })
}
