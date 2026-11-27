//! Blockberg — a Bloomberg-style terminal for the Hypixel SkyBlock bazaar.
//!
//! Runs on the PC. Prices come straight from the public Hypixel bazaar API.
//! Orders go over TCP (one JSON object per line) to `agent/blockberg_agent.py`,
//! a minescript running inside Minecraft, which carries them out.
//!
//!   blockberg [AGENT_ADDR]      default 127.0.0.1:7777 (env BLOCKBERG_AGENT)

use std::{collections::HashMap, time::Duration};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Sparkline, Table, TableState},
    Frame,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::mpsc,
};

const BAZAAR_URL: &str = "https://api.hypixel.net/v2/skyblock/bazaar";
const DEFAULT_AGENT: &str = "127.0.0.1:7777";
const MAX_QTY_PER_ORDER: u64 = 2304;
const DAILY_SPEND_CAP: f64 = 5_000_000.0;
const CONFIRM_ABOVE: f64 = 500_000.0;
const HISTORY_LEN: usize = 720;
const DEFAULT_WATCH: &[&str] = &[
    "COAL", "ENCHANTED_COAL", "ENCHANTED_COAL_BLOCK", "IRON_INGOT", "ENCHANTED_IRON",
    "GOLD_INGOT", "ENCHANTED_GOLD", "DIAMOND", "ENCHANTED_DIAMOND", "COBBLESTONE",
    "ENCHANTED_COBBLESTONE", "WHEAT", "ENCHANTED_BREAD", "SUGAR_CANE", "ENCHANTED_SUGAR",
];

// ---------- Hypixel API ----------

#[derive(Deserialize, Clone, Debug)]
struct Level {
    amount: u64,
    #[serde(rename = "pricePerUnit")]
    price: f64,
    orders: u64,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct Quick {
    sell_price: f64,
    sell_moving_week: u64,
    buy_price: f64,
    buy_moving_week: u64,
}

#[derive(Deserialize, Clone, Debug)]
struct Product {
    /// Buy orders — what you receive when you insta-sell.
    sell_summary: Vec<Level>,
    /// Sell offers — what you pay when you insta-buy.
    buy_summary: Vec<Level>,
    quick_status: Quick,
}

impl Product {
    fn insta_buy(&self) -> f64 {
        self.buy_summary.first().map(|l| l.price).unwrap_or(self.quick_status.buy_price)
    }
    fn insta_sell(&self) -> f64 {
        self.sell_summary.first().map(|l| l.price).unwrap_or(self.quick_status.sell_price)
    }
}

#[derive(Deserialize)]
struct BazaarResp {
    success: bool,
    products: HashMap<String, Product>,
}

async fn fetch_bazaar(client: &reqwest::Client) -> Result<HashMap<String, Product>> {
    let r: BazaarResp = client.get(BAZAAR_URL).send().await?.error_for_status()?.json().await?;
    anyhow::ensure!(r.success, "API returned success=false");
    Ok(r.products)
}

// ---------- Messages between tasks ----------

enum Msg {
    Prices(HashMap<String, Product>),
    PriceErr(String),
    AgentUp(bool),
    AgentLine(Value),
}

/// Keeps a TCP connection to the in-game agent alive and pumps lines both ways.
async fn agent_task(addr: String, tx: mpsc::UnboundedSender<Msg>, mut cmds: mpsc::UnboundedReceiver<Value>) {
    loop {
        match TcpStream::connect(&addr).await {
            Ok(stream) => {
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
            Err(_) => {}
        }
        // Drop commands queued while disconnected so nothing fires late.
        while cmds.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

// ---------- App state ----------

#[derive(PartialEq)]
enum Mode { Normal, Search, Qty }

struct Order { side: &'static str, id: String, qty: u64, unit: f64 }

struct App {
    history: HashMap<String, Vec<f64>>,
    halted: bool,
    spent_today: f64,
    pending: Option<Order>,
    products: HashMap<String, Product>,
    watch: Vec<String>,
    table: TableState,
    mode: Mode,
    input: String,
    qty: u64,
    log: Vec<(Color, String)>,
    agent_addr: String,
    agent_up: bool,
    purse: Option<f64>,
    bank: Option<f64>,
    last_update: Option<std::time::Instant>,
    cmd_tx: mpsc::UnboundedSender<Value>,
}

impl App {
    fn selected(&self) -> Option<&String> { self.table.selected().and_then(|i| self.watch.get(i)) }

    fn push_log(&mut self, c: Color, s: impl Into<String>) {
        self.log.push((c, s.into()));
        if self.log.len() > 200 { self.log.remove(0); }
    }

    /// Validate against the risk limits, then either confirm or dispatch.
    fn request(&mut self, side: &'static str) {
        let Some(id) = self.selected().cloned() else { return };
        if self.halted { self.push_log(Color::Red, "TRADING HALTED — press x to resume"); return; }
        if !self.agent_up { self.push_log(Color::Red, format!("agent offline — {side} {id} not sent")); return; }
        let unit = match self.products.get(&id) {
            Some(p) => match side {
                "instabuy" => p.insta_buy(),
                "instasell" => p.insta_sell(),
                "buyorder" => p.insta_sell() + 0.1,
                _ => p.insta_buy() - 0.1,
            },
            None => return,
        };
        let value = unit * self.qty as f64;
        if self.qty > MAX_QTY_PER_ORDER && side != "instasell" {
            self.push_log(Color::Red, format!("blocked: qty {} over max {MAX_QTY_PER_ORDER}", self.qty));
            return;
        }
        let is_buy = matches!(side, "instabuy" | "buyorder");
        if is_buy && self.spent_today + value > DAILY_SPEND_CAP {
            self.push_log(Color::Red, format!("blocked: daily spend cap {} reached", fmt_coins(DAILY_SPEND_CAP)));
            return;
        }
        let o = Order { side, id, qty: self.qty, unit };
        if value > CONFIRM_ABOVE {
            self.push_log(Color::Yellow, format!("CONFIRM {} {} x{} ~{} — Enter/y to send, Esc to cancel",
                o.side, o.id, o.qty, fmt_coins(value)));
            self.pending = Some(o);
        } else {
            self.dispatch(o);
        }
    }

    fn dispatch(&mut self, o: Order) {
        let _ = self.cmd_tx.send(json!({"cmd": o.side, "product": o.id, "qty": o.qty, "price": o.unit}));
        self.push_log(Color::Yellow, format!("→ {} {} x{} @ {} (~{})", o.side, o.id, o.qty,
            fmt_coins(o.unit), fmt_coins(o.unit * o.qty as f64)));
    }

    fn record_history(&mut self) {
        for id in self.watch.clone() {
            if let Some(p) = self.products.get(&id) {
                let h = self.history.entry(id).or_default();
                h.push(p.insta_buy());
                if h.len() > HISTORY_LEN { h.remove(0); }
            }
        }
    }

    fn on_agent(&mut self, v: Value) {
        let t = v["type"].as_str().unwrap_or("log");
        match t {
            "balance" => {
                self.purse = v["purse"].as_f64();
                self.bank = v["bank"].as_f64();
            }
            "fill" => {
                if let Some(c) = v["cost"].as_f64() { self.spent_today += c; }
                self.push_log(Color::Green, format!("✓ {}", v["msg"].as_str().unwrap_or(&v.to_string())));
            }
            "error" => self.push_log(Color::Red, format!("✗ {}", v["msg"].as_str().unwrap_or(&v.to_string()))),
            _ => self.push_log(Color::Gray, v["msg"].as_str().map(String::from).unwrap_or(v.to_string())),
        }
    }

    /// Search box: exact id, or first product whose id contains the text.
    fn add_from_search(&mut self) {
        let q = self.input.trim().to_uppercase().replace(' ', "_");
        self.input.clear();
        if q.is_empty() { return; }
        let hit = if self.products.contains_key(&q) { Some(q.clone()) } else {
            let mut ids: Vec<&String> = self.products.keys().filter(|k| k.contains(&q)).collect();
            ids.sort_by_key(|k| k.len());
            ids.first().map(|s| s.to_string())
        };
        match hit {
            Some(id) => {
                let pos = self.watch.iter().position(|w| *w == id).unwrap_or_else(|| { self.watch.push(id.clone()); self.watch.len() - 1 });
                self.table.select(Some(pos));
            }
            None => self.push_log(Color::Red, format!("no product matching {q}")),
        }
    }
}

fn fmt_coins(x: f64) -> String {
    let a = x.abs();
    if a >= 1e9 { format!("{:.2}B", x / 1e9) }
    else if a >= 1e6 { format!("{:.2}M", x / 1e6) }
    else if a >= 1e4 { format!("{:.1}k", x / 1e3) }
    else { format!("{:.1}", x) }
}

fn pretty(id: &str) -> String {
    id.split('_').map(|w| {
        let mut c = w.chars();
        c.next().map(|f| f.to_string() + &c.as_str().to_lowercase()).unwrap_or_default()
    }).collect::<Vec<_>>().join(" ")
}

// ---------- UI ----------

fn draw(f: &mut Frame, app: &mut App) {
    let rows = Layout::default().direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(8), Constraint::Length(9), Constraint::Length(1)])
        .split(f.area());

    // Header
    let agent = if app.agent_up { Span::styled(" AGENT ● ", Style::default().fg(Color::Black).bg(Color::Green)) }
        else { Span::styled(" AGENT ○ ", Style::default().fg(Color::Black).bg(Color::Red)) };
    let age = app.last_update.map(|t| format!("{}s ago", t.elapsed().as_secs())).unwrap_or("loading…".into());
    let bal = format!("  purse {}  bank {}",
        app.purse.map(fmt_coins).unwrap_or("—".into()), app.bank.map(fmt_coins).unwrap_or("—".into()));
    f.render_widget(Paragraph::new(Line::from(vec![
        Span::styled(" BLOCKBERG ", Style::default().fg(Color::Black).bg(Color::Rgb(255, 140, 0)).add_modifier(Modifier::BOLD)),
        Span::raw(" "), agent, Span::raw(format!(" {}  prices {age}", app.agent_addr)),
        Span::styled(bal, Style::default().fg(Color::Yellow)),
        Span::raw(format!("  spent {}/{}", fmt_coins(app.spent_today), fmt_coins(DAILY_SPEND_CAP))),
        if app.halted { Span::styled("  ■ HALTED", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)) } else { Span::raw("") },
    ])), rows[0]);

    let mid = Layout::default().direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)]).split(rows[1]);

    // Watchlist
    let orange = Style::default().fg(Color::Rgb(255, 140, 0));
    let header = Row::new(["Product", "Insta-buy", "Insta-sell", "Spread", "Margin", "Wk vol"])
        .style(orange.add_modifier(Modifier::BOLD));
    let body: Vec<Row> = app.watch.iter().map(|id| match app.products.get(id) {
        Some(p) => {
            let (b, s) = (p.insta_buy(), p.insta_sell());
            let spread = b - s;
            let margin = if b > 0.0 { spread / b * 100.0 } else { 0.0 };
            Row::new(vec![
                Cell::from(pretty(id)),
                Cell::from(fmt_coins(b)).style(Style::default().fg(Color::Green)),
                Cell::from(fmt_coins(s)).style(Style::default().fg(Color::Red)),
                Cell::from(fmt_coins(spread)),
                Cell::from(format!("{margin:.1}%")),
                Cell::from(fmt_coins((p.quick_status.buy_moving_week + p.quick_status.sell_moving_week) as f64)),
            ])
        }
        None => Row::new(vec![Cell::from(pretty(id)), Cell::from("—")]),
    }).collect();
    let t = Table::new(body, [Constraint::Min(22), Constraint::Length(10), Constraint::Length(10),
        Constraint::Length(9), Constraint::Length(7), Constraint::Length(8)])
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" Watchlist ").border_style(orange))
        .row_highlight_style(Style::default().bg(Color::Rgb(50, 50, 70)).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(t, mid[0], &mut app.table);

    // Order book for the selected product
    let mut lines = vec![];
    if let Some(p) = app.selected().and_then(|id| app.products.get(id)) {
        lines.push(Line::styled("  SELL OFFERS (you insta-buy)", Style::default().fg(Color::Green)));
        for l in p.buy_summary.iter().take(6).rev() {
            lines.push(Line::raw(format!("  {:>12}  {:>9}  {:>3} ord", fmt_coins(l.price), l.amount, l.orders)));
        }
        lines.push(Line::styled("  ────────────────────────────", Style::default().fg(Color::DarkGray)));
        for l in p.sell_summary.iter().take(6) {
            lines.push(Line::raw(format!("  {:>12}  {:>9}  {:>3} ord", fmt_coins(l.price), l.amount, l.orders)));
        }
        lines.push(Line::styled("  BUY ORDERS (you insta-sell)", Style::default().fg(Color::Red)));
        lines.push(Line::raw(""));
        let (b, s) = (p.insta_buy(), p.insta_sell());
        lines.push(Line::styled(format!("  Qty {}  → buy ~{}  sell ~{}", app.qty,
            fmt_coins(b * app.qty as f64), fmt_coins(s * app.qty as f64)), Style::default().fg(Color::Yellow)));
    }
    let title = app.selected().map(|s| format!(" {} ", pretty(s))).unwrap_or(" Book ".into());
    let right = Layout::default().direction(Direction::Vertical)
        .constraints([Constraint::Min(10), Constraint::Length(6)]).split(mid[1]);
    f.render_widget(Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title).border_style(orange)), right[0]);
    let spark: Vec<u64> = app.selected().and_then(|id| app.history.get(id)).map(|h| {
        let (lo, hi) = h.iter().fold((f64::MAX, f64::MIN), |(a, b), &x| (a.min(x), b.max(x)));
        h.iter().map(|&x| if hi > lo { ((x - lo) / (hi - lo) * 100.0) as u64 } else { 50 }).collect()
    }).unwrap_or_default();
    let range = app.selected().and_then(|id| app.history.get(id)).filter(|h| h.len() > 1).map(|h| {
        let (lo, hi) = h.iter().fold((f64::MAX, f64::MIN), |(a, b), &x| (a.min(x), b.max(x)));
        format!(" insta-buy {} · lo {} hi {} ", h.len(), fmt_coins(lo), fmt_coins(hi))
    }).unwrap_or(" history (collecting…) ".into());
    f.render_widget(Sparkline::default().data(&spark).max(100).style(Style::default().fg(Color::Green))
        .block(Block::default().borders(Borders::ALL).title(range).border_style(orange)), right[1]);

    // Log
    let h = rows[2].height.saturating_sub(2) as usize;
    let log: Vec<Line> = app.log.iter().rev().take(h).rev()
        .map(|(c, s)| Line::styled(s.clone(), Style::default().fg(*c))).collect();
    f.render_widget(Paragraph::new(log).block(Block::default().borders(Borders::ALL).title(" Blotter ").border_style(orange)), rows[2]);

    // Footer / input line
    let foot = match app.mode {
        Mode::Search => format!(" search: {}█   (Enter add · Esc cancel)", app.input),
        Mode::Qty => format!(" quantity: {}█   (Enter set · Esc cancel)", app.input),
        Mode::Normal => " ↑↓ select  b/s insta buy/sell  o buy order  p sell offer  n qty  / add  d del  x HALT  r bal  q quit".into(),
    };
    f.render_widget(Paragraph::new(foot).style(Style::default().fg(Color::Black).bg(Color::Gray)), rows[3]);
}

// ---------- main ----------

#[tokio::main]
async fn main() -> Result<()> {
    let agent_addr = std::env::args().nth(1)
        .or_else(|| std::env::var("BLOCKBERG_AGENT").ok())
        .unwrap_or(DEFAULT_AGENT.into());

    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Value>();

    // Price poller
    let ptx = tx.clone();
    tokio::spawn(async move {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).build().unwrap();
        loop {
            let _ = ptx.send(match fetch_bazaar(&client).await {
                Ok(p) => Msg::Prices(p),
                Err(e) => Msg::PriceErr(e.to_string()),
            });
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
    if agent_addr == "demo" {
        tokio::spawn(demo_agent(tx.clone(), cmd_rx));
    } else {
        tokio::spawn(agent_task(agent_addr.clone(), tx.clone(), cmd_rx));
    }

    let mut app = App {
        history: load_history(), halted: false, spent_today: 0.0, pending: None,
        products: HashMap::new(),
        watch: DEFAULT_WATCH.iter().map(|s| s.to_string()).collect(),
        table: TableState::default().with_selected(Some(0)),
        mode: Mode::Normal, input: String::new(), qty: 64,
        log: vec![(Color::Gray, "Blockberg started".into())],
        agent_addr, agent_up: false, purse: None, bank: None, last_update: None, cmd_tx,
    };

    let mut term = ratatui::init();
    let res = run(&mut term, &mut app, &mut rx).await;
    ratatui::restore();
    save_history(&app.history);
    res
}

async fn run(term: &mut ratatui::DefaultTerminal, app: &mut App, rx: &mut mpsc::UnboundedReceiver<Msg>) -> Result<()> {
    loop {
        while let Ok(m) = rx.try_recv() {
            match m {
                Msg::Prices(p) => { app.products = p; app.last_update = Some(std::time::Instant::now()); app.record_history(); }
                Msg::PriceErr(e) => app.push_log(Color::Red, format!("price feed: {e}")),
                Msg::AgentUp(up) => {
                    app.agent_up = up;
                    app.push_log(if up { Color::Green } else { Color::Red },
                        if up { "agent connected" } else { "agent disconnected" });
                }
                Msg::AgentLine(v) => app.on_agent(v),
            }
        }
        term.draw(|f| draw(f, app))?;

        if !event::poll(Duration::from_millis(200))? { continue; }
        let Event::Key(k) = event::read()? else { continue };
        if k.kind != KeyEventKind::Press { continue; }
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) { return Ok(()); }

        match app.mode {
            Mode::Normal if app.pending.is_some() => match k.code {
                KeyCode::Enter | KeyCode::Char('y') => { if let Some(o) = app.pending.take() { app.dispatch(o); } }
                _ => { app.pending = None; app.push_log(Color::Gray, "order cancelled"); }
            },
            Mode::Normal => {
                let n = app.watch.len();
                let i = app.table.selected().unwrap_or(0);
                match k.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Down | KeyCode::Char('j') if n > 0 => app.table.select(Some((i + 1) % n)),
                    KeyCode::Up | KeyCode::Char('k') if n > 0 => app.table.select(Some((i + n - 1) % n)),
                    KeyCode::Char('b') => app.request("instabuy"),
                    KeyCode::Char('s') => app.request("instasell"),
                    KeyCode::Char('o') => app.request("buyorder"),
                    KeyCode::Char('p') => app.request("selloffer"),
                    KeyCode::Char('x') => {
                        app.halted = !app.halted;
                        let m = if app.halted { "TRADING HALTED" } else { "trading resumed" };
                        app.push_log(Color::Red, m);
                    }
                    KeyCode::Char('n') => { app.mode = Mode::Qty; app.input.clear(); }
                    KeyCode::Char('/') => { app.mode = Mode::Search; app.input.clear(); }
                    KeyCode::Char('r') => { let _ = app.cmd_tx.send(json!({"cmd":"balance"})); }
                    KeyCode::Char('d') if n > 0 => {
                        app.watch.remove(i);
                        app.table.select(if app.watch.is_empty() { None } else { Some(i.min(app.watch.len() - 1)) });
                    }
                    _ => {}
                }
            }
            Mode::Search | Mode::Qty => match k.code {
                KeyCode::Esc => { app.mode = Mode::Normal; app.input.clear(); }
                KeyCode::Backspace => { app.input.pop(); }
                KeyCode::Enter => {
                    if app.mode == Mode::Search { app.add_from_search(); }
                    else if let Ok(q) = app.input.trim().parse::<u64>() { if q > 0 { app.qty = q.min(71680); } }
                    app.input.clear();
                    app.mode = Mode::Normal;
                }
                KeyCode::Char(c) => {
                    if app.mode == Mode::Search || c.is_ascii_digit() { app.input.push(c); }
                }
                _ => {}
            },
        }
    }
}

fn history_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share/blockberg/history.json"))
}

fn load_history() -> HashMap<String, Vec<f64>> {
    history_path().and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn save_history(h: &HashMap<String, Vec<f64>>) {
    if let Some(p) = history_path() {
        if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
        let _ = std::fs::write(p, serde_json::to_string(h).unwrap_or_default());
    }
}

/// `blockberg demo`: a simulated in-game agent so the terminal can be shown without Minecraft.
async fn demo_agent(tx: mpsc::UnboundedSender<Msg>, mut cmds: mpsc::UnboundedReceiver<Value>) {
    let (mut purse, bank) = (2_000_000.0f64, 25_000_000.0f64);
    let _ = tx.send(Msg::AgentUp(true));
    let _ = tx.send(Msg::AgentLine(json!({"type":"balance","purse":purse,"bank":bank})));
    while let Some(c) = cmds.recv().await {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let cmd = c["cmd"].as_str().unwrap_or("");
        let (qty, price) = (c["qty"].as_u64().unwrap_or(0) as f64, c["price"].as_f64().unwrap_or(0.0));
        let product = c["product"].as_str().unwrap_or("?");
        let reply = match cmd {
            "balance" => json!({"type":"balance","purse":purse,"bank":bank}),
            "instabuy" | "buyorder" if price * qty > purse =>
                json!({"type":"error","msg":format!("not enough coins for {product} x{qty}")}),
            "instabuy" | "buyorder" => {
                purse -= price * qty;
                json!({"type":"fill","msg":format!("[demo] bought {qty} {product} @ {price:.1}"),"cost":price*qty})
            }
            "instasell" | "selloffer" => {
                purse += price * qty;
                json!({"type":"fill","msg":format!("[demo] sold {qty} {product} @ {price:.1}")})
            }
            _ => json!({"type":"error","msg":"unknown cmd"}),
        };
        let _ = tx.send(Msg::AgentLine(reply));
        let _ = tx.send(Msg::AgentLine(json!({"type":"balance","purse":purse,"bank":bank})));
    }
}
