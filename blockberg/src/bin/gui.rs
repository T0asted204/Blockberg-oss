#![recursion_limit = "512"]
//! Blockberg: windowed trading GUI (Bloomberg look, TWS-style ticket/blotter).
//!   blockberg-gui [AGENT_ADDR | demo]     env BLOCKBERG_AGENT

#[path = "../bb.rs"]
mod bb;
#[path = "../candles.rs"]
mod candles;
#[path = "../webchart.rs"]
mod webchart;
#[path = "../fx.rs"]
mod fx;
#[path = "../server.rs"]
mod server;
#[path = "../iteminfo.rs"]
mod iteminfo;
use iteminfo::{Alert, AlertDlg, MenuAct};
#[path = "../iceberg.rs"]
mod iceberg;
#[path = "../portfolio.rs"]
mod portfolio;
#[path = "../screener.rs"]
mod screener;
#[path = "../autoflip.rs"]
mod autoflip;
#[path = "../quickbook.rs"]
mod quickbook;
use iceberg::{IceForm, IceJob, InvItem, InvSlot, SackInfo, Screen};

use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bb::*;
use eframe::egui::{self, Color32, FontId, RichText, Stroke};
use egui_extras::{Column, TableBuilder};
use serde_json::{json, Value};
use tokio::sync::mpsc;

const BG: Color32 = Color32::from_rgb(0, 0, 0);
const PANEL: Color32 = Color32::from_rgb(5, 5, 5);
const ORANGE: Color32 = Color32::from_rgb(255, 176, 0);
const GREEN: Color32 = Color32::from_rgb(0, 240, 20);
const RED: Color32 = Color32::from_rgb(255, 20, 35);
const DIM: Color32 = Color32::from_rgb(150, 150, 150);
const HISTORY_LEN: usize = 1440;

const DEFAULT_WATCH: &[&str] = &[
    "COAL", "ENCHANTED_COAL", "ENCHANTED_COAL_BLOCK", "IRON_INGOT", "ENCHANTED_IRON",
    "GOLD_INGOT", "ENCHANTED_GOLD", "DIAMOND", "ENCHANTED_DIAMOND", "COBBLESTONE",
    "ENCHANTED_COBBLESTONE", "WHEAT", "ENCHANTED_BREAD", "SUGAR_CANE", "ENCHANTED_SUGAR",
];

#[derive(PartialEq, Clone, Copy)]
enum Kind { Instant, Limit, Npc }

#[derive(Clone, PartialEq)]
enum Status { Working, Resting, Filled, Rejected(String) }

/// Independent evidence that a fill really happened.
/// verified = chat AND (orders OR inventory OR purse)
#[derive(Clone, Default)]
struct Sig { chat: bool, orders: bool, inv: Option<bool>, sacks: Option<bool>, purse: Option<bool> }

impl Sig {
    fn verified(&self) -> bool { self.chat && (self.orders || self.inv == Some(true) || self.sacks == Some(true) || self.purse == Some(true)) }
    fn merge(&mut self, v: &Value) {
        if v["chat"].as_bool() == Some(true) { self.chat = true; }
        if v["orders"].as_bool() == Some(true) { self.orders = true; }
        if let Some(b) = v["inventory"].as_bool() { self.inv = Some(self.inv.unwrap_or(false) || b); }
        if let Some(b) = v["sacks"].as_bool() { self.sacks = Some(self.sacks.unwrap_or(false) || b); }
        if let Some(b) = v["purse"].as_bool() { self.purse = Some(self.purse.unwrap_or(false) || b); }
    }
    fn describe(&self) -> String {
        let f = |b: bool| if b { "✔" } else { "✘" };
        let o = |b: Option<bool>| b.map(f).unwrap_or("–");
        format!("chat {}  ·  order menu {}  ·  inventory {}  ·  sacks {}  ·  purse {}\n✘ = checked, no change seen   – = not applicable\nverified = chat AND (order menu OR inventory OR sacks OR purse)",
            f(self.chat), f(self.orders), o(self.inv), o(self.sacks), o(self.purse))
    }
}

#[derive(Clone)]
struct Trade { time: String, side: &'static str, id: String, qty: u64, price: f64, kind: Kind, status: Status, sig: Sig }

#[derive(Clone)]
struct OrderRow { side: String, product: String, amount: Option<u64>, price: Option<f64>, filled: u64, total: u64, claimable: bool, to_claim: u64 }

#[derive(Clone)]
struct Order { cmd: &'static str, id: String, qty: u64, unit: f64, kind: Kind }

struct Gui {
    first_frame: bool,
    worth_hist: Vec<portfolio::WPoint>, worth_others: Vec<portfolio::OtherWorth>, worth_hist_at: Option<Instant>, worth_last_t: f64, worth_rec_at: Instant, worth_range: usize,
    _rt: tokio::runtime::Runtime,
    rx: mpsc::UnboundedReceiver<Msg>,
    cmd_tx: CmdTx,
    startup_synced: bool,

    products: HashMap<String, Product>,
    watch: Vec<String>,
    selected: Option<String>,
    /// per-second (unix secs, ask, bid) samples of the item on screen, for the 1s/15s/30s candles (last 3 hours)
    fine: HashMap<String, VecDeque<(f64, f64, f64)>>,
    qb_focus: Option<(String, Instant)>, qb_focus_sent: Option<String>,
    quick_book: bool, qb: quickbook::Qb, qb_main_down: Option<Instant>, qb_started: bool, qb_start_at: Instant,
    search: String,
    /// (unix secs, ask, bid)
    history: HashMap<String, Vec<(f64, f64, f64)>>,

    agent_addr: String,
    agent_up: bool,
    purse: Option<f64>,
    bank: Option<f64>,
    last_update: Option<Instant>,
    halted: bool,
    spent_today: f64,
    /// Coins NPCs paid us today (UTC day), counted from /trades sales made through the app; saved in npc_sold.json.
    npc_sold: f64, npc_sold_day: i64,

    exchange: u8, // 0 bazaar, 1 auction, 2 NPC trades
    trade_mode: u8, // quantity, fill inventory, all sacks
    inventory: HashMap<String,(u64,u64)>,
    last_inventory: Instant,
    // order ticket
    buy: bool,
    kind: Kind,
    qty: u64,
    limit_price: f64,
    limit_for: Option<(String, bool)>,
    confirm: Option<Order>,

    trades: Vec<Trade>,
    orders: Vec<OrderRow>,
    auto_claim: bool,
    watch_mode: u8,
    source_sacks: bool,
    auto_sack: bool, sack_due: Option<Instant>, sack_last: Instant,   // put claimed items into the sacks by itself
    auto_coins: bool, coins_next: Instant, server_has_coins: bool,
    manual_claiming: bool, auto_sleep: bool, agent_sleeping: bool, last_activity: Instant, activity_sent: Instant, auto_stash: bool, auto_lf: bool, stash_next: Instant, lf_next: Instant, server_has_pickup: bool, server_has_sack: bool, server_has_sleep: bool, server_has_qb: bool,
    sell_inv_confirm: Option<Vec<(String, u64, f64)>>,
    npc_calc: Option<NpcCalc>, bank_dlg: Option<BankDlg>, acct_menu: Option<egui::Pos2>, acct_menu_t: f64, pending_switch: Option<String>,
    server_has_agent_ctl: bool,   // the server knows the connect/disconnect-agent command (older ones silently ignore it)
    sync_in_mock: bool,   // tests only: let the start-up refresh run without the session counting as a real (file-writing) one
    // item info window, alerts (see iteminfo.rs)
    info_item: Option<String>, info_open: bool, info_pinned: bool, info_min: bool, info_range: u8, info_series: u8,
    info_pts: Option<((String, i64, u8), Instant, Vec<(f64, f64)>)>, info_cache: HashMap<String, (Instant, Vec<candles::Candle>)>,
    alerts: Vec<Alert>, alert_dlg: Option<AlertDlg>, watch_hover: bool,
    // server / client (see server.rs)
    mode: server::Mode,
    srv: Option<std::sync::Arc<server::Shared>>,
    op_tx: Option<mpsc::UnboundedSender<Value>>,
    link_up: std::sync::Arc<std::sync::atomic::AtomicBool>,
    link_state: String,
    remote_queue: Vec<iceberg::QRow>,
    server_restart: Option<u64>,
    server_uptime: f64,
    snap_at: Option<Instant>,
    snap_pub: Instant, snap_last: String, snap_forced: Instant, snap_seq: u64,
    restart_req: bool,
    draining: bool,
    boot: Instant,
    screen_sent: Screen,
    _lock: Option<std::net::TcpListener>,
    toasts: Vec<fx::Toast>, toast_seq: u64, act_toasts: HashMap<u64,u64>, trade_toasts: HashMap<usize,u64>, ui_time: f64,
    blotter_sel: Option<usize>,
    reduced_motion: bool,
    frames: u32,
    ice_auto_remove: bool,
    iceberg: bool,
    sugg_sel: Option<usize>, sugg_rect: Option<egui::Rect>, calc: Option<(String,String)>,
    flip_price: f64,
    wf_base: String,
    wf_ench: String,
    wf_units: u64,
    wf_steps: Vec<(String, Option<bool>, String)>,
    wf_running: bool,
    wf_result: Option<String>,
    holdings: HashMap<String, (u64, Option<u64>)>,
    claiming: std::collections::HashSet<usize>,
    last_orders_check: Instant,
    last_bank_check: Instant,
    bank_api_at: Option<Instant>,
    bank_detail: String,
    inflight: VecDeque<usize>,
    positions: HashMap<String, (i64, f64)>, // qty, avg cost
    log: Vec<(Color32, String)>,
    tab: usize,
    show_ticket: bool,
    show_bottom: bool,
    show_about: bool,
    chart_view: Option<(f64, f64)>,
    price_view: Option<(f64, f64)>,
    chart_symbol: String,
    web: Option<webchart::WebChart>, web_failed: bool, web_rect: Option<egui::Rect>, web_json: Option<(String,String)>, web_pushed: Option<(String,Instant)>, web_style: String,
    chart_minutes: f64,
    chart_ask: bool,
    chart_bid: bool,
    chart_table: bool,
    mock: bool,
    show_watch: bool,
    show_depth: bool,
    show_workflows: bool,
    chat: Vec<ChatLine>,
    chat_input: String,
    icons: Option<Icons>,
    chart_candles: bool,
    candle_tf: i64,            // minutes per candle; 0 = automatic for the chosen range
    candle_series: u8,         // 0 ask, 1 bid, 2 mid
    candle_cache: HashMap<String,(Instant,Vec<candles::Candle>)>,
    last_list_sync: Instant,
    snap_start: Instant,
    snap_sent: bool,
    // iceberg orders, queue screen, inventory screen, saved watchlist (see iceberg.rs)
    screen: Screen,
    jobs: Vec<IceJob>,
    next_job_id: u32,
    flips: Vec<autoflip::FlipJob>, next_flip_id: u32, flip_form: autoflip::FlipForm, flip_confirm: Option<autoflip::FlipJob>, flip_next: Instant, flip_saved: String, flip_log: Vec<autoflip::FlipEvent>, flip_cycles: Vec<autoflip::FlipCycle>, flip_hist: autoflip::FlipHistUi, flip_tab: u8, flip_load_dlg: bool, flip_search_open: bool, flip_filled_for: String,
    ice_form: IceForm,
    screener: screener::ScreenerState, ice_confirm: Option<IceJob>, ice_edit: Option<(u32, IceForm, bool, String)>,
    ice_last_tick: Instant,
    act_secs: HashMap<&'static str, f64>,
    act_t0: HashMap<&'static str, Instant>,
    inflight_since: Option<Instant>,
    inv_all: Vec<InvItem>,
    inv_free: Option<u64>,
    inv_slots: Vec<InvSlot>,   // what is in each storage slot (from the agent's inventory read)
    inv_all_at: Option<Instant>,
    inv_poll_at: Option<Instant>,
    inv_show_all: bool,
    inv_open: bool,
    recipe_open: std::collections::HashSet<String>,
    inv_show_sacks: bool,
    inv_search: String,
    sacks: Vec<SackInfo>,
    /// the other co-op members' orders (the order list is shared): never claimed or cancelled, but their bids and offers are not outbid / undercut
    coop_orders: Vec<OrderRow>,
    sacks_at: Option<f64>,
    sacks_scanning: bool,
    saved_view: (bool, bool, String),
    restore_claims: Vec<(String, f64)>,
    ice_saved: (String, Instant),
    ice_logged: HashMap<u32, (String, String)>,   // per iceberg job: the state (and, when paused, the reason) last written to the journal
    ice_deferred: Vec<(usize, Instant)>,   // iceberg claims held back until the next order is placed
    inv_unsupported: bool,
    orders_at: Option<Instant>,
    saved_watch: (Vec<String>, Option<String>),
}

/// The bank dialog: deposit into / withdraw from the co-op or the personal account (in game through /bank).
#[derive(Clone)]
struct BankDlg { coop: bool, deposit: bool, mode: u8, text: String }
/// The NPC sell limit calculator (right-click the NPC SOLD number).
#[derive(Clone)]
struct NpcCalc { item: String, have: String, search_open: bool }
impl BankDlg {
    /// modes: 0 everything / all of the purse, 1 half, 2 an amount, 3 deposit: keep this much in the purse, withdraw: top the purse up to this much
    fn command(&self) -> Result<Value, String> {
        let mut c = json!({"cmd":"bank","account":if self.coop{"coop"}else{"personal"},"action":if self.deposit{"deposit"}else{"withdraw"}});
        let num = || iceberg::parse_qty(&self.text).filter(|v| *v > 0).ok_or("enter an amount, e.g. 5m or 250k".to_string());
        match self.mode {
            0 => c["amount"] = json!("all"), 1 => c["amount"] = json!("half"),
            2 => c["amount"] = json!(num()?),
            _ => { if self.deposit { c["keep"] = json!(num()? as f64); } else { c["target"] = json!(num()? as f64); } }
        }
        Ok(c)
    }
    fn summary(&self) -> String {
        let acct = if self.coop { "the CO-OP bank" } else { "your PERSONAL bank" };
        let what = match self.mode { 0 => if self.deposit { "your whole purse".to_string() } else { "everything in the account".to_string() }, 1 => if self.deposit { "half your purse".to_string() } else { "half the account".to_string() },
            2 => format!("{} coins", self.text.trim()), _ => if self.deposit { format!("everything except {} coins", self.text.trim()) } else { format!("enough to have {} coins in the purse", self.text.trim()) } };
        format!("{} {} {} {}", if self.deposit { "Deposit" } else { "Withdraw" }, what, if self.deposit { "into" } else { "from" }, acct)
    }
}

/// Exact figures from the reference mockup (image 2), for layout comparison only.
fn mock_products() -> HashMap<String, Product> {
    let lv=|v:&[(f64,u64,u64)]|v.iter().map(|&(price,amount,orders)|Level{price,amount,orders}).collect::<Vec<_>>();
    let mut m=HashMap::new();
    for (id,b,a,vol) in [("COAL",1.4,5.9,888_700_000u64),("ENCHANTED_COAL",1175.7,1205.7,82_830_000),("ENCHANTED_COAL_BLOCK",24_600.0,26_500.0,267_800),
        ("IRON_INGOT",0.4,2.2,231_480_000),("ENCHANTED_IRON",384.6,418.1,6_960_000),("GOLD_INGOT",1.6,4.6,269_280_000),("ENCHANTED_GOLD",462.8,512.1,28_330_000),
        ("DIAMOND",6.1,7.2,144_970_000),("ENCHANTED_DIAMOND",1272.9,1428.8,28_310_000),("COBBLESTONE",1.3,2.5,83_480_000),("ENCHANTED_COBBLESTONE",381.5,498.0,10_550_000),
        ("WHEAT",3.3,6.0,695_730_000),("ENCHANTED_BREAD",9.2,443.0,232_400),("SUGAR_CANE",0.6,7.5,85_750_000),("ENCHANTED_SUGAR",614.6,678.8,39_790_000)] {
        let (bids,asks)=if id=="COAL" {
            (lv(&[(1.4,13_387_789,20),(1.3,143_366,2),(1.0,140_675,3),(0.9,208_437,3),(0.8,827_899,18),(0.6,200_449,3),(0.5,276_941,4)]),
             lv(&[(5.9,323,1),(6.0,127_420,3),(6.1,1_728,1),(6.2,69_952,1),(6.3,143_360,2),(6.4,55_000,1),(6.5,216_174,4),(7.1,1_984,1)]))
        } else { (lv(&[(b,1000,1)]),lv(&[(a,1000,1)])) };
        m.insert(id.to_string(),Product{sell_summary:bids,buy_summary:asks,quick_status:Quick{sell_price:b,buy_price:a,buy_moving_week:vol,sell_moving_week:0}});
    }
    m
}

fn now_secs() -> f64 { SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0) }
/// "You sold Enchanted Hard Stone x64 for 36,864 Coins!" -> 36864 (formatting codes allowed); None for any other line.
fn npc_sale_coins(line: &str) -> Option<f64> {
    let mut clean = String::new(); let mut it = line.chars();
    while let Some(c) = it.next() { if c == '§' { it.next(); } else { clean.push(c); } }
    let t = clean.trim();
    let rest = t.strip_prefix("You sold ")?;
    let (_, tail) = rest.rsplit_once(" for ")?;
    let coins = tail.strip_suffix(" Coins!")?;
    coins.replace(',', "").trim().parse::<f64>().ok().filter(|v| *v > 0.0)
}
fn clock() -> String {
    let s = now_secs() as u64 % 86400;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

impl Gui {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        tlog("window ready");
        let addr = agent_addr_from_args();
        let fake = addr == "mockup" || addr == "demo";
        // With a server running the app is only its window; otherwise it does the work itself (as it always did).
        let want_client = std::env::var("BLOCKBERG_CLIENT").is_ok();
        let mode = if std::env::var("BLOCKBERG_STANDALONE").is_ok() || (fake && !want_client) { server::Mode::Standalone } else if server::server_running() { server::Mode::Client } else { server::Mode::Standalone };
        let lock = if mode == server::Mode::Standalone && !fake {
            match server::take_engine_lock() {
                Some(l) => Some(l),
                None => { eprintln!("Another Blockberg is already running and controlling the agent (or the server is starting). Close it first, or start the server and use this window as its client."); std::process::exit(3); }
            }
        } else { None };
        let mut g = Self::with_context(&cc.egui_ctx, addr, mode);
        g._lock = lock;
        if !fake && mode == server::Mode::Standalone { server::init_journal(); }
        g
    }

    fn with_context(ctx: &egui::Context, addr: String, mode: server::Mode) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let (tx, rx) = mpsc::unbounded_channel::<Msg>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Value>();
        let qb: quickbook::Qb = Default::default();
        if mode == server::Mode::Server && bb::profile::is_main() { quickbook::spawn(qb.clone()); }

        let ptx = tx.clone();
        if addr != "mockup" { rt.spawn(async move {
            let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).build().unwrap();
            // Hypixel publishes a new bazaar snapshot about every 20 s; asking every second sees each one within ~1 s
            // (it used to be up to 10 s late). Copies already seen are dropped, so the app only redoes its work on new data.
            let mut seen = 0i64;
            loop {
                match fetch_bazaar_stamped(&client).await {
                    Ok((ts, p)) => { if ts == 0 || ts != seen { seen = ts; let _ = ptx.send(Msg::Prices(p)); } }
                    Err(e) => { let _ = ptx.send(Msg::PriceErr(e.to_string())); tokio::time::sleep(Duration::from_secs(4)).await; }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }); }
        let btx = tx.clone();
        if addr != "mockup" && addr != "demo" && mode != server::Mode::Client { rt.spawn(async move {
            let client = reqwest::Client::builder().timeout(Duration::from_secs(20)).build().unwrap();
            let mut last_err = String::new();
            loop {
                let r = fetch_bank(&client).await;
                let wait = match &r { Ok(_) => 300, Err(e) if e == "no key file" => 30, Err(_) => 120 };
                // a missing key is not an error worth reporting; anything else is, once
                match &r { Err(e) if e == "no key file" => {}, Err(e) if *e == last_err => {}, Err(e) => { last_err = e.clone(); let _ = btx.send(Msg::Bank(r.clone())); }, Ok(_) => { last_err.clear(); let _ = btx.send(Msg::Bank(r.clone())); } }
                tokio::time::sleep(Duration::from_secs(wait)).await;
            }
        }); }
        let mock = addr == "mockup";
        if mock { let _ = tx.send(Msg::Prices(mock_products())); let _ = tx.send(Msg::AgentUp(true)); let _ = tx.send(Msg::AgentLine(json!({"type":"balance","purse":831_000.0,"bank":1_760_000.0})));
            if std::env::var("BLOCKBERG_MOCK_INV").is_ok() { let _ = tx.send(Msg::AgentLine(json!({"type":"invall","free_slots":23,"items":[{"id":"COAL","count":296,"fits":1000},{"id":"SEEDS","count":128,"fits":1000},{"id":"ENCHANTED_COAL","count":12,"fits":1000}],
                "slots":[{"slot":0,"id":"ASPECT_OF_THE_END","count":1},{"slot":1,"id":"COAL","count":64},{"slot":2,"id":"COAL","count":64},{"slot":3,"id":"SEEDS","count":64},{"slot":8,"id":"SKYBLOCK_MENU","count":1},
                         {"slot":9,"id":"COAL","count":64},{"slot":10,"id":"SEEDS","count":64},{"slot":11,"id":"ENCHANTED_COAL","count":12},{"slot":18,"id":"COAL","count":40},{"slot":27,"id":"HARD_STONE","count":1728}]}))); } }
        else if mode == server::Mode::Client { drop(cmd_rx); }   // the server owns the agent connection
        else if addr == "demo" { rt.spawn(demo_agent(tx.clone(), cmd_rx)); }
        else { rt.spawn(agent_task(addr.clone(), tx.clone(), cmd_rx)); }
        // server: listen for apps; client: connect to the server
        let link_up = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (mut srv, mut op_tx, mut cmd_tx) = (None, None, CmdTx::new(cmd_tx));
        match mode {
            server::Mode::Server => {
                let secret = server::load_secret(true).expect("cannot create the shared secret file");
                let shared = std::sync::Arc::new(server::Shared { snap: std::sync::Mutex::new((0, String::new())), clients: std::sync::atomic::AtomicUsize::new(0) });
                rt.spawn(server::server_task(secret, shared.clone(), tx.clone()));
                srv = Some(shared);
            }
            server::Mode::Client => {
                let secret = server::load_secret(false).unwrap_or_default();
                let (otx, orx) = mpsc::unbounded_channel::<Value>();
                rt.spawn(server::client_task(secret, tx.clone(), orx, link_up.clone()));
                cmd_tx = CmdTx::new_remote(otx.clone(), link_up.clone());
                op_tx = Some(otx);
            }
            server::Mode::Standalone => {}
        }

        tlog("runtime+tasks up");
        setup_style(ctx);
        tlog("style+fonts");
        let watch: Vec<String> = DEFAULT_WATCH.iter().map(|s| s.to_string()).collect();
        let mut g = Gui {
            first_frame: false, worth_hist: vec![], worth_others: vec![], worth_hist_at: None, worth_last_t: 0.0, worth_rec_at: Instant::now() - Duration::from_secs(40), worth_range: 1, fine: HashMap::new(), qb_focus: None, qb_focus_sent: None, quick_book: false, qb: qb.clone(), qb_main_down: None, qb_started: false, qb_start_at: Instant::now(), _rt: rt, rx, cmd_tx, startup_synced: false,
            products: HashMap::new(), selected: watch.first().cloned(), watch, search: String::new(),
            history: if mock { let t=now_secs(); HashMap::from([("COAL".to_string(),(0..=58).map(|i|(t-1740.0+i as f64*30.0,5.9,1.4)).collect())]) } else { load_history() },
            agent_addr: if mock { DEFAULT_AGENT.into() } else { addr }, agent_up: false, purse: None, bank: None, last_update: None,
            halted: false, spent_today: 0.0, npc_sold: 0.0, npc_sold_day: 0,
            exchange: 0, trade_mode: 0, inventory: HashMap::new(), last_inventory: Instant::now()-Duration::from_secs(5),
            buy: true, kind: Kind::Instant, qty: 1, limit_price: 0.0, limit_for: None, confirm: None,
            npc_calc: None, bank_dlg: None, acct_menu: None, acct_menu_t: 0.0, pending_switch: None, trades: vec![], orders: vec![], coop_orders: vec![], auto_claim: true, watch_mode: 0, source_sacks: true, auto_coins: true, coins_next: Instant::now() + Duration::from_secs(10 * 60), server_has_coins: false, manual_claiming: false, auto_sleep: false, server_has_sleep: false, server_has_qb: false, agent_sleeping: false, last_activity: Instant::now(), activity_sent: Instant::now() - Duration::from_secs(60), auto_stash: false, auto_lf: false, auto_sack: false, sack_due: None, sack_last: Instant::now() - Duration::from_secs(3600), stash_next: Instant::now() + Duration::from_secs(20 * 60), lf_next: Instant::now() + Duration::from_secs(20 * 60), server_has_pickup: false, server_has_sack: false, server_has_agent_ctl: false, sell_inv_confirm: None, sync_in_mock: false,
            info_item: None, info_open: false, info_pinned: false, info_min: false, info_range: 0, info_series: 0, info_pts: None, info_cache: HashMap::new(),
            alerts: vec![], alert_dlg: None, watch_hover: false,
            mode, srv, op_tx, link_up, link_state: String::new(), remote_queue: vec![], server_restart: None, server_uptime: 0.0, snap_at: None,
            snap_pub: Instant::now(), snap_last: String::new(), snap_forced: Instant::now(), snap_seq: 0, restart_req: false, draining: false, boot: Instant::now(), screen_sent: Screen::Main, _lock: None,
            toasts: vec![], toast_seq: if mode==server::Mode::Client {1u64<<40} else {0}, act_toasts: HashMap::new(), trade_toasts: HashMap::new(), ui_time: 0.0, blotter_sel: None, reduced_motion: fx::system_prefers_reduced(), frames: 0, ice_auto_remove: true, iceberg: std::env::var("BLOCKBERG_ICE_TICKET").is_ok(), sugg_sel: None, sugg_rect: None, calc: None, flip_price: 0.0, wf_base: "SEEDS".into(), wf_ench: "ENCHANTED_SEEDS".into(), wf_units: 160, wf_steps: vec![], wf_running: false, wf_result: None, holdings: HashMap::new(), claiming: Default::default(), last_orders_check: Instant::now(), last_bank_check: Instant::now(), bank_api_at: None, bank_detail: String::new(), inflight: VecDeque::new(), positions: HashMap::new(),
            log: vec![(DIM, format!("{}  Blockberg started", clock()))],
            tab: 0, show_ticket: true, show_bottom: true, show_about: false, chart_view: None, price_view: None, chart_symbol: String::new(), web: None, web_failed: std::env::var("BLOCKBERG_NOWEB").is_ok(), web_rect: None, web_json: None, web_pushed: None, web_style: "candles".into(), chart_minutes: 43200.0, chart_ask: true, chart_bid: true, chart_table: false, mock,
            show_watch: true, show_depth: true, show_workflows: false, chat: vec![], chat_input: String::new(),
            icons: load_icons(ctx),
            chart_candles: std::env::var("BLOCKBERG_CANDLES").is_ok(), candle_tf: 0, candle_series: 1, candle_cache: HashMap::new(), last_list_sync: Instant::now(), snap_start: Instant::now(), snap_sent: false,
            screen: Screen::Main, jobs: vec![], next_job_id: 1, flips: vec![], next_flip_id: 1, flip_form: autoflip::FlipForm::new(), flip_confirm: None, flip_next: Instant::now(), flip_saved: String::new(), flip_log: vec![], flip_cycles: vec![], flip_hist: Default::default(), flip_tab: std::env::var("BLOCKBERG_FLIP_TAB").ok().and_then(|v| v.parse().ok()).unwrap_or(0), flip_load_dlg: false, flip_search_open: false, flip_filled_for: String::new(), ice_form: IceForm::new(), screener: Default::default(), ice_confirm: None, ice_edit: None, ice_last_tick: Instant::now(),
            act_secs: HashMap::new(), act_t0: HashMap::new(), inflight_since: None, inv_all: vec![], inv_free: None, inv_slots: vec![], inv_all_at: None, inv_poll_at: None,
            inv_show_all: false, inv_open: std::env::var("BLOCKBERG_INV_OPEN").is_ok(), recipe_open: Default::default(), inv_show_sacks: false, inv_search: String::new(), sacks: vec![], sacks_at: None, sacks_scanning: false, saved_view: (false, false, String::new()), ice_deferred: vec![], ice_logged: HashMap::new(), restore_claims: vec![], ice_saved: (String::new(), Instant::now()), inv_unsupported: false, orders_at: None, saved_watch: (vec![], None),
        };
        tlog("struct built (history+icons)");
        if !mock {
            // the watchlist and any iceberg orders come back the way they were left
            if let Some((list, sel)) = iceberg::load_watch() {
                g.selected = sel.filter(|s| list.contains(s)).or_else(|| list.first().cloned());
                g.watch = list;
            }
            g.saved_watch = (g.watch.clone(), g.selected.clone());
            if g.is_engine() {
                let (jobs, next, claims) = Gui::ice_load();
                g.jobs = jobs; g.next_job_id = next; g.restore_claims = claims;
                { let (f, n, l, c) = Gui::flip_load(); g.flips = f; g.next_flip_id = n; g.flip_log = l; g.flip_cycles = c; }
                g.ice_saved = (g.ice_json(false), Instant::now());
                g.load_sacks();
            }
            g.load_inv_view();
            g.alerts = iteminfo::load_alerts();
        }
        g.load_layout();
        // Debug aid for screenshots: BLOCKBERG_SCREEN=queue|inventory opens that screen at start.
        // BLOCKBERG_DEMO_ICE=SEEDS starts a demo iceberg (with the `demo` agent) for screenshots.
        if let Ok(p) = std::env::var("BLOCKBERG_DEMO_ICE") { g.ice_add(IceJob::new(0, p, 300_000, 40_000, 0.1, 50.0, 0.1, 300)); }
        g.load_npc_sold();
        if let Ok(i) = std::env::var("BLOCKBERG_NPC_CALC") { g.npc_calc = Some(NpcCalc { item: i, have: "3000".into(), search_open: false }); }
        if let Ok(r) = std::env::var("BLOCKBERG_RECIPE_OPEN") { for id in r.split(',') { g.recipe_open.insert(id.to_string()); } }
        if let Ok(id) = std::env::var("BLOCKBERG_DEMO_INFO") { g.info_item = Some(id); g.info_open = true; g.info_pinned = true; }
        match std::env::var("BLOCKBERG_SCREEN").as_deref() { Ok("queue") => g.screen = Screen::Queue, Ok("inventory") => g.screen = Screen::Inventory, Ok("portfolio") => g.screen = Screen::Portfolio, Ok("screener") => g.screen = Screen::Screener, Ok("autoflip") => g.screen = Screen::AutoFlip, _ => {} }
        candles::ensure_in_list(&g.watch);
        tlog("new() done");
        g
    }

    #[allow(dead_code)]
    /// Ticker headlines built from locally recorded price history (last 24 h).
    fn headlines(&self) -> Vec<(String, String)> {
        if self.mock { return vec![("03:24".into(),"Global Coal Prices Jump 76% on Supply Constraints".into()),("03:18".into(),"Ench. Coal Block Volumes Up 7.2%".into()),("02:55".into(),"Commodities Mixed as Demand Data Releases".into())]; }
        let hhmm=|t:f64|{let s=t as u64%86400;format!("{:02}:{:02}",s/3600,s/60%60)};
        let now=now_secs();
        let mut moves=Vec::new();
        let (mut up,mut down)=(0,0);
        for id in &self.watch {
            let Some(h)=self.history.get(id) else{continue};
            let w:Vec<_>=h.iter().filter(|r|now-r.0<=86400.0&&r.2>0.0).collect();
            if w.len()<2{continue}
            let (first,last)=(w[0].2,w[w.len()-1].2);
            let pct=(last-first)/first*100.0;
            if pct>0.05{up+=1}else if pct< -0.05{down+=1}
            // when the biggest single jump happened
            let at=w.windows(2).max_by(|a,b|(a[1].2-a[0].2).abs().total_cmp(&(b[1].2-b[0].2).abs())).map(|p|p[1].0).unwrap_or(w[w.len()-1].0);
            moves.push((pct,at,id.clone()));
        }
        moves.sort_by(|a,b|b.0.abs().total_cmp(&a.0.abs()));
        let mut out:Vec<(f64,String,String)>=moves.iter().filter(|m|m.0.abs()>=1.0).take(2).enumerate().map(|(i,(pct,at,id))|{
            let n=pretty_s(id);
            let text=match (i,*pct>0.0) {
                (0,true)=>format!("Global {n} Prices Jump {:.0}% on Bazaar",pct),
                (0,false)=>format!("Global {n} Prices Slide {:.0}% on Bazaar",-pct),
                (_,true)=>format!("{n} Bids Up {:.1}%",pct),
                (_,false)=>format!("{n} Bids Down {:.1}%",-pct),
            };
            (*at,hhmm(*at),text)
        }).collect();
        if up+down>0 {
            let mood=if up>0&&down>0{"Mixed"}else if up>0{"Higher"}else{"Lower"};
            let t=moves.iter().map(|m|m.1).fold(0.0,f64::max);
            out.push((t,hhmm(t),format!("Commodities {mood} as {up} Rise, {down} Fall")));
        }
        out.into_iter().map(|(_,t,m)|(t,m)).collect()
    }

    fn say(&mut self, c: Color32, s: impl Into<String>) {
        self.log.push((c, format!("{}  {}", clock(), short(&s.into()))));
        if self.log.len() > 300 { self.log.remove(0); }
    }

    /// Everything that makes the engine work, with or without a window. A client only listens to the server.
    fn engine_tick(&mut self) {
        self.poll();
        self.fine_tick();
        if self.is_client() { self.client_tick(); return; }
        self.ice_tick();
        self.flip_tick();
        if !self.draining { self.inv_tick(); }
        self.toasts_derive();
        // a claim whose answer never came (the agent was restarted, say) must not stay "running" for ever: it would hold up a server restart
        if !self.claiming.is_empty() && !self.busy(&["claimorder"]) { self.claiming.clear(); }
        self.auto_pickup_tick();
        self.auto_coins_tick();
        self.auto_sack_tick();
        self.sleep_tick();
        self.qb_tick();
        self.escrow_refresh_tick();
        self.ice_auto_resume_tick();
        self.worth_tick();
        // While limit orders rest, look at the real order menu every 30 s.
        let resting = self.trades.iter().any(|t| t.status == Status::Resting);
        if self.agent_up && !self.halted && self.inflight.is_empty() && !self.draining && resting && self.last_orders_check.elapsed() > Duration::from_secs(30) {
            let _ = self.cmd_tx.send(json!({"cmd":"orders","auto":true}));
            self.last_orders_check = Instant::now();
        }
    }

    /// The price a sell offer should have to be the cheapest on the bazaar: the lowest current offer minus 0.1 (never below 0.1).
    fn undercut_price(&self, id: &str) -> Option<f64> {
        let ask = self.products.get(id)?.ask();
        if ask <= 0.0 { return None; }
        Some((((ask - 0.1) * 10.0).round() / 10.0).max(0.1))
    }

    /// What "sell inventory to the bazaar" would list, from the last inventory read: (item, how many, price each).
    fn sell_inv_preview(&self) -> Vec<(String, u64, f64)> {
        self.inv_all.iter().filter(|i| i.count > 0).filter_map(|i| self.undercut_price(&i.id).map(|p| (i.id.clone(), i.count, p))).collect()
    }

    fn send_sell_inventory(&mut self) {
        let prices: serde_json::Map<String, Value> = self.products.keys().filter_map(|id| self.undercut_price(id).map(|p| (id.clone(), json!(p)))).collect();
        let _ = self.cmd_tx.send(json!({"cmd": "sellinv", "prices": prices}));
    }

    /// After a claim (when the switch is on): put what is in the inventory into the sacks, so the next claim has room
    /// and nothing spills into the stash. The game does not do this by itself for bazaar claims.
    /// When the claims or cancels the user started (Claim all, Claim coins, Cancel all, a row's Claim or Cancel) have all finished, read the real order menu once.
    fn escrow_refresh_tick(&mut self) {
        let now_claiming = self.cmd_tx.log.lock().map_or(false, |l| l.iter().any(|a| !a.auto && matches!(a.cmd.as_str(), "claimorder" | "claim" | "cancel") && a.done.is_none()));
        if self.manual_claiming && !now_claiming && self.agent_up && !self.mock {
            let _ = self.cmd_tx.send(json!({"cmd":"orders"})); self.orders_at = Some(Instant::now());
        }
        self.manual_claiming = now_claiming;
    }
    /// The user did something: restart the agent if it was put to sleep for being idle.
    pub fn note_activity(&mut self) {
        self.last_activity = Instant::now();
        if self.auto_sleep && self.agent_sleeping && !self.agent_up {
            self.agent_sleeping = false; self.save_layout();
            self.say(ORANGE, "waking the agent: you are back");
            self.agent_service_local("start");
        }
    }
    /// With "disconnect when idle" on: after 20 minutes without any activity and with nothing running, stop the desktop Minecraft.
    fn sleep_tick(&mut self) {
        if self.is_client() || self.mock || !self.auto_sleep { return; }
        if self.agent_sleeping {
            if self.agent_up { self.agent_sleeping = false; self.save_layout(); }
            else if self.jobs.iter().any(|j| j.state.live()) || self.flips.iter().any(|f| f.state == autoflip::FlipState::Running) { self.note_activity(); }   // an iceberg or an auto flip needs the agent
            return;
        }
        if !self.agent_up { return; }
        if !bb::profile::is_main() && quickbook::spare_in_use() { self.last_activity = Instant::now(); return; }   // the main account is reading live order books through this agent
        // anything the agent is doing right now (a sell-inventory run, a claim, a bank move...) keeps it awake, and so does a command the user sent in the last 20 minutes
        // only what the user queued by hand counts: the app's own checks (order / inventory reads, auto-claims, pickups, an auto flip's steps) are flagged `auto` and must not keep the agent awake
        let pending = self.cmd_tx.log.lock().map_or(false, |l| l.iter().any(|a| a.done.is_none() && !a.auto));
        let user_cmd = bb::LAST_USER_CMD.load(std::sync::atomic::Ordering::Relaxed);
        let user_ago = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()).saturating_sub(user_cmd);
        if user_cmd > 0 && user_ago < 20 * 60 { let t = Instant::now() - Duration::from_secs(user_ago); if t > self.last_activity { self.last_activity = t; } }
        let busy = !self.inflight.is_empty() || !self.claiming.is_empty() || self.draining || pending
            || self.jobs.iter().any(|j| j.state.live())
            || self.flips.iter().any(|f| f.state == autoflip::FlipState::Running);   // a running auto flip needs the agent
        if busy { self.last_activity = Instant::now(); return; }
        if self.last_activity.elapsed() > Duration::from_secs(20 * 60) {
            self.agent_sleeping = true; self.save_layout();
            self.say(ORANGE, "idle for 20 minutes: disconnecting the agent (it reconnects when you do something)");
            self.agent_service_local("stop");
        }
    }
    fn auto_sack_tick(&mut self) {
        let Some(due) = self.sack_due else { return };
        if !self.auto_sack { self.sack_due = None; return; }
        if due > Instant::now() { return; }
        let busy = !self.inflight.is_empty() || self.draining || self.halted || !self.agent_up || self.busy(&["insert_sacks", "claimorder", "pickup"])
            || self.jobs.iter().any(|j| matches!(j.state, iceberg::JobState::Placing | iceberg::JobState::Requoting | iceberg::JobState::Adopting));
        if busy { self.sack_due = Some(Instant::now() + Duration::from_secs(10)); return; }   // look again shortly
        if self.sack_last.elapsed() < Duration::from_secs(20) { self.sack_due = Some(self.sack_last + Duration::from_secs(20)); return; }
        self.sack_due = None; self.sack_last = Instant::now();
        let _ = self.cmd_tx.send(json!({"cmd":"insert_sacks","auto":true}));
    }

    /// Every 20 minutes (each one only when its own switch is on): take what is in the stash, and what is in Lost & Found.
    fn auto_pickup_tick(&mut self) {
        let due_stash = self.auto_stash && self.stash_next <= Instant::now();
        let due_lf = self.auto_lf && self.lf_next <= Instant::now();
        if !due_stash && !due_lf { return; }
        // not in the middle of the iceberg's own placing / requoting, and not while the server is draining for a restart
        let busy = !self.inflight.is_empty() || self.draining || self.halted || !self.agent_up || self.busy(&["pickup"])
            || self.jobs.iter().any(|j| matches!(j.state, iceberg::JobState::Placing | iceberg::JobState::Requoting | iceberg::JobState::Adopting) || (j.state == iceberg::JobState::Resting && self.ice_requote_in(j).map_or(false, |t| t < 160.0)));   // a pickup can take 2 minutes: it must not make a requote wait
        if busy { if due_stash { self.stash_next = Instant::now() + Duration::from_secs(60); } if due_lf { self.lf_next = Instant::now() + Duration::from_secs(60); } return; }   // look again in a minute
        if due_stash { self.stash_next = Instant::now() + Duration::from_secs(20 * 60); let _ = self.cmd_tx.send(json!({"cmd":"pickup","auto":true,"stash":true,"lost_found":false,"max_secs":150})); }
        if due_lf { self.lf_next = Instant::now() + Duration::from_secs(20 * 60); let _ = self.cmd_tx.send(json!({"cmd":"pickup","auto":true,"stash":false,"lost_found":true})); }
    }

    /// Every 10 minutes: claim the coins your filled sell offers left in escrow (only when a sell offer is known, and never while the agent is busy).
    fn auto_coins_tick(&mut self) {
        if !self.auto_coins || self.coins_next > Instant::now() { return; }
        let busy = !self.inflight.is_empty() || self.draining || self.halted || !self.agent_up || self.busy(&["claim", "claimorder", "pickup", "fliporder", "requote_sell"])
            || self.flips.iter().any(|f| f.pending.is_some())
            || self.jobs.iter().any(|j| matches!(j.state, iceberg::JobState::Placing | iceberg::JobState::Requoting | iceberg::JobState::Adopting));
        if busy { self.coins_next = Instant::now() + Duration::from_secs(45); return; }   // look again soon
        self.coins_next = Instant::now() + Duration::from_secs(10 * 60);
        if !self.orders.iter().any(|r| r.side == "SELL") { return; }   // no sell offer, so nothing to collect
        let _ = self.cmd_tx.send(json!({"cmd":"claim","auto":true}));
        let _ = self.cmd_tx.send(json!({"cmd":"orders","auto":true}));   // the list drops what was claimed
        self.orders_at = Some(Instant::now());
        self.say(DIM, "auto-claim: collecting coins from filled sell offers");
    }

    /// One turn of the headless server loop.
    fn server_tick(&mut self) {
        self.ui_time = self.boot.elapsed().as_secs_f64();
        self.engine_tick();
        self.publish_snapshot();
        self.restart_check();
    }

    /// The app, connected to a server: tell it which screen is showing (it polls what that screen needs).
    fn client_tick(&mut self) {
        {   // a seconds chart is open: ask the server to have the spare account watch just that item, as fast as it can
            let f = if self.candle_tf < 0 { self.selected.clone() } else { None };
            // re-sent every 5 s while it applies (the server forgets it after 15 s, so a closed window can't leave it stuck)
            let due = f != self.qb_focus_sent || (f.is_some() && self.qb_focus.as_ref().map_or(true, |x| x.1.elapsed() > Duration::from_secs(5)));
            if due && self.link_up.load(std::sync::atomic::Ordering::Relaxed) && self.remote_op(json!({"op":"qb_focus","v":f})) { self.qb_focus = f.clone().map(|x| (x, Instant::now())); self.qb_focus_sent = f; }
        }
        if self.screen != self.screen_sent && self.link_up.load(std::sync::atomic::Ordering::Relaxed) {
            let s = match self.screen { Screen::Inventory => "inventory", Screen::Queue => "queue", Screen::Portfolio => "portfolio", Screen::Screener => "screener", Screen::AutoFlip => "autoflip", Screen::Main => "main" };
            if self.remote_op(json!({"op":"screen","v":s})) { self.screen_sent = self.screen; }
        }
    }

    fn poll(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Prices(p) => { if self.last_update.is_none() { tlog("first prices"); }
                    self.products = p;
                    quickbook::overlay(&self.qb, &mut self.products);
                    self.check_alerts();
                    self.last_update = Some(Instant::now());
                    let t = now_secs();
                    for id in self.watch.clone() {
                        if let Some(p) = self.products.get(&id) {
                            let h = self.history.entry(id).or_default();
                            h.push((t, p.ask(), p.bid()));
                            if h.len() > HISTORY_LEN { h.remove(0); }
                        }
                    }
                }
                Msg::PriceErr(e) => self.say(RED, format!("price feed: {e}")),
                Msg::AgentUp(up) => {
                    let was_up = self.agent_up;
                    self.agent_up = up;
                    server::journal_event(json!({"ev": "agent", "up": up, "was_up": was_up, "pending": self.cmd_tx.log.lock().map(|l| l.iter().filter(|a| a.done.is_none()).map(|a| a.label.clone()).collect::<Vec<_>>()).unwrap_or_default(),
                        "jobs": self.jobs.iter().map(|j| json!({"id": j.id, "state": j.state.label()})).collect::<Vec<_>>()}));
                    self.say(if up { GREEN } else { RED }, if up { "agent connected" } else { "agent disconnected" });
                    if !up { self.sacks_scanning = false; }   // a scan in flight dies with the agent: let the button work again
                    if !up && was_up { self.ice_agent_lost(); }
                    if !up && was_up {
                        // anything the agent was still doing is lost with it: say so at once (it may have stopped half-way)
                        let lost: Vec<String> = self.cmd_tx.log.lock().map(|mut l| l.iter_mut().filter(|a| a.done.is_none()).map(|a| { a.done = Some((Instant::now(), Some("agent connection lost".into()))); a.label.clone() }).collect()).unwrap_or_default();
                        if !lost.is_empty() {
                            let what = if lost.len() == 1 { lost[0].clone() } else { format!("{} and {} more", lost[0], lost.len() - 1) };
                            let _ = self.toast_new(2, format!("Agent connection lost while: {what}. It may have stopped part-way: check before repeating it."));
                        } else { let _ = self.toast_new(2, "Agent connection lost"); }
                    }
                    if up {
                        self.inv_unsupported = false; self.inv_poll_at = None;   // the agent may have been restarted with a newer version
                        let _ = self.cmd_tx.send(json!({"cmd":"balance"}));   // purse only: reads the scoreboard, opens no menu
                        let _ = self.cmd_tx.send(json!({"cmd":"subscribe_chat"}));
                        self.startup_sync();
                    }
                }
                Msg::AgentLine(v) => self.on_agent(v),
                Msg::Bank(Ok((b, p, detail))) => { self.bank = Some(b); self.bank_api_at = Some(Instant::now()); if self.purse.is_none() { self.purse = p; } if self.bank_detail != detail { self.say(DIM, format!("bank via Hypixel API: {detail}")); self.bank_detail = detail; } }
                Msg::Op(v) => self.handle_op(v),
                Msg::Snap(v) => { if self.snap_at.is_none() { tlog("first server snapshot"); } self.apply_snapshot(&v) }
                Msg::Link(s) => { if s == "connected" { self.screen_sent = Screen::Main; } self.link_state = s; }
                Msg::Bank(Err(e)) => self.say(ORANGE, format!("bank via Hypixel API: {e} (falling back to /bank in game)")),
            }
        }
    }

    /// Book a fill once: mark the trade, update spend and positions.
    fn apply_fill(&mut self, i: usize, cost: Option<f64>) {
        if self.trades[i].status == Status::Filled { return; }
        self.trades[i].status = Status::Filled;
        let t = self.trades[i].clone();
        let buy = t.side == "BUY";
        let value = t.price * t.qty as f64;
        if buy { self.spent_today += cost.unwrap_or(value); }
        let e = self.positions.entry(t.id.clone()).or_insert((0, 0.0));
        if buy {
            let tot = e.1 * e.0 as f64 + value;
            e.0 += t.qty as i64;
            e.1 = if e.0 != 0 { tot / e.0 as f64 } else { 0.0 };
        } else {
            e.0 -= t.qty as i64;
            if e.0 == 0 { e.1 = 0.0; }
        }
    }

    /// Resting/filled limit trade for this product and side, newest first.
    fn find_limit(&self, product_lower: &str, side: Option<&str>, resting_only: bool) -> Option<usize> { self.find_limit_at(product_lower, side, resting_only, None) }
    /// Same, but when the order's price is known only an order at that price matches: with two orders of one item open
    /// (a finished slice next to the new one), "the newest" is the wrong one and books a fill that never happened.
    fn find_limit_at(&self, product_lower: &str, side: Option<&str>, resting_only: bool, price: Option<f64>) -> Option<usize> {
        (0..self.trades.len()).rev().find(|&i| {
            let t = &self.trades[i];
            t.kind == Kind::Limit
                && (t.status == Status::Resting || (!resting_only && t.status == Status::Filled))
                && pretty(&t.id).to_lowercase() == product_lower
                && side.map_or(true, |s| t.side == s)
                && price.map_or(true, |p| (t.price - p).abs() < 0.05)
        })
    }

    fn claim_trade(&mut self, i: usize) {
        let job = self.jobs.iter().find(|j| j.cur_trade == Some(i)).map(|j| (j.id, j.auto_claim));
        let ice = job.is_some();
        if let Some((id, false)) = job {
            // this iceberg was started without auto-claim: the filled slice stays in escrow for the user
            self.say(DIM, format!("iceberg #{id}: slice left in escrow (auto-claim is off for it)"));
            return;
        }
        if !(self.auto_claim || ice) { return; }
        if ice {
            // the next order goes out first, then this claim (see ice_flush_claims): no downtime waiting on a long claim
            if !self.ice_deferred.iter().any(|d| d.0 == i) && !self.claiming.contains(&i) {
                self.ice_deferred.push((i, Instant::now()));
                self.say(ORANGE, "claim queued: the next iceberg order is placed first");
            }
            return;
        }
        if !self.claiming.insert(i) { return; }
        let t = self.trades[i].clone();
        let _ = self.cmd_tx.send(json!({"cmd":"claimorder","auto":true,"product":t.id,"side":t.side,"price":t.price,"sack_on_full":self.auto_sack}));
        self.say(ORANGE, format!("→ claim {} {}", t.side, pretty_s(&t.id)));
    }

    fn utc_day() -> i64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| (d.as_secs() / 86400) as i64) }
    fn npc_sold_path() -> std::path::PathBuf { bb::profile::data_dir().join("npc_sold.json") }
    /// What NPCs paid us so far today (0 once the UTC day has rolled over).
    pub fn npc_sold_today(&self) -> f64 { if self.npc_sold_day == Self::utc_day() { self.npc_sold } else { 0.0 } }
    fn load_npc_sold(&mut self) {
        if self.mock { return; }
        if let Ok(t) = std::fs::read_to_string(Self::npc_sold_path()) { if let Ok(v) = serde_json::from_str::<Value>(&t) { self.npc_sold_day = v["day"].as_i64().unwrap_or(0); self.npc_sold = v["total"].as_f64().unwrap_or(0.0); } }
    }
    fn note_npc_sold(&mut self, gained: f64) {
        if !(gained > 0.0) || self.is_client() { return; }
        let today = Self::utc_day();
        if self.npc_sold_day != today { self.npc_sold_day = today; self.npc_sold = 0.0; }
        self.npc_sold += gained;
        if self.mock { return; }   // tests and the mock-up never touch the real count
        let p = Self::npc_sold_path();
        if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
        let _ = std::fs::write(p, json!({"day": self.npc_sold_day, "total": self.npc_sold}).to_string());
    }

    fn on_agent(&mut self, v: Value) {
        server::journal_reply(&v);
        // NPC sales are counted from the game's own chat line ("You sold <item> x64 for 36,864 Coins!"), so a lost reply (a restart in the middle of a sale) cannot lose one,
        // and sales you make by hand count too
        if v["type"].as_str() == Some("chat") { if let Some(c) = npc_sale_coins(v["text"].as_str().unwrap_or("")) { self.note_npc_sold(c); } }
        self.act_complete(&v);
        match v["type"].as_str().unwrap_or("log") {
            "bank" => {
                let msg = v["msg"].as_str().unwrap_or("bank done").to_string();
                self.say(GREEN, format!("✓ {msg}")); let _ = self.toast_new(1, msg);
                if v["account"].as_str() == Some("coop") { if let Some(b) = v["balance"].as_f64() { self.bank = Some(b); } }
                if let Some(p) = v["purse"].as_f64() { self.purse = Some(p); }   // the purse as it is right now, not at the next balance poll   // the game says what is in the account now
                self.last_bank_check = Instant::now() - Duration::from_secs(3600);   // read the bank balance again soon
            }
            "insert_sacks" => {
                let n = v["moved"].as_u64().unwrap_or(0);
                self.say(DIM, format!("inventory into sacks: {n} item(s) moved"));
                if self.agent_up { let _ = self.cmd_tx.send(json!({"cmd":"invall","auto":true})); }   // show what is left in the inventory now
                if n > 0 { let _ = self.toast_new(1, format!("{} item(s) moved from your inventory into your sacks", grouped(n))); }
                else {
                    // say why: which of the items you are carrying no sack of yours holds (from the last sack scan), or whether the sacks are full
                    let held: Vec<&InvItem> = self.inv_all.iter().filter(|i| i.count > 0 && i.id != "SKYBLOCK_MENU").collect();
                    let (ok, no): (Vec<&InvItem>, Vec<&InvItem>) = held.into_iter().partition(|i| self.sack_row(&pretty(&i.id)).is_some());
                    let names = |v: &[&InvItem]| v.iter().take(6).map(|i| pretty_s(&i.id)).collect::<Vec<_>>().join(", ");
                    let why = if self.sacks.is_empty() { "scan your sacks (Inventory screen) so I can say why".to_string() }
                        else if ok.is_empty() && !no.is_empty() { format!("none of your sacks takes: {}", names(&no)) }
                        else if !ok.is_empty() { format!("your sacks look full for: {}", names(&ok)) }
                        else { "your inventory has nothing to insert".to_string() };
                    let _ = self.toast_new(2, format!("Nothing moved: {why}"));
                }
            }
            "sellinv" => {
                let n = |k: &str| v[k].as_array().map_or(0, |a| a.len());
                let (placed, failed) = (n("placed"), n("failed"));
                if let Some(a) = v["placed"].as_array() { for p in a { self.say(GREEN, format!("sell offer: {} x{} @ {:.1}", pretty_s(p["id"].as_str().unwrap_or("")), grouped(p["qty"].as_u64().unwrap_or(0)), p["price"].as_f64().unwrap_or(0.0))); } }
                if let Some(a) = v["failed"].as_array() { for f in a { self.say(RED, format!("sell offer failed: {}: {}", pretty_s(f["id"].as_str().unwrap_or("")), f["err"].as_str().unwrap_or(""))); } }
                if placed == 0 && failed == 0 { let _ = self.toast_new(2, "Nothing to sell: no bazaar items in the inventory"); }
                else if failed == 0 { let _ = self.toast_new(1, format!("{placed} sell offer(s) placed at best offer − 0.1")); }
                else { let _ = self.toast_new(2, format!("{placed} sell offer(s) placed, {failed} failed (see the log)")); }
            }
            "pickup" => {
                let (s, l) = (v["stash"].as_str().unwrap_or(""), v["lost_found"].as_str().unwrap_or(""));
                self.say(DIM, format!("stash: {s}  ·  Lost & Found: {l}"));
                let took = |t: &str| t.starts_with("took");
                if took(s) || took(l) { let _ = self.toast_new(1, format!("Picked up: {}", [s, l].iter().filter(|t| took(t)).cloned().collect::<Vec<_>>().join(" · "))); }
                else if s.contains("full") || l.contains("full") { let _ = self.toast_new(2, "Stash / Lost & Found: your inventory is full"); }
            }
            "invcount" => {if let Some(id)=v["product"].as_str(){self.inventory.insert(id.into(),(v["count"].as_u64().unwrap_or(0),v["fits"].as_u64().unwrap_or(0)));}}
            "invall" => self.on_invall(&v),
            "sacks" => self.on_sacks(&v),
            "requoted" => self.ice_on_requoted(&v),
            "balance" => {
                self.purse = v["purse"].as_f64();
                if let Some(b) = v["bank"].as_f64() { self.bank = Some(b); }
            }
            "fill" => {
                if v["trades"].as_bool() == Some(true) && self.flip_on_trades_fill(&v) { self.say(GREEN, format!("✓ {}", v["msg"].as_str().unwrap_or("sold"))); return; }   // an auto flip's own sale: it never owned an inflight order
                let msg = v["msg"].as_str().unwrap_or("filled").to_string();
                if let Some(i) = self.inflight.pop_front() {
                    if let Some(tid) = self.trade_toasts.remove(&i) { let t = &self.trades[i]; let txt = format!("{} {} x{} done", t.side, pretty_s(&t.id), t.qty); self.toast_finish(tid, true, txt); }
                    if v["trades"].as_bool()==Some(true) {
                        if let Some(n)=v["sold"].as_u64(){self.trades[i].qty=n;if n>0{if let Some(g)=v["gained"].as_f64(){self.trades[i].price=g/n as f64;}}}
                    }
                    self.apply_fill(i, v["cost"].as_f64());
                    self.trades[i].sig.merge(&v["signals"]);
                    let ok = self.trades[i].sig.verified();
                    self.say(if ok { GREEN } else { ORANGE }, format!("{} {msg}", if ok { "✔" } else { "◐" }));
                } else { self.say(GREEN, format!("✓ {msg}")); }
            }
            "requoted_sell" => self.flip_on_requoted_sell(&v),
            "placed" => {
                let msg = v["msg"].as_str().unwrap_or("order placed").to_string();
                if let Some(i) = self.inflight.pop_front() {
                    if let Some(tid) = self.trade_toasts.remove(&i) { let t = &self.trades[i]; let txt = format!("{} {} x{} placed", t.side, pretty_s(&t.id), t.qty); self.toast_finish(tid, true, txt); }
                    self.trades[i].status = Status::Resting;
                    if let Some(p) = v["price"].as_f64() { self.trades[i].price = p; }   // the price Hypixel really used (Top Order +0.1)
                    // The order menu is authoritative: an order can fill the instant it is placed.
                    let (f, t) = (v["order"]["filled"].as_u64().unwrap_or(0), v["order"]["total"].as_u64().unwrap_or(0));
                    if t > 0 && f >= t {
                        self.trades[i].sig.orders = true;
                        self.apply_fill(i, None);
                        self.say(GREEN, "◐ filled immediately (order menu)");
                        self.claim_trade(i);
                    }
                }
                self.say(ORANGE, format!("◔ {msg}"));
            }
            "bazaar" => {
                let msg = v["msg"].as_str().unwrap_or("").to_string();
                self.say(DIM, msg.clone());
                let low = msg.to_lowercase();
                // "Your Buy Order / Sell Offer for Nx <Product> was filled!"
                if low.contains("filled") {
                    let hit = (0..self.trades.len()).rev().find(|&i| {
                        let t = &self.trades[i];
                        t.kind == Kind::Limit && matches!(t.status, Status::Resting | Status::Filled)
                            && low.contains(&pretty(&t.id).to_lowercase()) && !t.sig.chat
                    });
                    if let Some(i) = hit {
                        self.trades[i].sig.chat = true;
                        self.apply_fill(i, None);
                        self.say(GREEN, "✓ limit order filled (chat)");
                        self.claim_trade(i);
                    }
                }
            }
            "orders" => {
                // a list read before our cancels ran would bring the cancelled rows back: the real re-read follows when they have finished
                if self.busy(&["cancel"]) { return; }
                // a co-op shares one order list: the other members' orders are not ours to claim, cancel or count (kept apart: their prices still matter)
                let row = |o: &Value| OrderRow { side: o["side"].as_str().unwrap_or("?").to_string(), product: o["product"].as_str().unwrap_or("?").to_string(),
                    amount: o["amount"].as_u64(), price: o["price"].as_f64(), filled: o["filled"].as_u64().unwrap_or(0), total: o["total"].as_u64().unwrap_or(0),
                    claimable: o["claimable"].as_bool().unwrap_or(false), to_claim: o["to_claim"].as_u64().unwrap_or(0) };
                self.coop_orders = v["orders"].as_array().map(|a| a.iter().filter(|o| o["mine"].as_bool() == Some(false)).map(row).collect()).unwrap_or_default();
                self.orders = v["orders"].as_array().map(|a| a.iter().filter(|o| o["mine"].as_bool() != Some(false)).map(|o| OrderRow {
                    side: o["side"].as_str().unwrap_or("?").to_string(),
                    product: o["product"].as_str().unwrap_or("?").to_string(),
                    amount: o["amount"].as_u64(), price: o["price"].as_f64(),
                    filled: o["filled"].as_u64().unwrap_or(0), total: o["total"].as_u64().unwrap_or(0),
                    claimable: o["claimable"].as_bool().unwrap_or(false),
                    to_claim: o["to_claim"].as_u64().unwrap_or(0),
                }).collect()).unwrap_or_default();
                // Cross-check resting trades against the real order menu.
                for row in self.orders.clone() {
                    if row.total > 0 && row.filled >= row.total {
                        if let Some(i) = self.find_limit_at(&row.product.to_lowercase(), Some(&row.side), true, row.price) {
                            self.trades[i].sig.orders = true;
                            self.apply_fill(i, None);
                            self.say(GREEN, format!("◐ {} {} filled (order menu)", row.side, row.product));
                            self.claim_trade(i);
                        }
                    }
                }
                self.say(DIM, format!("{} open order(s)", self.orders.len()));
                self.orders_at = Some(Instant::now());
                self.ice_on_orders();
            }
            "step" => {
                let i = v["i"].as_u64().unwrap_or(0) as usize;
                if let Some(row) = self.wf_steps.get_mut(i) {
                    row.1 = Some(v["ok"].as_bool().unwrap_or(false));
                    row.2 = v["msg"].as_str().unwrap_or("").to_string();
                }
                let ok = v["ok"].as_bool().unwrap_or(false);
                self.say(if ok { GREEN } else { RED }, format!("workflow step {}: {}", i + 1, v["msg"].as_str().unwrap_or("")));
            }
            "pipeline_done" => {
                self.wf_running = false;
                let d = v["purse_end"].as_f64().unwrap_or(0.0) - v["purse_start"].as_f64().unwrap_or(0.0);
                let ok = v["ok"].as_bool().unwrap_or(false);
                self.wf_result = Some(format!("{} — purse {:+.1} coins", if ok { "completed" } else { "STOPPED at a failed step" }, d));
            }
            "holdings" => {
                if let Some(id) = v["product"].as_str() {
                    self.holdings.insert(id.to_string(), (v["inventory"].as_u64().unwrap_or(0), v["sacks"].as_u64()));
                }
            }
            "flipped" => {
                self.flip_on_flipped(&v);
                let msg = v["msg"].as_str().unwrap_or("flipped").to_string();
                let name = pretty(v["product"].as_str().unwrap_or("")).to_lowercase();
                let sell_price = v["sell_price"].as_f64().unwrap_or(0.0);
                let qty = v["qty"].as_u64().unwrap_or(0);
                if let Some(i) = self.find_limit_at(&name, Some("BUY"), false, v["price"].as_f64().filter(|p| *p > 0.0)) {
                    self.trades[i].sig.chat = true;
                    self.trades[i].sig.orders = true;
                    self.apply_fill(i, None);
                    let id = self.trades[i].id.clone();
                    // the flip leaves a resting sell offer behind
                    self.trades.push(Trade { time: clock(), side: "SELL", id, qty, price: sell_price, kind: Kind::Limit, status: Status::Resting, sig: Sig::default() });
                }
                self.say(GREEN, format!("↻ {msg}"));
            }
            "claimed" => {
                if self.flip_on_claimed(&v) { self.say(GREEN, format!("✓ {}", v["msg"].as_str().unwrap_or("claimed"))); return; }   // an auto flip's own claim: not an order of the blotter
                if self.auto_sack && self.sack_due.is_none() { self.sack_due = Some(Instant::now() + Duration::from_secs(4)); }   // let the claim settle, then tidy the inventory into the sacks
                let msg = v["msg"].as_str().unwrap_or("claimed").to_string();
                let name = pretty(v["product"].as_str().unwrap_or("")).to_lowercase();
                if let Some(i) = self.find_limit_at(&name, v["side"].as_str(), false, v["price"].as_f64().filter(|p| *p > 0.0)) {
                    self.trades[i].sig.merge(&v["signals"]);
                    self.apply_fill(i, None);
                    self.claiming.remove(&i);
                    let ok = self.trades[i].sig.verified();
                    self.say(if ok { GREEN } else { ORANGE }, format!("{} {msg}", if ok { "✔ verified" } else { "◐" }));
                } else { self.say(GREEN, format!("✓ {msg}")); }
                if let Some(info) = v.get("info") {
                    let rounds = info["rounds"].as_u64().unwrap_or(0);
                    let reason = info["reason"].as_str().unwrap_or("");
                    let stalled = reason.starts_with("stalled");
                    self.say(if stalled { RED } else { GREEN }, format!("claim finished in {rounds} click(s): {reason}"));
                    if !stalled {   // show it at once: the real menu is read again when the whole claim queue is done
                        let (pid, side) = (v["product"].as_str().unwrap_or("").to_string(), v["side"].as_str().unwrap_or("").to_string());
                        if let Some(k) = self.orders.iter().position(|o| o.claimable && o.side == side && o.product.to_uppercase().replace(' ', "_") == pid) { self.orders.remove(k); }
                    }
                }
            }
            "chat" => { let t = v["text"].as_str().unwrap_or("").to_string(); self.push_chat(&t); }
            "signals" => {
                let name = pretty(v["product"].as_str().unwrap_or("")).to_lowercase();
                let side = v["side"].as_str().unwrap_or("");
                if let Some(i) = (0..self.trades.len()).rev().find(|&i| self.trades[i].status == Status::Filled && pretty(&self.trades[i].id).to_lowercase() == name && self.trades[i].side == side) {
                    self.trades[i].sig.merge(&v["signals"]);
                }
                self.say(DIM, v["msg"].as_str().unwrap_or("signals").to_string());
            }
            "error" => {
                let msg = v["msg"].as_str().unwrap_or("error").to_string();
                // Only an error from a command that is waiting in `inflight` may reject a trade. An error from a claim,
                // a poll or a balance read must not mark a different, still-running order as rejected (it would be re-placed).
                let flip_error=self.flip_on_error(&msg, v["cmd"].as_str());   // an auto flip's own sale / claim / flip failed: no iceberg order is rejected by it
                let owns_inflight=!flip_error&&v["cmd"].as_str().map_or(true,|c|matches!(c,"instabuy"|"instasell"|"buyorder"|"selloffer"|"tradesellqty"|"tradesell"));
                if owns_inflight && !self.inflight.is_empty() { if let Some(i) = self.inflight.pop_front() { self.trades[i].status = Status::Rejected(msg.clone()); if let Some(tid) = self.trade_toasts.remove(&i) { self.toast_finish(tid, false, format!("Order failed: {msg}")); } } }
                else if matches!(v["cmd"].as_str(),None|Some("claim"|"claimorder"|"fliporder"|"cancel")) { self.claiming.clear(); }
                self.ice_on_error(&msg, v["cmd"].as_str());
                self.say(RED, format!("✗ {msg}"));
            }
            _ => { let m = v["msg"].as_str().map(String::from).unwrap_or(v.to_string()); self.say(DIM, m); }
        }
    }

    // ---- order ticket logic ----
    fn cmd(&self) -> &'static str {
        match (self.buy, self.kind) {
            (_, Kind::Npc) => "tradesellqty",
            (true, Kind::Instant) => "instabuy", (false, Kind::Instant) => "instasell",
            (true, Kind::Limit) => "buyorder", (false, Kind::Limit) => "selloffer",
        }
    }

    /// Largest quantity the in-game flow accepts: instant buys fill at most one inventory (2,304);
    /// bazaar orders go up to 71,680.
    fn max_qty(&self) -> u64 { if self.kind == Kind::Instant { MAX_QTY_PER_ORDER } else { 71_680 } }

    fn unit_price(&self, p: &Product) -> f64 {
        if self.exchange==2 {return self.npc_price();}
        match self.kind {
            Kind::Npc => self.npc_price(),
            Kind::Instant => if self.buy { p.ask() } else { p.bid() },
            Kind::Limit => self.limit_price,
        }
    }

    fn npc_price(&self) -> f64 {
        static PRICES: std::sync::OnceLock<HashMap<String,f64>>=std::sync::OnceLock::new();
        let _=&PRICES;
        self.selected.as_ref().and_then(|id|npc_sell_of(id)).unwrap_or(0.0)
    }

    fn buy_max(&self, unit: f64) -> u64 {
        let mut n=self.max_qty();
        if unit>0.0 {n=n.min(((DAILY_SPEND_CAP-self.spent_today).max(0.0)/unit) as u64);if let Some(p)=self.purse{n=n.min((p/unit).max(0.0) as u64);}}
        n.max(1)
    }

    /// How many of the selected item can be sold from the chosen source (inventory, plus sacks when Sacks is selected).
    fn sell_avail(&self) -> u64 {
        let Some(id)=self.selected.as_ref() else { return 0 };
        let inv=self.inventory.get(id).map(|v|v.0).unwrap_or(0);
        if self.source_sacks { inv+self.sack_row(&pretty(id)).map(|r|r.stored).unwrap_or(0) } else { inv }
    }

    fn checks(&self, unit: f64) -> Vec<(bool, String)> {
        if self.exchange==1 {return vec![(false,"Auction House isn't wired up yet".into())];}
        if self.exchange==2 {return vec![(self.agent_up,"Agent connected".into()),(!self.halted,"Trading enabled".into()),(unit>0.0,if unit>0.0{format!("NPC pays {} each",fmt_coins(unit))}else{"No NPC price for this item".into()}),(self.trade_mode>0||(self.sell_avail()>0&&self.qty<=self.sell_avail()),if self.sell_avail()==0{format!("None in your {} to sell",if self.source_sacks{"inventory or sacks"}else{"inventory"})}else{format!("Only {} in your {}",self.sell_avail(),if self.source_sacks{"inventory + sacks"}else{"inventory"})})];}
        let value = unit * self.qty as f64;
        let mut c = vec![
            (self.agent_up, "Agent connected".to_string()),
            (!self.halted, "Trading enabled".to_string()),
            (self.qty >= 1 && (self.qty <= self.max_qty() || !self.buy), format!("Quantity 1...{}", self.max_qty())),
        ];
        if !self.buy {c[2]=(self.sell_avail()>0,if self.source_sacks{"None in your inventory or sacks".to_string()}else{"None in your inventory".to_string()});}
        if self.kind == Kind::Limit {
            if let Some(p) = self.selected.as_ref().and_then(|id| self.products.get(id)) {
                let (a, b) = (p.ask(), p.bid());
                if self.buy {
                    c.push((unit <= a, format!("Bid {unit:.1} not above ask {a:.1}")));
                    c.push((unit >= b * 0.5, format!("Bid {unit:.1} not far under bid {b:.1}")));
                } else {
                    c.push((unit >= b, format!("Offer {unit:.1} not below bid {b:.1}")));
                    c.push((unit <= a * 1.5, format!("Offer {unit:.1} not far over ask {a:.1}")));
                }
            }
        }
        if self.buy {
            if let Some(p) = self.purse { c.push((value <= p, format!("Purse {} covers order", fmt_coins(p)))); }
        }
        c
    }

    fn submit(&mut self) {
        let Some(id) = self.selected.clone() else { return };
        let Some(unit) = self.products.get(&id).map(|p| self.unit_price(p)) else { return };
        if self.exchange==1 || !self.checks(unit).iter().all(|c|c.0) {return;}
        // a second click while the same order is still being sent must not send it twice
        let side_now = if self.exchange==2 || !self.buy { "SELL" } else { "BUY" };
        if self.trades.iter().any(|t| t.status==Status::Working && t.id==id && t.side==side_now && t.qty==self.qty) {
            let _ = self.toast_new(2, "That order is already being sent: wait for the result first");
            return;
        }
        if self.exchange==2 {
            if self.is_client() { let (m,q,sx)=(self.trade_mode as u64,self.qty,self.source_sacks); self.remote_op(json!({"op":"npc_sell","id":id,"mode":m,"qty":q,"unit":unit,"from_sacks":sx})); return; }
            self.npc_sell_now(&id,self.trade_mode,self.qty,unit,self.source_sacks);
            return;
        }
        let o = Order { cmd: self.cmd(), id, qty: self.qty, unit, kind: self.kind };
        if unit * self.qty as f64 > CONFIRM_ABOVE { self.confirm = Some(o); } else { self.dispatch(o); }
    }

    /// Sell to the NPC shop (/trades) right now: `mode` 0 = the quantity, 1 = fill the inventory, 2 = all sacks.
    fn npc_sell_now(&mut self, id: &str, mode_idx: u8, qty_field: u64, unit: f64, from_sacks: bool) {
        let mode=["qty","fill","all"][(mode_idx as usize).min(2)];
        let qty=if mode_idx==0{Some(qty_field.max(1))}else{None};
        let _=self.cmd_tx.send(json!({"cmd":"tradesellqty","product":id,"qty":qty,"mode":mode,"from_sacks":from_sacks}));
        self.trades.push(Trade{time:clock(),side:"SELL",id:id.to_string(),qty:qty_field,price:unit,kind:Kind::Npc,status:Status::Working,sig:Sig::default()});
        self.inflight.push_back(self.trades.len()-1);
        self.say(ORANGE,format!("→ /trades sell {} ({mode})",pretty_s(id)));
    }

    fn set_halted(&mut self, v: bool) {
        if self.is_client() { self.remote_op(json!({"op":"halt","v":v})); return; }
        self.halted = v;
        self.say(if v { RED } else { GREEN }, if v { "TRADING HALTED" } else { "trading resumed" });
    }

    /// Settings that live on the server: the app tells it when one is changed.
    fn push_setting(&mut self, key: &str, v: bool) { if self.is_client() { self.remote_op(json!({"op":"set","k":key,"v":v})); } }

    fn dispatch(&mut self, o: Order) {
        if self.is_client() { let k = match o.kind { Kind::Instant => "instant", Kind::Limit => "limit", Kind::Npc => "npc" }; self.remote_op(json!({"op":"order","cmd":o.cmd,"id":o.id,"qty":o.qty,"unit":o.unit,"kind":k,"from_sacks":self.source_sacks})); return; }
        let mut cmd=json!({"cmd": o.cmd, "product": o.id, "qty": o.qty, "price": o.unit});
        if self.source_sacks && matches!(o.cmd,"instasell"|"selloffer") { cmd["from_sacks"]=json!(true); }
        let _ = self.cmd_tx.send(cmd);
        let side = if matches!(o.cmd, "instabuy" | "buyorder") { "BUY" } else { "SELL" };
        self.trades.push(Trade { time: clock(), side, id: o.id.clone(), qty: o.qty, price: o.unit, kind: o.kind, status: Status::Working, sig: Sig::default() });
        self.inflight.push_back(self.trades.len() - 1);
        let tid = self.toast_new(0, format!("Sending {side} {} x{}…", pretty_s(&o.id), o.qty));
        self.trade_toasts.insert(self.trades.len() - 1, tid);
        self.say(ORANGE, format!("→ {side} {} x{} @ {}", pretty_s(&o.id), o.qty, fmt_coins(o.unit)));
    }

    /// Buy base -> (compactor merges 160:1 into the sack) -> pull enchanted from sacks -> sell at /trades.
    fn run_workflow(&mut self) {
        if self.is_client() { let (b, e, u) = (self.wf_base.clone(), self.wf_ench.clone(), self.wf_units); self.remote_op(json!({"op":"wf_run","base":b,"ench":e,"units":u})); return; }
        let units = self.wf_units - self.wf_units % 160;
        if units == 0 { self.say(RED, "workflow: units must be at least 160"); return; }
        if self.halted || !self.agent_up { self.say(RED, "workflow blocked: agent offline or trading halted"); return; }
        let ask = self.products.get(&self.wf_base).map(|p| p.ask()).unwrap_or(0.0);
        let cost = ask * units as f64;
        if self.spent_today + cost > DAILY_SPEND_CAP { self.say(RED, format!("workflow blocked: daily cap ({} + {})", fmt_coins(self.spent_today), fmt_coins(cost))); return; }
        self.spent_today += cost;
        let n = units / 160;
        let steps = json!([
            {"op":"instabuy","product":self.wf_base,"qty":units},
            {"op":"wait","seconds":3},
            {"op":"sackget","product":self.wf_ench,"qty":n},
            {"op":"tradesell","product":self.wf_ench}
        ]);
        self.wf_steps = vec![
            (format!("Buy {units} {}", pretty_s(&self.wf_base)), None, String::new()),
            ("Wait for the compactor".into(), None, String::new()),
            (format!("Take {n} {} from sacks", pretty_s(&self.wf_ench)), None, String::new()),
            ("Sell to /trades".into(), None, String::new()),
        ];
        self.wf_running = true;
        self.wf_result = None;
        let _ = self.cmd_tx.send(json!({"cmd":"pipeline","steps":steps}));
        self.say(ORANGE, format!("→ workflow: {units} {} (~{} coins)", pretty_s(&self.wf_base), fmt_coins(cost)));
    }

    /// Every bazaar item matching what was typed, best first. Each typed word has to start (or sit inside) a word of the
    /// item's name, so "ench seed", "enchanted seeds", "ench_seeds", "blk" (block) and even a missed letter all find it.
    fn search_items(&self, q: &str) -> Vec<String> {
        let q = q.trim().to_lowercase().replace('_', " ");
        if q.is_empty() { return vec![]; }
        let alias = |w: &str| -> String { match w { "blk" | "blck" => "block".into(), "ench" | "enchant" | "e" => "enchanted".into(), o => o.to_string() } };
        let toks: Vec<String> = q.split_whitespace().map(alias).collect();
        let squashed: String = q.split_whitespace().collect();
        let mut scored: Vec<(u32, usize, String)> = vec![];
        for id in self.products.keys() {
            let words: Vec<String> = id.to_lowercase().split('_').map(|s| s.to_string()).collect();
            let joined: String = words.concat();
            let mut total = 0u32; let mut ok = true;
            for t in &toks {
                let best = words.iter().filter_map(|w| if w == t { Some(0) } else if w.starts_with(t.as_str()) { Some(1) } else if w.contains(t.as_str()) { Some(3) } else { None }).min()
                    .or_else(|| if joined.contains(t.as_str()) { Some(4) } else { None });
                match best { Some(s) => total += s, None => { ok = false; break; } }
            }
            if ok { scored.push((total, id.len(), id.clone())); }
        }
        if scored.is_empty() {
            // nothing matched word by word: allow typos / skipped letters (the typed letters appear in order in the name)
            for id in self.products.keys() {
                let joined: String = id.to_lowercase().replace('_', "");
                let mut it = joined.chars();
                if squashed.chars().all(|c| it.any(|x| x == c)) { scored.push((20 + joined.len() as u32 / 4, id.len(), id.clone())); }
            }
        }
        scored.sort();
        scored.into_iter().map(|s| s.2).collect()
    }

    fn pick_product(&mut self, id: &str) {
        if !self.watch.iter().any(|w| w == id) { self.watch.push(id.to_string()); }
        self.selected = Some(id.to_string());
        self.search.clear(); self.sugg_sel = None;
    }

    fn add_from_search(&mut self) {
        let q = self.search.trim().to_uppercase().replace(' ', "_");
        let typed = self.search.clone();
        self.search.clear();
        if q.is_empty() { return; }
        let hit = if self.products.contains_key(&q) { Some(q.clone()) } else { self.search_items(&typed).into_iter().next() };
        match hit {
            Some(id) => { if !self.watch.contains(&id) { self.watch.push(id.clone()); } self.selected = Some(id); }
            None => self.say(RED, format!("no product matching {q}")),
        }
    }
}

fn setup_style(ctx: &egui::Context) {
    let mut s = (*ctx.style()).clone();
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("terminal".into(), egui::FontData::from_static(include_bytes!("../../assets/fonts/Terminal-Regular.ttf")).into());
    fonts.font_data.insert("terminal-bold".into(), egui::FontData::from_static(include_bytes!("../../assets/fonts/Terminal-Bold.ttf")).into());
    fonts.font_data.insert("symbols".into(), egui::FontData::from_static(include_bytes!("../../assets/fonts/Terminal-Symbols.ttf")).into());
    fonts.families.entry(egui::FontFamily::Proportional).or_default().insert(0, "symbols".into());
    fonts.families.entry(egui::FontFamily::Proportional).or_default().insert(0, "terminal".into());
    fonts.families.insert(egui::FontFamily::Name("terminal-bold".into()), vec!["terminal-bold".into(), "symbols".into()]);
    fonts.font_data.insert("brand".into(), egui::FontData::from_static(include_bytes!("../../assets/fonts/Brand-Bold.ttf")).into());
    fonts.families.insert(egui::FontFamily::Name("brand".into()), vec!["brand".into()]);
    ctx.set_fonts(fonts);
    s.override_font_id = Some(FontId::proportional(20.0));
    s.spacing.button_padding = egui::vec2(8.0, 3.0);
    s.visuals.override_text_color = Some(Color32::from_rgb(220, 220, 220));
    let v = &mut s.visuals;
    v.dark_mode = true;
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.extreme_bg_color = BG;
    v.faint_bg_color = Color32::from_rgb(12, 12, 12);
    v.selection.bg_fill = Color32::from_rgb(70, 45, 0);
    v.selection.stroke = Stroke::new(1.0, ORANGE);
    v.hyperlink_color = ORANGE;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(115, 115, 115));
    v.widgets.inactive.bg_fill = Color32::from_rgb(22, 22, 22);
    v.widgets.hovered.bg_fill = Color32::from_rgb(55, 40, 10);
    v.widgets.active.bg_fill = Color32::from_rgb(90, 55, 0);
    s.spacing.item_spacing = egui::vec2(6.0, 4.0);
    for (style, size) in [(egui::TextStyle::Body,20.0),(egui::TextStyle::Button,20.0),(egui::TextStyle::Small,16.0)] {s.text_styles.insert(style, FontId::proportional(size));}
    s.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_gray(90));
    for w in [&mut s.visuals.widgets.noninteractive, &mut s.visuals.widgets.inactive, &mut s.visuals.widgets.hovered, &mut s.visuals.widgets.active, &mut s.visuals.widgets.open] {
        w.corner_radius = egui::CornerRadius::same(3);
    }
    // buttons that egui draws itself (add_enabled, combo boxes, drag values): a little lift on hover, a press-down on click
    s.visuals.widgets.hovered.expansion = 1.0;
    s.visuals.widgets.hovered.bg_stroke = Stroke::new(1.5, ORANGE.gamma_multiply(0.85));
    s.visuals.widgets.active.expansion = -2.0;
    s.visuals.widgets.active.bg_stroke = Stroke::new(1.5, ORANGE);
    for w in [&mut s.visuals.widgets.inactive, &mut s.visuals.widgets.hovered, &mut s.visuals.widgets.active] { w.corner_radius = egui::CornerRadius::same(5); }
    s.animation_time = 0.2;
    ctx.set_style(s);
}

/// Shared button feel: colours ease over 0.2 s on hover (with a soft glow), and a press shrinks the button to 95 %.
/// Paints the button background and returns the (possibly shrunk) rectangle to draw the label in.
fn fx_bg(ui: &egui::Ui, resp: &egui::Response, rect: egui::Rect, base: Color32, hover: Color32, edge: Color32, edge_hover: Color32, rounding: f32) -> egui::Rect {
    let ctx = ui.ctx();
    let live = ui.is_enabled();
    let t = ctx.animate_bool_with_time(resp.id.with("fx_hover"), live && resp.hovered(), 0.2);
    let p = ctx.animate_bool_with_time(resp.id.with("fx_press"), live && resp.is_pointer_button_down_on(), 0.08);
    let r = egui::Rect::from_center_size(rect.center(), rect.size() * (1.0 - 0.05 * p));
    if t > 0.01 {
        for k in 1..=3 {
            ui.painter().rect_filled(r.expand(k as f32 * 2.0 * t), rounding + k as f32 * 2.0, Color32::from_rgba_unmultiplied(255, 150, 0, (14.0 * t) as u8));
        }
    }
    ui.painter().rect_filled(r, rounding, base.lerp_to_gamma(hover, t));
    ui.painter().rect_stroke(r, rounding, Stroke::new(1.0 + 0.5 * t, edge.lerp_to_gamma(edge_hover, t)), egui::StrokeKind::Inside);
    r
}

trait FxResp { fn fx_tip(self, text: impl Into<String>) -> Self; }
impl FxResp for egui::Response { fn fx_tip(self, text: impl Into<String>) -> Self { fx::tip(self, text) } }

trait FxUi {
    fn fx_checkbox(&mut self, on: &mut bool, label: &str) -> egui::Response;
    fn fx_button_en(&mut self, enabled: bool, text: impl Into<egui::WidgetText>) -> egui::Response;
    fn fx_small_button_en(&mut self, enabled: bool, text: impl Into<egui::WidgetText>) -> egui::Response;
    fn fx_button(&mut self, text: impl Into<egui::WidgetText>) -> egui::Response;
    fn fx_small_button(&mut self, text: impl Into<egui::WidgetText>) -> egui::Response;
}
fn fx_btn_impl(ui: &mut egui::Ui, text: egui::WidgetText, small: bool) -> egui::Response {
    let galley = text.into_galley(ui, None, f32::INFINITY, if small { egui::TextStyle::Small } else { egui::TextStyle::Button });
    let pad = ui.spacing().button_padding;
    let mut size = galley.size() + pad * 2.0;
    if !small { size.y = size.y.max(ui.spacing().interact_size.y); }
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let en = ui.is_enabled();
        let r = fx_bg(ui, &resp, rect, Color32::from_rgb(22, 22, 22), Color32::from_rgb(58, 42, 12), Color32::from_gray(90), ORANGE.gamma_multiply(0.85), 5.0);
        let col = if en { Color32::from_rgb(220, 220, 220) } else { Color32::from_gray(110) };
        ui.painter().galley(r.center() - galley.size() / 2.0, galley, col);
    }
    if ui.is_enabled() { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}
impl FxUi for egui::Ui {
    fn fx_checkbox(&mut self, on: &mut bool, label: &str) -> egui::Response { fx::switch(self, on, label) }
    fn fx_button_en(&mut self, enabled: bool, text: impl Into<egui::WidgetText>) -> egui::Response { self.add_enabled_ui(enabled, |ui| ui.fx_button(text)).inner }
    fn fx_small_button_en(&mut self, enabled: bool, text: impl Into<egui::WidgetText>) -> egui::Response { self.add_enabled_ui(enabled, |ui| ui.fx_small_button(text)).inner }
    fn fx_button(&mut self, text: impl Into<egui::WidgetText>) -> egui::Response { fx_btn_impl(self, text.into(), false) }
    fn fx_small_button(&mut self, text: impl Into<egui::WidgetText>) -> egui::Response { fx_btn_impl(self, text.into(), true) }
}

fn terminal_frame() -> egui::Frame {
    egui::Frame::NONE.fill(BG).stroke(Stroke::new(1.0, Color32::from_gray(115))).inner_margin(5).outer_margin(2).corner_radius(3)
}

fn gradient(ui: &egui::Ui, rect: egui::Rect, top: Color32, bottom: Color32) {
    let mut mesh = egui::Mesh::default();
    for (pos, color) in [(rect.left_top(),top),(rect.right_top(),top),(rect.left_bottom(),bottom),(rect.right_bottom(),bottom)] {
        mesh.colored_vertex(pos, color);
    }
    mesh.add_triangle(0, 1, 2); mesh.add_triangle(1, 3, 2);
    ui.painter().add(egui::Shape::mesh(mesh));
}

fn panel_heading(ui: &mut egui::Ui, text: &str, closable: bool) -> egui::Rect {
    let (rect,_) = ui.allocate_exact_size(egui::vec2(ui.available_width(),28.0),egui::Sense::hover());
    let mut bar=rect;
    let pad=if text.starts_with("Watchlist") {bar.min+=egui::vec2(-3.0,-1.0);bar.max+=egui::vec2(-1.0,3.0);11.0}
        else if text.starts_with("Order Ticket") {bar.min+=egui::vec2(-7.0,-6.0);bar.max+=egui::vec2(1.0,-2.0);11.0}
        else {bar.max+=egui::vec2(4.0,-1.0);10.0};
    gradient(ui,bar,Color32::from_rgb(255,194,16),Color32::from_rgb(255,155,0));
    let parts:Vec<_>=text.splitn(2,"  ").collect();
    if let Some(id)=parts[0].strip_prefix("Order Ticket ") {
        ui.painter().text(bar.left_center()+egui::vec2(pad,0.0),egui::Align2::LEFT_CENTER,"Order Ticket",FontId::new(22.0,egui::FontFamily::Name("terminal-bold".into())),Color32::BLACK);
        ui.painter().text(bar.left_center()+egui::vec2(150.0,0.0),egui::Align2::LEFT_CENTER,short_id(id),FontId::proportional(20.0),Color32::BLACK);
    } else {
        let spacing=if text.starts_with("Watchlist"){1.6}else{0.4};
        let mut job=egui::text::LayoutJob::default();
        job.append(parts[0],0.0,egui::TextFormat{font_id:FontId::new(24.0,egui::FontFamily::Name("terminal-bold".into())),color:Color32::BLACK,extra_letter_spacing:spacing,..Default::default()});
        let g=ui.painter().layout_job(job);
        ui.painter().galley(bar.left_center()+egui::vec2(pad,-g.size().y/2.0),g,Color32::BLACK);
    }
    if let Some(right)=parts.get(1){ui.painter().text(egui::pos2(rect.right()-7.0-if closable{24.0}else{0.0},bar.center().y),egui::Align2::RIGHT_CENTER,right.trim(),FontId::proportional(19.0),Color32::BLACK);}
    bar
}

/// A small "×" at the right end of a panel heading. Returns true when clicked.
fn close_x(ui: &mut egui::Ui, bar: egui::Rect) -> bool {
    let r=egui::Rect::from_center_size(egui::pos2(bar.right()-14.0,bar.center().y),egui::vec2(24.0,24.0));
    let resp=ui.interact(r,ui.id().with(("close_x",bar.min.x as i32,bar.min.y as i32)),egui::Sense::click()).fx_tip("Close panel (reopen from Layout or the F-keys)");
    ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,"×",FontId::proportional(26.0),if resp.hovered(){Color32::from_rgb(160,0,0)}else{Color32::BLACK});
    resp.clicked()
}

fn terminal_tab(ui: &mut egui::Ui, width:f32, text:&str, selected:bool)->egui::Response {
    let layer=ui.painter().add(egui::Shape::Noop);
    let is_blotter_tab=matches!(text,"Blotter"|"Positions"|"Escrow"|"Workflows"|"Messages");
    let (_allocated,response)=ui.allocate_exact_size(egui::vec2(width,if is_blotter_tab{28.0}else{30.0}),egui::Sense::click());
    let rect=response.rect.shrink(1.0);
    let (top,bottom)=if selected{(Color32::from_rgb(255,194,16),Color32::from_rgb(255,155,0))}else{(Color32::from_gray(if response.hovered(){55}else{37}),Color32::from_gray(15))};
    let mut shapes=Vec::new();
    let radius=4.0f32;
    for y in 0..rect.height().ceil() as usize {
        let dy=y as f32+0.5;
        let edge=dy.min(rect.height()-dy);
        let inset=if edge<radius {radius-(radius*radius-(radius-edge).powi(2)).max(0.0).sqrt()}else{0.0};
        let t=dy/rect.height();
        let rgb=|a:u8,b:u8|((a as f32)*(1.0-t)+b as f32*t)as u8;
        let color=Color32::from_rgb(rgb(top.r(),bottom.r()),rgb(top.g(),bottom.g()),rgb(top.b(),bottom.b()));
        shapes.push(egui::Shape::rect_filled(egui::Rect::from_min_max(egui::pos2(rect.left()+inset,rect.top()+y as f32),egui::pos2(rect.right()-inset,(rect.top()+y as f32+1.0).min(rect.bottom()))),0.0,color));
    }
    ui.painter().set(layer,egui::Shape::Vec(shapes));
    ui.painter().rect_stroke(response.rect,5.0,Stroke::new(1.0,Color32::from_gray(90)),egui::StrokeKind::Inside);
    ui.painter().text(response.rect.center(),egui::Align2::CENTER_CENTER,text,if selected{FontId::new(if is_blotter_tab{22.0}else{20.0},egui::FontFamily::Name("terminal-bold".into()))}else{FontId::proportional(20.0)},if selected{Color32::BLACK}else{Color32::WHITE});
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn load_history() -> HashMap<String, Vec<(f64, f64, f64)>> {
    Some(crate::bb::profile::data_dir().join("gui_history.json"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn save_history(h: &HashMap<String, Vec<(f64, f64, f64)>>) {
    {
        let p = crate::bb::profile::data_dir().join("gui_history.json");
        if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
        let _ = std::fs::write(p, serde_json::to_string(h).unwrap_or_default());
    }
}

impl eframe::App for Gui {
    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        if !self.first_frame { self.first_frame = true; tlog("first frame"); }
        self.web_rect = None; self.web_json = None;
        fx::set_reduced(self.reduced_motion);
        self.ui_time = ctx.input(|i| i.time);
        if self.auto_sleep && ctx.input(|i| i.pointer.any_pressed() || i.events.iter().any(|e| matches!(e, egui::Event::Key{pressed:true,..}|egui::Event::Text(_)))) {
            if self.is_client() { if self.activity_sent.elapsed() > Duration::from_secs(5) { self.activity_sent = Instant::now(); self.remote_op(json!({"op":"activity"})); } }
            else { self.note_activity(); }
        }
        // XFCE ignores the maximize hint given at window creation for a borderless window, so ask again once the window is up
        if self.frames < 600 { self.frames += 1; if self.frames == 3 || self.frames == 30 { ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true)); } }
        self.engine_tick();
        self.watch_autosave();
        self.view_save_tick();
        ctx.request_repaint_after(Duration::from_millis(250));
        if let Ok(n) = std::env::var("BLOCKBERG_TEST_SWITCH") { if self.pending_switch.is_none() && self.snap_start.elapsed() > Duration::from_secs(4) && bb::profile::name() != n { self.pending_switch = Some(n); } }   // test aid
        // Debug aid: BLOCKBERG_SNAPSHOT=/path.png saves the app's own rendering after a few seconds, then exits.
        if let Ok(path) = std::env::var("BLOCKBERG_SNAPSHOT") {
            if !self.snap_sent && self.snap_start.elapsed() > Duration::from_secs(9) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
                self.snap_sent = true;
            }
            let shot = ctx.input(|i| i.raw.events.iter().find_map(|e| if let egui::Event::Screenshot { image, .. } = e { Some(image.clone()) } else { None }));
            if let Some(img) = shot {
                let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
                let _ = image::save_buffer(&path, &rgba, img.width() as u32, img.height() as u32, image::ColorType::Rgba8);
                std::process::exit(0);
            }
        }
        if self.last_list_sync.elapsed() > Duration::from_secs(60) { candles::ensure_in_list(&self.watch); self.last_list_sync = Instant::now(); }
        if ctx.input(|i| i.key_pressed(egui::Key::F12)) { self.set_halted(!self.halted); }
        // ` jumps to the command line (unless you are already typing somewhere)
        if !ctx.wants_keyboard_input() && ctx.input(|i| i.key_pressed(egui::Key::Backtick)) {
            ctx.memory_mut(|m| m.request_focus(egui::Id::new("cmdline")));
            ctx.input_mut(|i| i.events.retain(|e| !matches!(e, egui::Event::Text(t) if t == "`")));
        }
        for (key, what) in [(egui::Key::F2,"watch"),(egui::Key::F3,"depth"),(egui::Key::F4,"ticket"),(egui::Key::F5,"blotter"),(egui::Key::F6,"positions"),(egui::Key::F7,"workflows"),(egui::Key::F8,"chat"),(egui::Key::F10,"logs")] {
            if ctx.input(|i| i.key_pressed(key)) { self.open_panel(what); }
        }
        if ctx.input(|i| i.key_pressed(egui::Key::F1)) { self.show_about = true; }
        // watchlist shortcuts (only while the pointer is over the watchlist and nothing is being typed)
        if self.watch_hover && !ctx.wants_keyboard_input() {
            if let Some(sel) = self.selected.clone() {
                if ctx.input(|i| i.key_pressed(egui::Key::Enter)) { self.open_info(ctx, &sel); }
                if ctx.input(|i| i.key_pressed(egui::Key::A) && !i.modifiers.command) { self.open_alert_dialog(&sel); }
                if ctx.input(|i| i.key_pressed(egui::Key::T) && !i.modifiers.command) { self.show_ticket = true; }
                if ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)) || (i.modifiers.command && i.key_pressed(egui::Key::C))) { ctx.copy_text(sel.clone()); let _ = self.toast_new(1, format!("Copied {sel}")); }
            }
        }
        if self.screen != Screen::Main && !ctx.wants_keyboard_input() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) { self.screen = Screen::Main; }

        self.menu_bar(ctx);
        self.status_bar(ctx);
        self.navigation(ctx);
        if self.show_bottom { self.bottom(ctx); }
        match self.screen {
            Screen::Main => {
                if self.show_watch { self.watchlist(ctx); }
                if self.show_ticket { self.ticket(ctx); }
                if self.show_depth { self.depth_panel(ctx); }
                self.instrument(ctx);
            }
            // the bottom panel (orders / positions / logs) stays; everything else is replaced
            Screen::Queue => self.queue_screen(ctx),
            Screen::Inventory => self.inventory_screen(ctx),
            Screen::Portfolio => self.portfolio_screen(ctx),
            Screen::Screener => { let t0 = Instant::now(); self.screener_screen(ctx); if std::env::var_os("BLOCKBERG_TIMING").is_some() { eprintln!("[screener frame] {:.1} ms", t0.elapsed().as_secs_f64() * 1000.0); } }
            Screen::AutoFlip => self.autoflip_screen(ctx),
        }
        self.dialogs(ctx);
        if self.frames == 5 && std::env::var("BLOCKBERG_DEMO_TOAST").is_ok() {
            let a = self.toast_new(0, "Claim BUY Ench Coal from escrow…"); let _ = a;
            let _ = self.toast_new(1, "Order placed: BUY Seeds x71,680");
            let _ = self.toast_new(2, "Order failed: top order +0.1 is 1.6, above your max 1.5");
        }
        self.info_window(ctx);
        self.alert_window(ctx);
        self.toasts_sync(ctx);
        self.web_sync(ctx, _f);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) { save_history(&self.history); self.save_layout(); self.save_watch(); self.ice_save(); }
}

// ---------- panels ----------
impl Gui {
    fn menu_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("brand_header_v2").show_separator_line(false).exact_height(46.0).frame(egui::Frame::NONE.fill(BG).inner_margin(0)).show(ctx, |ui| {
            let o=ui.max_rect().min;
            let bar=ui.max_rect();
            let drag=ui.interact(bar,ui.id().with("titledrag"),egui::Sense::click_and_drag());
            if drag.drag_started_by(egui::PointerButton::Primary) { ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag); }
            if drag.double_clicked() { let m=ctx.input(|i| i.viewport().maximized.unwrap_or(false)); ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!m)); }
            ui.painter().text(o+egui::vec2(18.0,23.0),egui::Align2::LEFT_CENTER,"BLOCKBERG",FontId::new(31.0,egui::FontFamily::Name("brand".into())),Color32::WHITE);
            let field=egui::Rect::from_min_size(o+egui::vec2(255.0,5.0),egui::vec2(370.0,36.0));
            ui.painter().rect_filled(field,3.0,BG);
            let r=ui.put(egui::Rect::from_min_max(field.min+egui::vec2(6.0,2.0),field.max-egui::vec2(32.0,2.0)),egui::TextEdit::singleline(&mut self.search).id(egui::Id::new("cmdline")).frame(false).font(FontId::proportional(22.0))
                .hint_text(RichText::new("Search item or command").size(21.0).extra_letter_spacing(1.2).color(Color32::from_gray(205))));
            ui.painter().rect_stroke(field,3.0,Stroke::new(1.0,Color32::from_gray(150)),egui::StrokeKind::Inside);
            fx::focus_ring(ui,&r,None);
            let drop=egui::Rect::from_min_size(egui::pos2(field.right()-24.0,field.top()+7.0),egui::vec2(18.0,22.0));
            ui.painter().rect_filled(drop,2.0,Color32::from_gray(28));
            ui.painter().text(drop.center(),egui::Align2::CENTER_CENTER,"▾",FontId::proportional(16.0),Color32::WHITE);
            // suggestions: every matching item with its icon, under the box
            let hovering_list = self.sugg_rect.map_or(false, |sr| ctx.input(|i| i.pointer.hover_pos()).map_or(false, |p| sr.contains(p)));
            let typed = self.search.trim().to_string();
            let mut chosen: Option<String> = None;
            let results = if !typed.is_empty() && (r.has_focus() || hovering_list) { self.search_items(&typed) } else { vec![] };
            if r.has_focus() && !results.is_empty() {
                let (down, up) = ctx.input(|i| (i.key_pressed(egui::Key::ArrowDown), i.key_pressed(egui::Key::ArrowUp)));
                let n = results.len().min(12);
                if down { self.sugg_sel = Some(self.sugg_sel.map_or(0, |s| (s + 1).min(n - 1))); }
                if up { self.sugg_sel = match self.sugg_sel { Some(0) | None => None, Some(s) => Some(s - 1) }; }
                if ctx.input(|i| i.key_pressed(egui::Key::Escape)) { self.search.clear(); self.sugg_sel = None; }
            } else if results.is_empty() { self.sugg_sel = None; }
            if results.is_empty() { self.sugg_rect = None; }
            else {
                let n = results.len().min(12);
                let area = egui::Area::new(egui::Id::new("search_suggestions")).order(egui::Order::Foreground).fixed_pos(egui::pos2(field.left(), field.bottom() + 2.0)).show(ctx, |ui| {
                    egui::Frame::NONE.fill(Color32::from_rgb(10, 10, 10)).stroke(Stroke::new(1.0, Color32::from_gray(150))).inner_margin(2).show(ui, |ui| {
                        ui.set_width(field.width());
                        for (i, id) in results.iter().take(n).enumerate() {
                            let (row, resp) = ui.allocate_exact_size(egui::vec2(field.width() - 4.0, 30.0), egui::Sense::click());
                            let sel = self.sugg_sel == Some(i);
                            if sel || resp.hovered() { ui.painter().rect_filled(row, 2.0, if sel { Color32::from_rgb(110, 70, 8) } else { Color32::from_gray(32) }); }
                            if let Some(ic) = &self.icons { ic.paint(ui.painter(), id, egui::Rect::from_center_size(row.left_center() + egui::vec2(16.0, 0.0), egui::vec2(22.0, 22.0))); }
                            ui.painter().text(row.left_center() + egui::vec2(36.0, 0.0), egui::Align2::LEFT_CENTER, pretty_s(id), FontId::proportional(20.0), Color32::WHITE);
                            ui.painter().text(row.right_center() - egui::vec2(8.0, 0.0), egui::Align2::RIGHT_CENTER, short_id(id), FontId::proportional(13.0), DIM);
                            if resp.clicked() { chosen = Some(id.clone()); }
                        }
                        if results.len() > n { ui.label(RichText::new(format!("+{} more, keep typing to narrow", results.len() - n)).color(DIM).size(13.0)); }
                    });
                });
                self.sugg_rect = Some(area.response.rect);
            }
            if let Some(id) = chosen { self.pick_product(&id); }
            else if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let again = self.search_items(&typed);
                if let Some(id) = self.sugg_sel.and_then(|s| again.get(s)).cloned() { self.pick_product(&id); } else { self.run_command(); }
            }
            for (x,label) in [(655.0,"F1 Help"),(745.0,"F2 Watch"),(845.0,"F3 Depth"),(945.0,"F4 Ticket"),(1055.0,"F5 Orders"),(1165.0,"F6 Port"),(1255.0,"F7 Flow"),(1340.0,"F8 Chat"),(1425.0,"F10 Logs")] {
                let available = true;
                let rect=ui.painter().text(o+egui::vec2(x,23.0),egui::Align2::LEFT_CENTER,label,FontId::new(23.0,egui::FontFamily::Name("terminal-bold".into())),ORANGE);
                let resp=ui.interact(rect,ui.id().with(label),if available{egui::Sense::click()}else{egui::Sense::hover()}).fx_tip(match label { "F1 Help"=>"Help and command list", "F2 Watch"=>"Show / hide the watchlist", "F3 Depth"=>"Show / hide market depth", "F4 Ticket"=>"Show / hide the order ticket", "F5 Orders"=>"Blotter: every order and its status", "F6 Port"=>"Positions and P&L", "F7 Flow"=>"Workflows: automated buy / compact / sell cycles", "F8 Chat"=>"In-game chat: read and send", _=>"Agent and app logs" });
                if resp.clicked() {
                    match label { "F1 Help" => self.show_about = true, "F2 Watch" => self.open_panel("watch"), "F3 Depth" => self.open_panel("depth"), "F4 Ticket" => self.open_panel("ticket"), "F5 Orders" => self.open_panel("blotter"), "F6 Port" => self.open_panel("positions"), "F7 Flow" => self.open_panel("workflows"), "F8 Chat" => self.open_panel("chat"), "F10 Logs" => self.open_panel("logs"), _ => {} }
                }
            }
            // window buttons (the window has no system title bar)
            let maxed=ctx.input(|i| i.viewport().maximized.unwrap_or(false));
            for (k,(glyph,tip)) in [("\u{2500}","Minimize"),(if maxed {"\u{2750}"} else {"\u{25A1}"},"Maximize / Restore"),("\u{2715}","Close")].into_iter().enumerate() {
                let r=egui::Rect::from_min_size(egui::pos2(bar.right()-138.0+k as f32*46.0,bar.top()),egui::vec2(46.0,bar.height()));
                let resp=ui.interact(r,ui.id().with(("winbtn",k)),egui::Sense::click()).fx_tip(tip);
                if resp.hovered() { ui.painter().rect_filled(r,0.0,if k==2 {Color32::from_rgb(200,30,40)} else {Color32::from_gray(45)}); }
                ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,glyph,FontId::proportional(20.0),Color32::WHITE);
                if resp.clicked() { match k {
                    0 => ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true)),
                    1 => ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maxed)),
                    _ => ctx.send_viewport_cmd(egui::ViewportCommand::Close) } }
            }
            // resize by dragging the window edges (a frameless window has none of its own)
            if !maxed {
                let sr=ctx.screen_rect(); let m=6.0;
                if let Some(p)=ctx.input(|i| i.pointer.hover_pos()) {
                    use egui::viewport::ResizeDirection as D;
                    let (l,r2,t,b)=(p.x<sr.left()+m,p.x>sr.right()-m,p.y<sr.top()+3.0,p.y>sr.bottom()-m);
                    let dir=match (l,r2,t,b) { (true,_,_,true)=>Some(D::SouthWest),(_,true,_,true)=>Some(D::SouthEast),(true,_,true,_)=>Some(D::NorthWest),(_,true,true,_)=>Some(D::NorthEast),
                        (true,..)=>Some(D::West),(_,true,..)=>Some(D::East),(_,_,true,_)=>Some(D::North),(_,_,_,true)=>Some(D::South),_=>None };
                    if let Some(d)=dir {
                        ctx.set_cursor_icon(match d { D::North|D::South=>egui::CursorIcon::ResizeVertical, D::East|D::West=>egui::CursorIcon::ResizeHorizontal, D::NorthWest|D::SouthEast=>egui::CursorIcon::ResizeNwSe, _=>egui::CursorIcon::ResizeNeSw });
                        if ctx.input(|i| i.pointer.primary_pressed()) { ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(d)); }
                    }
                }
            }
        });
    }

    fn navigation(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("navigation_measured").show_separator_line(false).exact_height(45.0).frame(egui::Frame::NONE.fill(BG).inner_margin(0)).show(ctx,|ui| {
            let full=ui.max_rect();
            let bar=egui::Rect::from_min_max(full.min+egui::vec2(0.0,2.0),egui::pos2(full.right(),full.top()+41.0));
            gradient(ui,bar,Color32::from_rgb(214,0,22),Color32::from_rgb(128,0,10));
            let labels=["Markets","Queue","Inventory","Portfolio","Auto Flip","Screener","Layout","Settings"];
            let mut tabs: Vec<(f32,f32,&str)>=Vec::new();
            { let mut x=0.0f32; for l in labels { let w=44.0+12.0*l.chars().count() as f32; tabs.push((x,x+w,l)); x+=w+1.0; } }
            let mut nav_sel:Option<(f32,f32)>=None;
            for (i,(x0,x1,label)) in tabs.into_iter().enumerate() {
                let r=egui::Rect::from_min_max(egui::pos2(bar.left()+x0,bar.top()),egui::pos2(bar.left()+x1,bar.bottom()));
                let selected=match self.screen { Screen::Main=>label=="Markets", Screen::Queue=>label=="Queue", Screen::Inventory=>label=="Inventory", Screen::Portfolio=>label=="Portfolio", Screen::Screener=>label=="Screener", Screen::AutoFlip=>label=="Auto Flip" };
                if selected { nav_sel=Some((r.left(),r.right())); }
                if selected {
                    let t=r.shrink2(egui::vec2(3.0,1.0));
                    ui.painter().rect_filled(t,4.0,Color32::from_rgb(255,170,0));
                    gradient(ui,t.shrink(2.0),Color32::from_rgb(255,196,30),Color32::from_rgb(255,158,0));
                } else {
                    ui.painter().line_segment([r.left_top(),r.left_bottom()],Stroke::new(1.0,Color32::from_rgb(100,0,8)));
                    ui.painter().line_segment([r.left_top()+egui::vec2(1.0,0.0),r.left_bottom()+egui::vec2(1.0,0.0)],Stroke::new(1.0,Color32::from_rgb(230,30,45)));
                }
                let col=if selected{Color32::BLACK}else{Color32::WHITE};
                let is_menu=matches!(label,"Layout"|"Settings");
                if is_menu { ui.painter().text(r.left_center()+egui::vec2(25.0,0.0),egui::Align2::CENTER_CENTER,"▼",FontId::proportional(14.0),col); }
                ui.painter().text(r.left_center()+egui::vec2(if is_menu{41.0}else{27.0},1.0),egui::Align2::LEFT_CENTER,label,if selected{FontId::new(24.0,egui::FontFamily::Name("terminal-bold".into()))}else{FontId::proportional(24.0)},col);
                let resp=ui.interact(r,ui.id().with(("nav",i)),egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                let popup=ui.id().with(("nav_popup",i));
                if resp.clicked(){
                    match label {
                        "Portfolio"=>self.open_screen(Screen::Portfolio), "Auto Flip"=>self.open_screen(Screen::AutoFlip), "Screener"=>self.open_screen(Screen::Screener),
                        "Queue"=>self.open_screen(Screen::Queue), "Inventory"=>self.open_screen(Screen::Inventory),
                        "Markets" => self.screen=Screen::Main,
                        _=>fx::dd_toggle(ui.ctx(),popup) }
                }
                fx::dd_popup(&ui.ctx().clone(),popup,(resp).rect,0.0,|ui| {
                    ui.set_min_width(220.0);
                    match label {
                        "Layout"=>{
                            fx::menu_switch(ui,&mut self.show_watch,"Watchlist  (F2)");
                            fx::menu_switch(ui,&mut self.show_depth,"Market depth  (F3)");
                            fx::menu_switch(ui,&mut self.show_ticket,"Order ticket  (F4)");
                            fx::menu_switch(ui,&mut self.show_bottom,"Blotter / Escrow / Logs / Chat  (F5)");
                            fx::menu_sep(ui);
                            if fx::menu_item(ui,"Reset layout").clicked(){self.reset_layout();fx::dd_close_all(ui.ctx());}
                        }
                        _=>{
                            fx::menu_switch(ui,&mut self.show_bottom,"Blotter / positions");
                            if fx::menu_switch(ui,&mut self.auto_claim,"Auto-claim filled orders").changed(){let v=self.auto_claim;self.push_setting("auto_claim",v);}
                            if fx::menu_item(ui,if self.halted{"Resume trading (F12)"}else{"HALT trading (F12)"}).clicked(){self.set_halted(!self.halted);fx::dd_close_all(ui.ctx());}
                            if fx::menu_item(ui,"Refresh balance + bank").clicked(){let _=self.cmd_tx.send(json!({"cmd":"balance","bank":"now"}));self.last_bank_check=Instant::now();fx::dd_close_all(ui.ctx());}
                            if fx::menu_item(ui,"Save price history").clicked(){save_history(&self.history);self.say(DIM,"history saved");fx::dd_close_all(ui.ctx());}
                            fx::menu_sep(ui);
                            if fx::menu_switch(ui,&mut self.reduced_motion,"Reduce motion").fx_tip("Turns the animations off: everything changes instantly. Starts on if your desktop asks for less motion.").changed(){self.save_layout();}
                            
                            if fx::menu_switch(ui,&mut self.auto_coins,"Auto-claim coins (every 10 min)").fx_tip("Every 10 minutes the agent collects the coins that filled sell offers left in escrow, when you have sell offers and nothing else is running.").changed(){
                                if self.is_client() && !self.server_has_coins { self.auto_coins=false; let _=self.toast_new(2,"The server is an older version and does not know this setting yet: restart the server first."); }
                                else { self.coins_next=Instant::now()+Duration::from_secs(10*60);let v=self.auto_coins;self.push_setting("auto_coins",v);if !self.is_client(){self.save_layout();} }}
                            if fx::menu_switch(ui,&mut self.auto_stash,"Auto-pick up the stash (every 20 min)").fx_tip("Every 20 minutes the agent takes everything out of your stash: it repeats /pickupstash, gives the compactor and sacks time to clear the inventory, and keeps going until the stash is empty or the inventory will not take more. Waits while an order is being placed.").changed(){
                                if self.is_client() && !self.server_has_pickup { self.auto_stash=false; let _=self.toast_new(2,"The server is an older version and does not know this setting yet: use Settings → Restart server, then switch it on."); }
                                else { self.stash_next=Instant::now()+Duration::from_secs(20*60);let v=self.auto_stash;self.push_setting("auto_stash",v);if !self.is_client(){self.save_layout();} }}
                            if fx::menu_switch(ui,&mut self.auto_lf,"Auto-pick up Lost & Found (every 20 min)").fx_tip("Every 20 minutes the agent opens the SkyBlock menu's Lost & Found (it only appears when something is waiting) and takes everything out.").changed(){
                                if self.is_client() && !self.server_has_pickup { self.auto_lf=false; let _=self.toast_new(2,"The server is an older version and does not know this setting yet: use Settings → Restart server, then switch it on."); }
                                else { self.lf_next=Instant::now()+Duration::from_secs(20*60);let v=self.auto_lf;self.push_setting("auto_lost_found",v);if !self.is_client(){self.save_layout();} }}
                            if fx::menu_switch(ui,&mut self.auto_sleep,"Disconnect agent after 20 min idle").fx_tip("Stops the desktop Minecraft when you have done nothing in Blockberg for 20 minutes and nothing is running. It starts again by itself the moment you click or type, or when an iceberg needs it.").clicked(){
                                if self.is_client() && !self.server_has_sleep { self.auto_sleep=false; let _=self.toast_new(2,"The server is an older version and does not know this setting yet: use Settings → Restart server, then try again."); }
                                else { let v=self.auto_sleep;self.push_setting("auto_sleep",v);self.last_activity=Instant::now();if !self.is_client(){self.save_layout();} }}
                            if fx::menu_switch(ui,&mut self.quick_book,"Quicker orderbook updates").fx_tip("Your spare account reads live order books in game for the items you are trading (a few seconds old instead of up to 20 s from Hypixel's API), so requotes and relists react sooner. Turning this on starts the spare account's Minecraft while your main agent is online; it is stopped when you turn this off or when the main agent has been offline for a minute.").clicked(){
                                if self.is_client() && !self.server_has_qb { self.quick_book=false; let _=self.toast_new(2,"The server is an older version and does not know this setting yet: use Settings → Restart server, then try again."); }
                                else { let v=self.quick_book;self.push_setting("quick_book",v);if !self.is_client(){self.save_layout();} }}
                            if fx::menu_switch(ui,&mut self.auto_sack,"Auto-insert claimed items into sacks").fx_tip("The game puts bazaar claims in your inventory (the overflow goes to the stash); it never fills sacks by itself. With this on, a few seconds after each claim the agent uses /sacks → Insert inventory, so claims keep fitting. Only items one of your sacks accepts are moved.").changed(){
                                if self.is_client() && !self.server_has_sack { self.auto_sack=false; let _=self.toast_new(2,"The server is an older version and does not know this setting yet: use Settings → Restart server, then switch it on."); }
                                else { let v=self.auto_sack;self.push_setting("auto_sack",v);if !self.is_client(){self.save_layout();} }}
                            if fx::menu_item_en(ui,!self.busy(&["pickup"]),"Pick up the stash now").fx_tip("Takes everything out of the stash (up to about 8 minutes of work), making room as it goes.").clicked(){let _=self.cmd_tx.send(json!({"cmd":"pickup","stash":true,"lost_found":false,"max_secs":480}));fx::dd_close_all(ui.ctx());}
                            if fx::menu_item_en(ui,!self.busy(&["pickup"]),"Pick up Lost & Found now").clicked(){let _=self.cmd_tx.send(json!({"cmd":"pickup","stash":false,"lost_found":true}));fx::dd_close_all(ui.ctx());}
                            fx::menu_sep(ui);
                            if fx::menu_item_en(ui,!self.busy(&["insert_sacks"]),"Insert inventory into sacks").fx_tip("Uses /sacks → Insert inventory. Items none of your sacks accept stay in the inventory.").clicked(){let _=self.cmd_tx.send(json!({"cmd":"insert_sacks"}));fx::dd_close_all(ui.ctx());}
                            if fx::menu_item_en(ui,!self.busy(&["sellinv"]),"Sell inventory to the bazaar…").fx_tip("Lists every bazaar item you carry as a sell offer at the lowest offer minus 0.1. Asks first.").clicked(){self.sell_inv_confirm=Some(self.sell_inv_preview());fx::dd_close_all(ui.ctx());}
                            fx::menu_sep(ui);
                            if fx::menu_item(ui,"Bank: deposit / withdraw…").fx_tip("Move coins between your purse and the co-op or personal bank (in game, through /bank).").clicked(){self.bank_dlg=Some(BankDlg{coop:true,deposit:true,mode:0,text:String::new()});fx::dd_close_all(ui.ctx());}
                            if fx::menu_item(ui,"Clear blotter").clicked(){self.clear_blotter();fx::dd_close_all(ui.ctx());}
                            if self.is_client() && fx::menu_item_en(ui,self.server_restart.is_none(),"Restart server (after current work)").fx_tip("Queued with high priority: it waits for what is running to finish, starts nothing new, then restarts and carries on from its saved state.").clicked(){self.remote_op(json!({"op":"restart"}));fx::dd_close_all(ui.ctx());}
                            {
                                let up=self.agent_up; let busy=!self.inflight.is_empty();
                                let label=if up{"Disconnect agent (stop Minecraft)"}else{"Connect agent (start Minecraft)"};
                                let r=fx::menu_item_en(ui,!(up&&busy),label).fx_tip(if up{"Stops the desktop Minecraft that carries the agent. Resting orders stay on the bazaar; Blockberg just cannot place or claim anything until you connect again. Greyed out while an action is running."}else{"Starts the desktop Minecraft (headless), joins Hypixel and goes to SkyBlock. The agent is ready about a minute later."});
                                if r.clicked(){
                                    let act=if up{"stop"}else{"start"};
                                    if self.is_client(){ if self.server_has_agent_ctl { self.remote_op(json!({"op":"agent_service","do":act})); } else { let _=self.toast_new(2,"The server is an older version and does not know this button yet: use Settings → Restart server, then try again."); } }else{self.agent_service_local(act);}
                                    fx::dd_close_all(ui.ctx());
                                }
                            }
                            for e in bb::profile::all().into_iter().filter(|e| e.name != bb::profile::name()) {
                                {
                                    let unit=if e.name=="main"{"bblauncher-agent".to_string()}else{format!("bblauncher-agent@{}-{}",e.name,e.agent.rsplit(':').next().unwrap_or(""))};
                                    let up=unit_active(&unit);
                                    let label=if up{format!("Disconnect {} agent (stop its Minecraft)",e.name)}else{format!("Connect {} agent (start its Minecraft)",e.name)};
                                    if fx::menu_item(ui,&label).fx_tip("Starts or stops that account's desktop Minecraft. Its resting orders stay on the bazaar. Disconnecting it also turns off Quicker orderbook updates, which would otherwise start it again.").clicked(){
                                        if up && self.quick_book { self.quick_book=false; self.push_setting("quick_book",false); if !self.is_client(){self.save_layout();} self.say(ORANGE,"Quicker orderbook updates turned off (it needs the spare agent)"); }
                                        let act=if up{"stop"}else{"start"};
                                        self.say(ORANGE,&format!("{} the {} agent…",if up{"Disconnecting"}else{"Connecting"},e.name));
                                        if !cfg!(test) && !self.mock { std::thread::spawn(move||{let _=std::process::Command::new("systemctl").args(["--user",act,&unit]).status();}); }
                                        fx::dd_close_all(ui.ctx());
                                    }
                                }
                                if fx::menu_item(ui,&format!("Open account window: {}",e.name)).fx_tip("Opens a separate Blockberg window for that Minecraft account (its own agent, orders, icebergs and portfolio).").clicked(){
                                    if let Ok(exe)=std::env::current_exe(){ let _=std::process::Command::new(exe).env("BLOCKBERG_PROFILE",&e.name).env_remove("BLOCKBERG_AGENT").spawn(); }
                                    fx::dd_close_all(ui.ctx());
                                }
                            }
                            if fx::menu_item(ui,"Restart agent (load newest version)").clicked(){let _=self.cmd_tx.send(json!({"cmd":"reload"}));self.say(ORANGE,"→ restarting the agent…");fx::dd_close_all(ui.ctx());}
                            if fx::menu_item(ui,"About").clicked(){self.show_about=true;fx::dd_close_all(ui.ctx());}
                            if fx::menu_item(ui,"Quit").clicked(){ctx.send_viewport_cmd(egui::ViewportCommand::Close);}
                        }
                    }
                });
            }
            if let Some((l,rr_))=nav_sel {   // a thin underline that slides to the selected tab
                let (ux0,ux1)=fx::slide_indicator(ctx,ui.id().with("nav_ind"),l,rr_);
                ui.painter().rect_filled(egui::Rect::from_min_max(egui::pos2(ux0,bar.bottom()-3.0),egui::pos2(ux1,bar.bottom())),0.0,Color32::WHITE);
            }
            let sel=self.selected.as_deref().unwrap_or("—");
            let mut x=bar.right()-17.0;
            let tabs_end=bar.left()+{let mut e=0.0f32; for l in labels { e+=44.0+12.0*l.chars().count() as f32+1.0; } e}+24.0;
            for part in [format!("TRAD"),"|".into(),format!("{} ({})",pretty_s(sel),sel),"›".into(),"CMDTY".into()] {
                let part_txt=part.clone();
                let g=ui.painter().layout_no_wrap(part,FontId::proportional(22.0),Color32::WHITE);
                let w=g.size().x;
                if x-w<tabs_end { break; }   // the new tabs take room: drop the breadcrumb parts that no longer fit
                let pr=egui::Rect::from_min_size(egui::pos2(x-w,bar.center().y-g.size().y/2.0),g.size());
                let clickable=part_txt=="TRAD"||part_txt.ends_with(')');
                if clickable {
                    let rs=ui.interact(pr.expand(3.0),ui.id().with(("crumb",part_txt.clone())),egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                    let tipt=if part_txt=="TRAD"{"Back to the trading screen"}else{"Open Item Info for this item"};
                    let rs=fx::tip(rs,tipt);
                    if rs.clicked(){ if part_txt=="TRAD"{self.screen=Screen::Main;}else if let Some(id)=self.selected.clone(){ let c=ctx.clone(); self.apply_menu_act(&c,&id,MenuAct::Info); } }
                    ui.painter().galley(pr.min,g,if rs.hovered(){ORANGE}else{Color32::WHITE});
                } else { ui.painter().galley(pr.min,g,Color32::from_gray(150)); }
                x-=w+21.0;
            }
        });
    }

    fn status_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("status_measured").show_separator_line(false).exact_height(41.0).frame(egui::Frame::NONE.fill(BG).inner_margin(0)).show(ctx,|ui| {
            let (rect,_)=ui.allocate_exact_size(egui::vec2(ui.available_width(),41.0),egui::Sense::hover());
            ui.painter().line_segment([rect.left_top()+egui::vec2(0.0,1.0),rect.right_top()+egui::vec2(0.0,1.0)],Stroke::new(1.0,Color32::from_gray(90)));
            let paint=|x:f32,text:String,color:Color32,bold:bool|ui.painter().text(rect.min+egui::vec2(x,22.0),egui::Align2::LEFT_CENTER,text,if bold{FontId::new(22.0,egui::FontFamily::Name("terminal-bold".into()))}else{FontId::proportional(21.5)},color);
            let agent_rect=egui::Rect::from_min_size(rect.min+egui::vec2(8.0,5.0),egui::vec2(274.0,29.0));
            fx::tip_rect(ui,agent_rect,"agent",if self.is_client(){"This app is a window onto the Blockberg server, which keeps working when the app is closed. SERVER is the link to it; AGENT is the server's link to Minecraft on the laptop."}else{"The helper running inside Minecraft on the laptop. Green means connected; orders, claims and scans go through it."});
            fx::tip_rect(ui,egui::Rect::from_min_size(rect.min+egui::vec2(330.0,5.0),egui::vec2(110.0,29.0)),"prices","How long ago the bazaar prices were last refreshed (every 10 seconds).");
            fx::tip_rect(ui,egui::Rect::from_min_size(rect.min+egui::vec2(458.0,5.0),egui::vec2(170.0,29.0)),"purse","Coins in your purse, read from the in-game scoreboard.");
            let link_ok=!self.is_client()||self.link_up.load(std::sync::atomic::Ordering::Relaxed);
            let all_ok=self.agent_up&&link_ok;
            ui.painter().rect_filled(agent_rect,3.0,if all_ok{Color32::from_rgb(0,25,0)}else{Color32::from_rgb(65,0,0)});
            if self.is_client() {
                let txt=if !link_ok{format!("SERVER OFFLINE  {}",self.link_state)}else if self.server_restart.is_some(){"SERVER RESTARTING…".to_string()}else{format!("SERVER ✔  AGENT {}",if self.agent_up{"✔"}else{"OFFLINE"})};
                paint(16.0,txt,if all_ok&&self.server_restart.is_none(){GREEN}else{RED},true);
            } else {
                paint(16.0,format!("AGENT  {}{}",if self.agent_up{""}else{"OFFLINE "},self.agent_addr),if self.agent_up{GREEN}else{RED},true);
            }
            for x in [318.0,445.0]{paint(x,"|".into(),Color32::WHITE,false);}
            paint(338.0,self.last_update.map(|t|format!("Prices {}s",if self.mock{6}else{t.elapsed().as_secs()})).unwrap_or("Prices …".into()),Color32::WHITE,false);
            paint(466.0,"PURSE".into(),Color32::WHITE,false);paint(535.0,self.purse.map(|v|fmt_coins(fx::count_to(ctx,egui::Id::new("purse_anim"),v))).unwrap_or("—".into()),ORANGE,false);
            {
                let sold=self.npc_sold_today();
                let lbl=paint(650.0,"NPC SOLD".into(),Color32::WHITE,false);
                let r=paint(762.0,format!("{} / {}",fmt_coins(sold),fmt_coins(NPC_SELL_CAP)),if sold>=NPC_SELL_CAP*0.9{RED}else{ORANGE},false);
                let npc_resp=ui.interact(lbl.union(r).expand(4.0),ui.id().with("npcsold"),egui::Sense::click());
                if npc_resp.secondary_clicked()||npc_resp.clicked(){ if self.npc_calc.is_some(){self.npc_calc=None;}else{self.npc_calc=Some(NpcCalc{item:self.selected.clone().unwrap_or_default(),have:String::new(),search_open:false});} }
                npc_resp.fx_tip("Coins NPCs paid you today out of the 500M daily NPC sell limit (resets 00:00 UTC). Right-click for a calculator: how much of an item you can still sell. Counts sales you make through Blockberg or that show in the game chat.");
            }
            let mut clear_calc=false;
            if let Some((expr,res))=self.calc.clone() {
                let r=paint(980.0,format!("{expr} = {res}"),ORANGE,true);
                if ui.interact(r.expand(4.0),ui.id().with("calcclear"),egui::Sense::click()).fx_tip("Click to clear").clicked(){clear_calc=true;}
            }
            if clear_calc {self.calc=None;}
            {
                let multi=self.is_client()&&bb::profile::all().len()>1;
                let txt=if self.halted{"TRADING HALTED (F12)".to_string()}else if multi{format!("ACCOUNT: {} ▾",bb::profile::name().to_uppercase())}else if bb::profile::is_main(){"BLOCKBERG WORKSTATION".to_string()}else{format!("ACCOUNT: {}",bb::profile::name().to_uppercase())};
                let col=if self.halted{RED}else if multi&&!bb::profile::is_main(){ORANGE}else{Color32::WHITE};
                let g=ui.painter().layout_no_wrap(txt,FontId::proportional(21.0),col);
                let r=egui::Rect::from_min_size(egui::pos2(rect.right()-14.0-g.size().x,rect.top()+22.0-g.size().y/2.0),g.size());
                ui.painter().galley(r.min,g,col);
                if multi{
                    let resp=ui.interact(r.expand(6.0),ui.id().with("acct_chip"),egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).fx_tip("Switch account (Blockberg keeps one server per Minecraft account)");
                    if resp.clicked(){self.acct_menu=if self.acct_menu.is_some(){None}else{self.acct_menu_t=self.ui_time;Some(egui::pos2(rect.right()-10.0,rect.top()+40.0))};}
                }
            }
        });
    }

    fn watchlist(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("watch_reference").show_separator_line(false).frame(terminal_frame()).resizable(true).default_width((ctx.screen_rect().width() * 0.306).max(420.0)).width_range(330.0..=1000.0).show(ctx, |ui| {
            let hb=panel_heading(ui, "Watchlist: Commodities", false);
            ui.painter().text(hb.right_center()-egui::vec2(10.0,0.0),egui::Align2::RIGHT_CENTER,"(Edit)",FontId::proportional(19.0),Color32::BLACK);
            let full=ui.max_rect();
            self.watch_hover=ui.rect_contains_pointer(full);
            let (l,t)=(full.left()-7.0,full.top()-139.0); // offsets so the numbers below are mockup pixels
            let sx=(full.width()/513.0).clamp(0.62,2.4);
            let at=|x:f32,y:f32|egui::pos2(l+x*sx,t+y);
            let font=|sz:f32|FontId::proportional(sz);
            let bold=|sz:f32|FontId::new(sz,egui::FontFamily::Name("terminal-bold".into()));
            let p=ui.painter().clone();
            // toolbar strip
            let strip=egui::Rect::from_min_max(at(7.0,174.0),at(506.0,201.0));
            p.rect_filled(strip,0.0,Color32::from_gray(8));
            p.rect_stroke(strip,1.0,Stroke::new(1.0,Color32::from_gray(70)),egui::StrokeKind::Inside);
            let mut clicked=None;
            for (x0,x1,label,boxed,arrow) in [(9.0,67.0,"View",true,Some(58.0)),(70.0,201.0,"Standard",true,Some(189.0)),(207.0,320.0,"+ Add",false,None),(339.0,412.0,"Remove",false,None),(432.0,505.0,"Wgt",false,Some(494.0))] {
                let r=egui::Rect::from_min_max(at(x0,175.0),at(x1,200.0));
                if boxed { p.rect_filled(r,2.0,Color32::from_gray(14)); p.rect_stroke(r,2.0,Stroke::new(1.0,Color32::from_gray(150)),egui::StrokeKind::Inside); }
                let shown=if label=="Standard"{["Bazaar","NPC","Both"][self.watch_mode as usize]}else{label};
                p.text(r.left_center()+egui::vec2(8.0,0.0),egui::Align2::LEFT_CENTER,shown,font(18.0),Color32::WHITE);
                if let Some(ax)=arrow { p.text(egui::pos2(l+ax*sx,r.center().y),egui::Align2::CENTER_CENTER,"▾",font(15.0),Color32::WHITE); }
                let resp=ui.interact(r,ui.id().with(("wtool",label)),egui::Sense::click());
                if resp.clicked(){clicked=Some(label);}
                let popup=ui.id().with(("wpop",label));
                if resp.clicked() && label!="Remove" {fx::dd_toggle(ui.ctx(),popup);}
                fx::dd_popup(&ui.ctx().clone(),popup,(resp).rect,0.0,|ui|{
                    ui.set_min_width(200.0);
                    match label {
                        "View"=>{ui.label("Click an instrument to select it.");ui.label("Right-click a row to remove it.");}
                        "Standard"=>{
                            for (m,n) in [(0u8,"Bazaar"),(1,"NPC"),(2,"Both")] { if ui.selectable_label(self.watch_mode==m,n).clicked() { self.watch_mode=m; self.save_layout(); fx::dd_close_all(ui.ctx()); } }
                            ui.separator();
                            if ui.fx_button("Restore default symbols").clicked(){for id in DEFAULT_WATCH{if !self.watch.iter().any(|w|w==id){self.watch.push((*id).into());}}fx::dd_close_all(ui.ctx());}}
                        "Wgt"=>{ui.label(format!("{} instruments",self.watch.len()));}
                        _=>{let r=ui.text_edit_singleline(&mut self.search);
                            if ui.fx_button("Add symbol").clicked()||(r.lost_focus()&&ui.input(|i|i.key_pressed(egui::Key::Enter))){self.add_from_search();fx::dd_close_all(ui.ctx());}}
                    }
                });
            }
            for x in [327.0,420.0] { p.text(at(x,187.5),egui::Align2::CENTER_CENTER,"|",font(18.0),Color32::from_gray(200)); }
            if clicked==Some("Remove") { if let Some(id)=self.selected.clone(){self.watch.retain(|w|w!=&id);self.selected=self.watch.first().cloned();} }
            // column header
            p.rect_filled(egui::Rect::from_min_max(at(7.0,203.0),at(506.0,234.0)),0.0,Color32::from_gray(6));
            p.line_segment([at(7.0,234.0),at(506.0,234.0)],Stroke::new(1.0,Color32::from_gray(45)));
            p.text(at(18.0,218.0),egui::Align2::LEFT_CENTER,"Symbol",font(20.0),Color32::WHITE);
            let heads:&[(f32,&str)]=match self.watch_mode { 1=>&[(205.0,"Buy"),(274.0,"Sell"),(336.0,"Sprd"),(469.0,"Vol (M)")], 2=>&[(208.0,"Bid"),(268.0,"Buy"),(336.0,"Ask"),(394.0,"Sell"),(469.0,"Vol (M)")], _=>&[(205.0,"Bid"),(274.0,"Ask"),(336.0,"Sprd"),(469.0,"Vol (M)")] };
            for (cx,h) in heads {
                p.text(at(*cx,218.0),egui::Align2::CENTER_CENTER,*h,font(if self.watch_mode==2{17.0}else{20.0}),Color32::WHITE);
                let tip=match *h { "Bid"=>"Bid: the highest price a buyer is offering. What you get if you sell instantly.", "Ask"=>"Ask: the lowest price a seller wants. What you pay if you buy instantly.",
                    "Buy"=>"NPC shop price: what an NPC charges you for one. A dash means no NPC sells it.", "Sell"=>"NPC sell price: what an NPC pays you for one.",
                    "Sprd"=>"Spread: Ask minus Bid. The gap between buying and selling instantly.", "Vol (M)"=>"Items traded on the bazaar over the last 7 days, instant buys plus instant sells (M = millions of items, not coins).", _=>"" };
                if !tip.is_empty() { fx::tip_rect(ui,egui::Rect::from_center_size(at(*cx,218.0),egui::vec2(60.0,24.0)),("wlhead",*h),tip); }
            }
            // rows
            let mut y=238.0;
            let mut pick=None; let mut remove=None; let mut menu_act:Option<(String,MenuAct)>=None;
            for id in self.watch.clone() {
                let sel=self.selected.as_ref()==Some(&id);
                let h=if sel{33.0}else{29.6};
                let row=egui::Rect::from_min_max(at(7.0,y),at(506.0,y+h));
                if row.bottom()>full.bottom()+2.0 {break;}
                let r=ui.interact(row,ui.id().with(("wrow",&id)),egui::Sense::click());
                if sel { gradient(ui,row,Color32::from_rgb(110,70,8),Color32::from_rgb(55,34,4)); }
                fx::row_bg(ui,ui.id().with(("wrowfx",&id)),row,r.hovered(),sel);
                let cy=y+h/2.0;
                let mut name=pretty_s(&id);
                {   // never run into the first number column: shorten the name with an ellipsis
                    let nf=if self.watch_mode==2{if sel{bold(18.0)}else{font(17.5)}}else if sel{bold(21.0)}else{font(20.0)};
                    let avail=(if self.watch_mode==2{232.0}else{232.0})-56.0-42.0;
                    if p.layout_no_wrap(name.clone(),nf.clone(),Color32::WHITE).size().x>avail {
                        let mut base:String=name.clone();
                        while base.chars().count()>2 { base.pop(); let t=format!("{}…",base.trim_end()); if p.layout_no_wrap(t.clone(),nf.clone(),Color32::WHITE).size().x<=avail { name=t; break; } }
                    }
                }
                if let Some(ic)=&self.icons { ic.paint(&p,&id,egui::Rect::from_center_size(at(26.0,cy),egui::vec2(22.0,22.0))); }
                p.text(at(42.0,cy),egui::Align2::LEFT_CENTER,name,if self.watch_mode==2{if sel{bold(18.0)}else{font(17.5)}}else if sel{bold(21.0)}else{font(20.0)},if sel{ORANGE}else{Color32::WHITE});
                if let Some(pr)=self.products.get(&id) {
                    let cx=ui.ctx().clone();
                    let an=|k:&str,v:f64|fx::count_to(&cx,egui::Id::new(("wl",&id,k)),v);   // numbers count to their new value
                    let (a,b)=(an("a",pr.ask()),an("b",pr.bid()));
                    let vol=fmt_coins(an("v",(pr.quick_status.buy_moving_week+pr.quick_status.sell_moving_week) as f64));
                    let (nb,ns)=(npc_buy_of(&id),npc_sell_of(&id));
                    let cols:Vec<(f32,String,Color32,bool)>=match self.watch_mode {
                        1=>vec![(232.0,npc_fmt(nb),ORANGE,true),(299.0,npc_fmt(ns),ORANGE,true),(357.0,"—".into(),DIM,false),(505.0,vol,Color32::WHITE,sel)],
                        2=>vec![(232.0,fmt_coins(b),RED,true),(292.0,npc_fmt(nb),ORANGE,true),(360.0,fmt_coins(a),GREEN,true),(418.0,npc_fmt(ns),ORANGE,true),(505.0,vol,Color32::WHITE,sel)],
                        _=>vec![(232.0,fmt_coins(b),RED,true),(299.0,fmt_coins(a),GREEN,true),(357.0,fmt_coins(a-b),Color32::WHITE,sel),(505.0,vol,Color32::WHITE,sel)] };
                    let fs=if self.watch_mode==2{17.0}else{20.0};
                    for (rx,text,col,b_) in cols {
                        p.text(at(rx,cy),egui::Align2::RIGHT_CENTER,text,if b_{bold(fs)}else{font(fs)},col);
                    }
                } else if fx::loading_for(ui.ctx(),ui.id().with("wl_loading"),self.products.is_empty(),0.15) {
                    // prices have not arrived yet: placeholders shaped like the numbers they will become
                    for (k,wd) in [(232.0f32,54.0f32),(299.0,54.0),(357.0,44.0),(505.0,64.0)] { fx::skeleton(ui,egui::Rect::from_min_max(at(k-wd,cy-8.0),at(k,cy+8.0)),3.0); }
                } else { p.text(at(505.0,cy),egui::Align2::RIGHT_CENTER,"—",font(20.0),DIM); }
                if r.clicked(){pick=Some(id.clone());}
                let mut chosen:Option<MenuAct>=None;
                r.context_menu(|ui|{ if let Some(a)=self.watch_menu(ui,&id){ chosen=Some(a); ui.close_menu(); } });
                if r.secondary_clicked() { pick=Some(id.clone()); }   // right-click also selects the row
                if let Some(a)=chosen { menu_act=Some((id.clone(),a)); }
                y+=h;
            }
            if let Some((mid,a))=menu_act{ let c=ui.ctx().clone(); self.apply_menu_act(&c,&mid,a); }
            if let Some(id)=pick{self.selected=Some(id);}
            if let Some(id)=remove{self.watch.retain(|w|*w!=id);if self.selected.as_ref()==Some(&id){self.selected=self.watch.first().cloned();}}
        });
    }

    fn instrument(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().frame(egui::Frame::NONE.inner_margin(0)).show(ctx, |ui| {
            let Some(id) = self.selected.clone() else { ui.label("Select a symbol"); return };
            let Some(p) = self.products.get(&id).cloned() else { ui.label("Loading live bazaar feed…"); return };
            terminal_frame().show(ui, |ui| {
                let avail=ui.available_height();
                let (area,_)=ui.allocate_exact_size(egui::vec2(ui.available_width(),avail.max(200.0)),egui::Sense::hover());
                self.quote_chart(ui,area,&id,&p);
            });
        });
    }

    /// Market depth as its own panel: drag its top edge to resize it, click × to close it.
    fn depth_panel(&mut self, ctx: &egui::Context) {
        let Some(id) = self.selected.clone() else { return };
        let loading_depth = !self.products.contains_key(&id);   // keep the panel in place with skeleton rows instead of popping in later
        let p = self.products.get(&id).cloned().unwrap_or_else(|| serde_json::from_str::<Product>("{}").unwrap_or_else(|_| mock_products().values().next().cloned().unwrap()));
        egui::TopBottomPanel::bottom("depth_panel").show_separator_line(false).resizable(true).default_height(253.0).height_range(110.0..=620.0)
            .frame(egui::Frame::NONE.inner_margin(0)).show(ctx, |ui| {
            terminal_frame().inner_margin(egui::Margin{left:5,right:5,top:0,bottom:4}).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y=0.0;
            let hb=panel_heading(ui, &format!("Market Depth ({})  DEPTH",short_id(&id)), true);
            if close_x(ui,hb){self.show_depth=false;}
            let area=ui.available_rect_before_wrap();
            ui.allocate_rect(area,egui::Sense::hover());
            let mid=area.left()+(area.width()-10.0)/2.0;
            ui.painter().line_segment([egui::pos2(mid+5.0,area.top()+2.0),egui::pos2(mid+5.0,area.bottom())],Stroke::new(1.0,Color32::from_gray(60)));
            let (sell_s,buy_s):(&[Level],&[Level])=if loading_depth {(&[],&[])} else {(&p.sell_summary,&p.buy_summary)};
            depth(ui, egui::Rect::from_min_max(area.min,egui::pos2(mid,area.bottom())), "Bids (Buy Orders)", sell_s, RED, loading_depth);
            depth(ui, egui::Rect::from_min_max(egui::pos2(mid+10.0,area.top()),area.max), "Asks (Sell Offers)", buy_s, GREEN, loading_depth);
            });
        });
    }

    // ---------- notifications ----------
    fn toast_new(&mut self, kind: u8, text: impl Into<String>) -> u64 {
        self.toast_seq += 1;
        let t = fx::Toast { id: self.toast_seq, text: text.into(), kind, born: self.ui_time, done: if kind == 0 { None } else { Some(self.ui_time) } };
        self.toasts.push(t);
        if self.toasts.len() > 6 { self.toasts.remove(0); }
        self.toast_seq
    }
    /// Turn a "working" notification into its result. Success is only ever shown from here, after the work is really done.
    fn toast_finish(&mut self, id: u64, ok: bool, text: impl Into<String>) {
        let now = self.ui_time;
        if let Some(t) = self.toasts.iter_mut().find(|t| t.id == id) { t.kind = if ok { 1 } else { 2 }; t.text = text.into(); t.done = Some(now); }
    }
    /// Is an agent request of one of these kinds still waiting for its answer? (Used to disable the button, so it can't be sent twice.)
    fn busy(&self, cmds: &[&str]) -> bool {
        self.cmd_tx.log.lock().map_or(false, |l| l.iter().any(|a| a.done.is_none() && cmds.contains(&a.cmd.as_str())))
    }
    /// Notifications for what the user asked the agent to do: a spinner while it runs, then the real outcome.
    fn toasts_derive(&mut self) {
        let log: Vec<ActRec> = self.cmd_tx.log.lock().map(|l| l.clone()).unwrap_or_default();
        for a in log.iter() {
            let worth = !a.auto && matches!(a.cmd.as_str(), "claimorder" | "claim" | "cancel" | "fliporder" | "sackscan" | "orders" | "reload" | "sackget" | "balance" | "pickup" | "insert_sacks" | "sellinv");
            if !worth { continue; }
            match self.act_toasts.get(&a.id).copied() {
                None => { if a.done.is_none() || a.sent.elapsed() < Duration::from_secs(4) { let id = self.toast_new(0, format!("{}…", a.label)); self.act_toasts.insert(a.id, id); } else { continue; } }
                Some(_) => {}
            }
            let tid = self.act_toasts[&a.id];
            if let Some((_, res)) = &a.done {
                let working = self.toasts.iter().any(|t| t.id == tid && t.kind == 0);
                if working { match res { None => self.toast_finish(tid, true, format!("{} — done", a.label)), Some(e) => self.toast_finish(tid, false, format!("{} — {}", a.label, e)) } }
            }
        }
        // anything still "working" long after the agent should have answered is reported as failed, never left spinning
        let now = self.ui_time;
        let stale: Vec<u64> = self.toasts.iter().filter(|t| t.kind == 0 && now - t.born > 180.0).map(|t| t.id).collect();
        for id in stale { self.toast_finish(id, false, "no answer from the agent"); }
        // a finished notification is dropped a few seconds after it was shown
        let ui_time = self.ui_time;
        self.toasts.retain(|t| t.done.map_or(true, |d| ui_time - d < 6.0));
        self.act_toasts.retain(|_, tid| self.toasts.iter().any(|t| t.id == *tid));
        self.trade_toasts.retain(|_, tid| self.toasts.iter().any(|t| t.id == *tid));
    }

    /// Draw the notifications (the app's window only).
    fn toasts_sync(&mut self, ctx: &egui::Context) {
        let gone = fx::show_toasts(ctx, &self.toasts);
        if !gone.is_empty() { self.toasts.retain(|t| !gone.contains(&t.id)); }
    }

    /// Empty the blotter, but keep (and re-number) every trade something still points at: an iceberg's live order, a held-back
    /// or running claim, an action waiting for the agent. Dropping those would leave stale indexes that crash the app.
    fn clear_blotter(&mut self) {
        if self.is_client() { self.remote_op(json!({"op":"clear_blotter"})); return; }
        let mut keep: std::collections::BTreeSet<usize> = Default::default();
        keep.extend(self.jobs.iter().filter_map(|j| j.cur_trade));
        keep.extend(self.ice_deferred.iter().map(|d| d.0));
        keep.extend(self.claiming.iter().copied());
        keep.extend(self.inflight.iter().copied());
        let map: HashMap<usize, usize> = keep.iter().enumerate().map(|(n, &o)| (o, n)).collect();
        self.trades = keep.iter().filter_map(|&o| self.trades.get(o).cloned()).collect();
        for j in self.jobs.iter_mut() { j.cur_trade = j.cur_trade.and_then(|t| map.get(&t).copied()); }
        for d in self.ice_deferred.iter_mut() { d.0 = map.get(&d.0).copied().unwrap_or(0); }
        self.claiming = self.claiming.iter().filter_map(|t| map.get(t).copied()).collect();
        self.inflight = self.inflight.iter().filter_map(|t| map.get(t).copied()).collect();
    }

    /// Create / move / feed the web chart that covers the chart panel (falls back to the built-in chart if it can't start).
    fn web_sync(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        let Some(rect) = self.web_rect else { if let Some(w) = &self.web { w.hide(); } return };
        // Anything egui draws above the panels (menus, drop-downs, dialogs, tooltips) would be hidden behind the web view,
        // so step aside while one is open over the chart.
        let over = |p: egui::Pos2| ctx.layer_id_at(p).map_or(false, |l| l.order != egui::Order::Background);
        let (l, r, t, b, c) = (rect.left()+4.0, rect.right()-4.0, rect.top()+4.0, rect.bottom()-4.0, rect.center());
        let _ = c;
        // a dialog is always on top of the chart's side of the screen, so step aside for them outright; for menus / popups
        // look at a grid of points over the chart (a small dialog can sit between a few sample points)
        let dialog_open = self.confirm.is_some() || self.ice_confirm.is_some() || self.ice_edit.is_some() || self.show_about || self.show_workflows || self.alert_dlg.is_some() || self.sell_inv_confirm.is_some();
        let covered = dialog_open || ctx.memory(|m| m.any_popup_open()) || (0..9).any(|ix| (0..7).any(|iy| over(egui::pos2(l + (r - l) * ix as f32 / 8.0, t + (b - t) * iy as f32 / 6.0))));
        if covered { if let Some(w) = &self.web { w.hide(); } return; }
        let ppp = ctx.pixels_per_point() as f64;
        let (x, y, w, h) = (rect.left() as f64 * ppp, rect.top() as f64 * ppp, rect.width() as f64 * ppp, rect.height() as f64 * ppp);
        if self.web.is_none() {
            match webchart::WebChart::new(frame, x, y, w, h) {
                Ok(wc) => self.web = Some(wc),
                Err(e) => { self.web_failed = true; self.say(RED, format!("web chart unavailable, using the built-in chart: {e}")); return; }
            }
        }
        let mut events = vec![];
        if let Some(wc) = self.web.as_mut() { events = wc.poll(); wc.set_bounds(x, y, w, h); wc.set_visible(true); }
        // until the chart page has loaded, show a chart-shaped skeleton in its place (and keep the blank page out of sight)
        if self.web.as_ref().map_or(false, |wc| !wc.ready) {
            if let Some(wc) = &self.web { wc.hide(); }
            fx::skeleton_chart(ctx, &ctx.layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("web_skeleton"))), rect);
            ctx.request_repaint_after(Duration::from_millis(33));
        }
        for m in events {
            let Ok(v) = serde_json::from_str::<Value>(&m) else { continue };
            self.web_msg(&v);
        }
        if let (Some(wc), Some((key, json))) = (self.web.as_ref(), self.web_json.as_ref()) {
            if wc.ready && !json.is_empty() && self.web_pushed.as_ref().map_or(true, |(k, t)| k != key || t.elapsed() > if self.candle_tf < 0 { Duration::from_millis(1000) } else { Duration::from_secs(3) }) {
                wc.push(json);
                self.web_pushed = Some((key.clone(), Instant::now()));
            }
        }
    }

    /// JSON the web chart renders: candles or ask/bid ticks for the chosen range, plus the toolbar's state.
    /// A pick in the chart page's toolbar.
    fn web_msg(&mut self, v: &Value) {
        if std::env::var("BB_CHART_DEBUG").is_ok() { eprintln!("chart msg {v}"); }
        match v["k"].as_str().unwrap_or("") {
            "range" => { if let Some(n) = v["v"].as_f64() { self.chart_minutes = n; } }
            "tf" => { if let Some(n) = v["v"].as_i64() { self.candle_tf = n; } }
            "series" => { if let Some(n) = v["v"].as_u64() { self.candle_series = n as u8; } }
            "type" => { let t = v["v"].as_str().unwrap_or("candles"); self.chart_candles = t != "line"; if t != "line" { self.web_style = t.into(); } }
            _ => {}
        }
    }

    fn web_payload(&mut self, id: &str) -> String {
        let t0 = now_secs();
        let h = self.history.get(id).cloned().unwrap_or_default();
        // Line + a seconds size: one point per second from this window's per-second samples
        let h = if self.chart_candles { h } else if self.candle_tf < 0 { self.fine.get(id).map(|d| d.iter().copied().collect()).unwrap_or_default() } else { self.line_history(id, &h) };
        let ranges = [("1D",1440.0),("3D",4320.0),("1M",43200.0),("6M",259200.0),("YTD",525600.0),("1Y",525601.0),("5Y",2628000.0),("Max",5256000.0)];
        let tfs = [("Auto",0i64),("1s",-1),("15s",-15),("30s",-30),("1m",1),("5m",5),("15m",15),("1h",60),("4h",240),("1d",1440)];
        let range_label = ranges.iter().find(|r| r.1 == self.chart_minutes).map(|r| r.0).unwrap_or("Range");
        let ty = if self.chart_candles { self.web_style.clone() } else { "line".to_string() };
        let tf_label = tf_name(self.effective_tf());
        let (mut candles, mut ask, mut bid) = (vec![], vec![], vec![]);
        if self.chart_candles {
            let cs = self.candles_for(id, &h, t0);
            // second sizes are re-sent every second: only the newest ~900 bars (15 min of 1 s), or the page lags and drops clicks
            let skip = if self.candle_tf < 0 { cs.len().saturating_sub(900) } else { 0 };
            for c in cs.into_iter().skip(skip) { let s = c.series(self.candle_series); candles.push(json!({"time": c.t, "open": s[0], "high": s[1], "low": s[2], "close": s[3]})); }
        } else {
            let rows: Vec<_> = h.iter().filter(|r| (t0 - r.0) / 60.0 <= self.chart_minutes).collect();
            let rows: Vec<_> = if self.candle_tf < 0 { rows[rows.len().saturating_sub(900)..].to_vec() } else { rows };
            let stride = (rows.len() / 2500).max(1);
            let mut last_t = 0i64;
            for r in rows.iter().step_by(stride) { let t = r.0 as i64; if t <= last_t { continue; } last_t = t; ask.push(json!({"time": t, "value": r.1})); bid.push(json!({"time": t, "value": r.2})); }
        }
        json!({"sym": short_id(id), "rangeMin": self.chart_minutes, "rangeLabel": range_label, "type": ty, "tfMin": self.candle_tf, "tfLabel": tf_label,
               "seriesIdx": self.candle_series, "ranges": ranges.iter().map(|r| json!([r.0, r.1])).collect::<Vec<_>>(), "tfs": tfs.iter().map(|r| json!([r.0, r.1])).collect::<Vec<_>>(),
               "candles": candles, "ask": ask, "bid": bid}).to_string()
    }

    /// Quote header, chart toolbar and price chart, laid out in the mockup's pixel coordinates
    /// (x from the panel's left or right edge, y from its top or bottom).
    fn quote_chart(&mut self, ui: &mut egui::Ui, area: egui::Rect, id: &str, p: &Product) {
        if self.chart_symbol != id { self.chart_view=None; self.price_view=None; self.chart_symbol=id.into(); }
        let (a,b)=(p.ask(),p.bid());
        if !self.web_failed && !self.chart_table {
            // the whole panel (toolbar, OHLC row, chart) is the ZTrader chart page
            self.chart_view=None; self.price_view=None;
            let rect=egui::Rect::from_min_max(egui::pos2(area.left()-2.0,area.top()-4.0),egui::pos2(area.right()+1.0,area.bottom()+1.0));
            self.web_rect=Some(rect);
            let key=format!("{id}|{}|{}|{}|{}|{}",self.chart_minutes,self.candle_tf,self.candle_series,self.chart_candles,self.web_style);
            let every=if self.candle_tf<0{Duration::from_millis(1000)}else{Duration::from_secs(3)};   // second candles: push every second so the newest bar ticks
            let need=self.web_pushed.as_ref().map_or(true,|(k,t)|*k!=key||t.elapsed()>every);
            let json=if need {self.web_payload(id)} else {String::new()};
            self.web_json=Some((key,json));
            return;
        }
        let o=area.min-egui::vec2(8.0,9.0);
        let sx=(area.width()+8.0)/808.0;
        let at=|x:f32,y:f32|o+egui::vec2(x*sx,y);
        let rx=|x:f32|o.x+x*sx;           // mockup x measured from the right edge
        let by=|y:f32|area.bottom()-(333.0-y);          // mockup y measured from the bottom edge
        let font=|sz:f32|FontId::proportional(sz);
        let bold=|sz:f32|FontId::new(sz,egui::FontFamily::Name("terminal-bold".into()));
        let pt=ui.painter().clone();
        // ---- ZTrader chart app layout: toolbar (symbol, range, type, candle size, indicators), OHLC row, chart ----
        let (zt_card,zt_border,zt_dim,zt_muted,zt_strong,zt_up,zt_dn)=(Color32::from_rgba_unmultiplied(2,3,3,220),Color32::from_rgb(70,83,80),Color32::from_rgb(178,189,183),Color32::from_rgb(125,137,131),Color32::from_rgb(251,253,247),Color32::from_rgb(8,153,129),Color32::from_rgb(242,54,69));
        pt.rect_filled(egui::Rect::from_min_max(egui::pos2(area.left()-4.0,at(0.0,5.0).y),egui::pos2(area.right(),by(333.0)+400.0)),0.0,Color32::from_rgb(1,2,2));
        let now=now_secs();
        let tf_label=tf_name(self.effective_tf());
        let ty=at(0.0,10.0).y; let th=30.0;
        let mut x=area.left()+6.0;
        // a ZTrader-style button: dark card, thin border, 4px radius; returns the rect
        let zbtn=|ui:&mut egui::Ui,x:&mut f32,w:f32,key:&str,active:bool|->(egui::Rect,bool){
            let r=egui::Rect::from_min_size(egui::pos2(*x,ty),egui::vec2(w,th)); *x+=w+6.0;
            let resp=ui.interact(r,ui.id().with(("zbtn",key)),egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
            ui.painter().rect_filled(r,4.0,if resp.hovered(){Color32::from_rgb(24,32,32)}else{zt_card});
            ui.painter().rect_stroke(r,4.0,Stroke::new(1.0,if active{Color32::from_rgb(131,165,179)}else{zt_border}),egui::StrokeKind::Inside);
            (r,resp.clicked())
        };
        // symbol button: BAZAAR  NAME  [tf]
        let symtxt=short_id(id);
        let sw=pt.layout_no_wrap(symtxt.clone(),bold(21.0),zt_strong).size().x;
        let tfw=pt.layout_no_wrap(tf_label.clone(),font(15.0),zt_dim).size().x;
        let (r,_)=zbtn(ui,&mut x,14.0+62.0+sw+10.0+tfw+16.0,"sym",false);
        pt.text(egui::pos2(r.left()+9.0,r.center().y),egui::Align2::LEFT_CENTER,"BAZAAR",font(13.0),zt_muted);
        pt.text(egui::pos2(r.left()+72.0,r.center().y),egui::Align2::LEFT_CENTER,&symtxt,bold(21.0),zt_strong);
        let badge=egui::Rect::from_min_size(egui::pos2(r.left()+72.0+sw+8.0,r.center().y-10.0),egui::vec2(tfw+10.0,20.0));
        pt.rect_filled(badge,3.0,Color32::from_rgba_unmultiplied(225,236,231,46));
        pt.text(badge.center(),egui::Align2::CENTER_CENTER,&tf_label,font(15.0),zt_dim);
        // range dropdown
        let ranges=[("1D",1440.0),("3D",4320.0),("1M",43200.0),("6M",259200.0),("YTD",525600.0),("1Y",525601.0),("5Y",2628000.0),("Max",5256000.0)];
        let cur=ranges.iter().find(|r|r.1==self.chart_minutes).map(|r|r.0).unwrap_or("Range");
        let (rr,clicked)=zbtn(ui,&mut x,86.0,"range",false);
        pt.text(egui::pos2(rr.left()+10.0,rr.center().y),egui::Align2::LEFT_CENTER,cur,font(18.0),zt_dim);
        pt.text(egui::pos2(rr.right()-12.0,rr.center().y),egui::Align2::CENTER_CENTER,"▾",font(12.0),zt_muted);
        if clicked { fx::dd_toggle(ui.ctx(),ui.id().with("rangepop")); }
        fx::dd_popup(&ui.ctx().clone(),ui.id().with("rangepop"),(ui.interact(rr,ui.id().with("rangeanchor"),egui::Sense::hover())).rect,0.0,|ui|{
            ui.set_min_width(100.0);
            for (n,m) in ranges { if ui.selectable_label(self.chart_minutes==m,n).clicked() { self.chart_minutes=m; self.chart_view=None; self.price_view=None; } }
        });
        // chart type dropdown
        let tname=if self.chart_table{"Table"}else if self.chart_candles{"Candles"}else{"Line"};
        let (tr,clicked)=zbtn(ui,&mut x,112.0,"type",false);
        pt.text(egui::pos2(tr.left()+10.0,tr.center().y),egui::Align2::LEFT_CENTER,tname,font(18.0),zt_dim);
        pt.text(egui::pos2(tr.right()-12.0,tr.center().y),egui::Align2::CENTER_CENTER,"▾",font(12.0),zt_muted);
        if clicked { fx::dd_toggle(ui.ctx(),ui.id().with("typepop")); }
        fx::dd_popup(&ui.ctx().clone(),ui.id().with("typepop"),(ui.interact(tr,ui.id().with("typeanchor"),egui::Sense::hover())).rect,0.0,|ui|{
            ui.set_min_width(110.0);
            if ui.selectable_label(self.chart_candles&&!self.chart_table,"Candles").clicked() { self.chart_table=false; self.chart_candles=true; }
            if ui.selectable_label(!self.chart_candles&&!self.chart_table,"Line").clicked() { self.chart_table=false; self.chart_candles=false; }
            if ui.selectable_label(self.chart_table,"Table").clicked() { self.chart_table=true; }
        });
        // candle size (only meaningful for candles)
        let (kr,clicked)=zbtn(ui,&mut x,92.0,"tick",false);
        pt.text(egui::pos2(kr.left()+10.0,kr.center().y),egui::Align2::LEFT_CENTER,if self.chart_candles{tf_label.as_str()}else{"Tick"},font(18.0),zt_dim);
        pt.text(egui::pos2(kr.right()-12.0,kr.center().y),egui::Align2::CENTER_CENTER,"▾",font(12.0),zt_muted);
        if clicked { fx::dd_toggle(ui.ctx(),ui.id().with("tickpop")); }
        let tick_rect=kr;
        // indicators
        let (ir,clicked)=zbtn(ui,&mut x,118.0,"ind",false);
        pt.text(egui::pos2(ir.left()+10.0,ir.center().y),egui::Align2::LEFT_CENTER,format!("Price: {}",["Ask","Bid","Mid"][self.candle_series.min(2) as usize]),font(18.0),zt_dim);
        pt.text(egui::pos2(ir.right()-12.0,ir.center().y),egui::Align2::CENTER_CENTER,"▾",font(12.0),zt_muted);
        if clicked { fx::dd_toggle(ui.ctx(),ui.id().with("studypop")); }
        let study_rect=ir;
        // reset
        let (zr,clicked)=zbtn(ui,&mut x,34.0,"reset",false);
        pt.text(zr.center(),egui::Align2::CENTER_CENTER,"⟲",font(20.0),zt_dim);
        if clicked { self.chart_ask=true;self.chart_bid=true;self.chart_table=false;self.chart_view=None;self.price_view=None; }
        // OHLC row (the candle under the pointer, else the latest one) / quote row for the line chart
        {
            let y=at(0.0,52.0).y+8.0; let mut cx=area.left()+12.0;
            let mut put=|pt:&egui::Painter,t:String,c:Color32,f:FontId|{ let w=pt.layout_no_wrap(t.clone(),f.clone(),c).size().x; pt.text(egui::pos2(cx,y),egui::Align2::LEFT_CENTER,t,f,c); cx+=w+12.0; };
            if self.chart_candles {
                let tf=tf_minutes(self.effective_tf()); let t0=now;
                let cs=self.candles_for(id,&self.history.get(id).cloned().unwrap_or_default(),t0);
                let hov=ui.input(|i|i.pointer.hover_pos());
                let pick=cs.last().cloned();
                if let Some(c)=pick { let s=c.series(self.candle_series); let _=tf;
                    let chg=s[3]-s[0]; let pct=if s[0]!=0.0{chg/s[0]*100.0}else{0.0};
                    put(&pt,short_id(id),zt_strong,bold(16.0));
                    for (k,v,col) in [("O",s[0],Color32::from_rgb(230,235,231)),("H",s[1],zt_up),("L",s[2],zt_dn),("C",s[3],Color32::from_rgb(230,235,231))] {
                        put(&pt,format!("{k}"),zt_muted,font(16.0)); put(&pt,format!("{v:.1}"),col,bold(16.0));
                    }
                    put(&pt,format!("{chg:+.1} ({pct:+.2}%)"),if chg>=0.0{zt_up}else{zt_dn},bold(16.0));
                }
                let _=hov;
            } else {
                let change=self.history.get(id).and_then(|h|h.iter().find(|r|now-r.0<=86400.0 && r.2>0.0)).filter(|r|now-r.0>=60.0).map(|r|(b-r.2,(b-r.2)/r.2*100.0));
                put(&pt,short_id(id),zt_strong,bold(16.0));
                put(&pt,"Ask".into(),zt_muted,font(16.0)); put(&pt,format!("{a:.1}"),zt_up,bold(16.0));
                put(&pt,"Bid".into(),zt_muted,font(16.0)); put(&pt,format!("{b:.1}"),zt_dn,bold(16.0));
                put(&pt,"Sprd".into(),zt_muted,font(16.0)); put(&pt,format!("{:.1}",a-b),Color32::from_rgb(230,235,231),bold(16.0));
                if let Some((c,pc))=change { put(&pt,format!("{c:+.1} ({pc:+.2}%)"),if c>=0.0{zt_up}else{zt_dn},bold(16.0)); }
            }
        }
        let anchor=ui.interact(tick_rect,ui.id().with("tickanchor"),egui::Sense::hover());
        fx::dd_popup(&ui.ctx().clone(),ui.id().with("tickpop"),(anchor).rect,0.0,|ui|{
            ui.set_min_width(260.0);
            if self.chart_candles {
                ui.label("Candle size");
                for (m,name) in [(0,"Auto (fits the range)"),(-1,"1 second"),(-15,"15 seconds"),(-30,"30 seconds"),(1,"1 minute"),(5,"5 minutes"),(15,"15 minutes"),(60,"1 hour"),(240,"4 hours"),(1440,"1 day")] {
                    if ui.radio_value(&mut self.candle_tf,m,name).clicked() { fx::dd_close_all(ui.ctx()); }
                }
                ui.label(RichText::new("Candles come from the recorder (blockberg-rec) plus this session's live prices. 1s/15s/30s use the prices this window has seen since it opened (the last 3 hours, sampled every second).").small().color(DIM));
            } else {
                ui.label("Live observations / 10-second refresh");
                ui.label("Click the chart-type icon (right) for candlesticks.");
            }
        });
        let anchor=ui.interact(study_rect,ui.id().with("studyanchor"),egui::Sense::hover());
        fx::dd_popup(&ui.ctx().clone(),ui.id().with("studypop"),(anchor).rect,0.0,|ui|{
            if self.chart_candles {
                ui.label("Candle price");
                ui.radio_value(&mut self.candle_series,0,"Ask  (insta-buy)");
                ui.radio_value(&mut self.candle_series,1,"Bid  (insta-sell)");
                ui.radio_value(&mut self.candle_series,2,"Mid");
            } else { ui.fx_checkbox(&mut self.chart_ask,"Ask");ui.fx_checkbox(&mut self.chart_bid,"Bid"); }
        });

        // chart
        let plot=egui::Rect::from_min_max(egui::pos2(rx(95.0),at(0.0,92.0).y),egui::pos2(rx(770.0),by(310.0)));
        let h=self.history.get(id).cloned().unwrap_or_default();
        let h=if self.chart_candles { h } else { self.line_history(id,&h) };
        let t0=now_secs();
        let rows:Vec<_>=h.iter().filter(|r|self.chart_view.map(|(l,rgt)|{let m=(r.0-t0)/60.0;m>=l-1.0 && m<=rgt+1.0}).unwrap_or((t0-r.0)/60.0<=self.chart_minutes)).collect();
        let cs:Vec<candles::Candle>=if self.chart_candles { self.candles_for(id,&h,t0) } else { vec![] };
        if self.chart_table {
            ui.scope_builder(egui::UiBuilder::new().max_rect(plot.expand2(egui::vec2(60.0,0.0))),|ui|{
                egui::ScrollArea::vertical().show(ui,|ui|{egui::Grid::new("price_history_table").striped(true).show(ui,|ui|{
                    for text in ["Minutes ago","Ask","Bid"]{ui.label(text);}ui.end_row();
                    for row in rows.iter().rev().take(1440){ui.label(format!("{:.1}",(t0-row.0)/60.0));ui.label(format!("{:.1}",row.1));ui.label(format!("{:.1}",row.2));ui.end_row();}
                });});
            });
            return;
        }
        let span=if self.chart_candles { cs.first().map(|c|(t0-c.t as f64)/60.0).unwrap_or(10.0).max(1.0) } else { rows.first().map(|r|(t0-r.0)/60.0).unwrap_or(10.0).max(1.0) };
        let step=if span<=60.0{10.0}else if span<=180.0{30.0}else if span<=720.0{120.0}else{360.0};
        let (x_min,x_max)=self.chart_view.unwrap_or((-((span/step).ceil()*step).max(step)*1.013,0.0));
        let cs:Vec<_>=cs.into_iter().filter(|c|{let m=(c.t as f64-t0)/60.0;m>=x_min-1.0 && m<=x_max+1.0}).collect();
        let (mut lo,mut hi)=(f64::MAX,f64::MIN);
        if self.chart_candles { for c in &cs { let s=c.series(self.candle_series); lo=lo.min(s[2] as f64); hi=hi.max(s[1] as f64); } }
        else { for r in &rows { if self.chart_ask{lo=lo.min(r.1);hi=hi.max(r.1);} if self.chart_bid{lo=lo.min(r.2);hi=hi.max(r.2);} } }
        if lo>hi {lo=b.min(a);hi=a.max(b);}
        let raw=((hi-lo)/5.0).max(1e-9);
        let pw=10f64.powf(raw.log10().floor());
        let ystep=pw*[1.0,2.0,5.0,10.0].into_iter().find(|m|raw<=pw*m).unwrap_or(10.0);
        let (y_min,y_max)=((lo/ystep).floor()*ystep,(hi/ystep).ceil()*ystep.max(1e-9));
        let y_max=if y_max<=y_min{y_min+ystep}else{y_max};
        let (y_min,y_max)=self.price_view.unwrap_or((y_min,y_max));
        let raw=((y_max-y_min)/5.0).max(1e-9);
        let pow=10f64.powf(raw.log10().floor());
        let ystep=pow*[1.0,2.0,5.0,10.0].into_iter().find(|m|raw<=pow*m).unwrap_or(10.0);
        let step=[1.0,2.0,5.0,10.0,15.0,30.0,60.0,120.0,240.0,360.0,720.0,1440.0,2880.0,10080.0,20160.0,43200.0].into_iter().find(|v|(x_max-x_min)/v<=4.5).unwrap_or(43200.0);
        let px=|m:f64|plot.left()+((m-x_min)/(x_max-x_min)) as f32*plot.width();
        let py=|v:f64|plot.bottom()-((v-y_min)/(y_max-y_min)) as f32*plot.height();
        // ZTrader (TradingView lightweight-charts) look: near-black, faint solid grid, muted axis border, teal/red candles
        let (zt_text,zt_grid,zt_border,zt_cross,zt_up,zt_dn)=(Color32::from_rgb(230,235,231),Color32::from_rgba_unmultiplied(225,236,231,28),Color32::from_rgb(70,83,80),Color32::from_rgb(145,160,154),Color32::from_rgb(8,153,129),Color32::from_rgb(242,54,69));
        pt.rect_filled(plot.expand2(egui::vec2(60.0,0.0)),0.0,Color32::from_rgb(1,2,2));
        let grid=Stroke::new(1.0,zt_grid);
        let last_for_axis:f64=if self.chart_candles { self.candles_for(id,&self.history.get(id).cloned().unwrap_or_default(),now).last().map(|c|c.series(self.candle_series)[3] as f64).unwrap_or(a) } else { a };
        let mut v=(y_min/ystep).ceil()*ystep; while v<=y_max+ystep*0.01 {
            let y=py(v);
            pt.line_segment([egui::pos2(plot.left(),y),egui::pos2(plot.right(),y)],grid);
            // the last-price tag sits on the axis too: skip a grid label it would cover (flat data gives identical numbers)
            if (y-py(last_for_axis)).abs()>12.0 { pt.text(egui::pos2(rx(57.0),y),egui::Align2::RIGHT_CENTER,format!("{v:.1}"),font(18.0),zt_text); }
            v+=ystep;
        }
        let mut m=(x_max/step).floor()*step-step; while m>x_min { 
            let x=px(m);
            pt.line_segment([egui::pos2(x,plot.top()),egui::pos2(x,plot.bottom())],grid);
            pt.text(egui::pos2(x,by(325.0)),egui::Align2::CENTER_CENTER,format_ago(m),font(18.0),zt_text);
            m-=step;
        }
        pt.text(egui::pos2(plot.right(),by(325.0)),egui::Align2::CENTER_CENTER,format_ago(x_max),font(18.0),zt_text);
        pt.line_segment([plot.left_top(),plot.left_bottom()],Stroke::new(1.0,zt_border));
        pt.line_segment([plot.left_bottom(),plot.right_bottom()],Stroke::new(1.0,zt_border));
        pt.line_segment([plot.right_top(),plot.right_bottom()],Stroke::new(1.0,zt_border));
        let clip=pt.with_clip_rect(plot);
        if self.chart_candles {
            let tf=tf_minutes(self.effective_tf());
            let cw=((px(0.0)-px(-tf))*0.72).clamp(1.5,26.0);
            if cs.is_empty() { clip.text(plot.center(),egui::Align2::CENTER_CENTER,"collecting candles…  (the recorder fills this in as time passes)",font(20.0),DIM); }
            for c in &cs {
                let s=c.series(self.candle_series);
                let x=px((c.t as f64+tf*30.0-t0)/60.0);
                let col=if s[3]>=s[0]{zt_up}else{zt_dn};
                clip.line_segment([egui::pos2(x,py(s[1] as f64)),egui::pos2(x,py(s[2] as f64))],Stroke::new(1.0,col));
                let (top,bot)=(py(s[0].max(s[3]) as f64),py(s[0].min(s[3]) as f64));
                clip.rect_filled(egui::Rect::from_min_max(egui::pos2(x-cw/2.0,top),egui::pos2(x+cw/2.0,bot.max(top+1.5))),0.0,col);
            }
            // hover: open / high / low / close of the candle under the pointer
            let hover=ui.interact(plot,ui.id().with("candle_hover"),egui::Sense::hover());
            if let Some(pos)=hover.hover_pos() {
                let m=x_min+((pos.x-plot.left()) as f64/plot.width() as f64)*(x_max-x_min);
                let tpt=t0+m*60.0;
                if let Some(c)=cs.iter().min_by(|a,b|((a.t as f64+tf*30.0)-tpt).abs().total_cmp(&((b.t as f64+tf*30.0)-tpt).abs())) {
                    let s=c.series(self.candle_series);
                    let ago=((t0-c.t as f64)/60.0).max(0.0);
                    hover.on_hover_ui_at_pointer(|ui|{
                        ui.label(RichText::new(format!("{} ago · {} candle",if ago<120.0{format!("{ago:.0} min")}else{format!("{:.1} h",ago/60.0)},tf_name(self.effective_tf()))).color(DIM));
                        ui.label(format!("O {:.1}   H {:.1}   L {:.1}   C {:.1}",s[0],s[1],s[2],s[3]));
                        ui.label(RichText::new(format!("{} price snapshots",c.n)).small().color(DIM));
                    });
                }
            }
        } else {
        for (on,col,f) in [(self.chart_bid,zt_dn,2usize),(self.chart_ask,zt_up,1usize)] {
            if !on {continue}
            let mut pts:Vec<_>=rows.iter().map(|r|egui::pos2(px((r.0-t0)/60.0).max(plot.left()),py(if f==1{r.1}else{r.2}))).collect();
            if let Some(first)=pts.first().copied().filter(|_|self.chart_view.is_none()) { pts.insert(0,egui::pos2(plot.left(),first.y)); }
            if pts.len()>1 { clip.add(egui::Shape::line(pts,Stroke::new(1.8,col))); }
        }
        }
        // last price: dashed line across the chart with a coloured tag on the price axis
        {
            let last=if self.chart_candles { cs.last().map(|c|(c.series(self.candle_series)[3] as f64,c.series(self.candle_series)[3]>=c.series(self.candle_series)[0])) } else { Some((a,true)) };
            if let Some((lp,up))=last.filter(|(v,_)|*v>=y_min && *v<=y_max) {
                let col=if up{zt_up}else{zt_dn}; let y=py(lp);
                clip.extend(egui::Shape::dashed_line(&[egui::pos2(plot.left(),y),egui::pos2(plot.right(),y)],Stroke::new(1.0,zt_cross),4.0,4.0));
                let tag=egui::Rect::from_center_size(egui::pos2(plot.left()-34.0,y),egui::vec2(60.0,20.0));
                pt.rect_filled(tag,2.0,col); pt.text(tag.center(),egui::Align2::CENTER_CENTER,format!("{lp:.1}"),font(17.0),Color32::WHITE);
            }
        }
        // crosshair: dashed lines with price / time labels on the axes
        if let Some(pos)=ui.input(|i|i.pointer.hover_pos()).filter(|p|plot.contains(*p)) {
            clip.extend(egui::Shape::dashed_line(&[egui::pos2(plot.left(),pos.y),egui::pos2(plot.right(),pos.y)],Stroke::new(1.0,zt_cross),4.0,4.0));
            clip.extend(egui::Shape::dashed_line(&[egui::pos2(pos.x,plot.top()),egui::pos2(pos.x,plot.bottom())],Stroke::new(1.0,zt_cross),4.0,4.0));
            let pv=y_min+((plot.bottom()-pos.y)/plot.height()) as f64*(y_max-y_min);
            let tm=x_min+((pos.x-plot.left())/plot.width()) as f64*(x_max-x_min);
            let tag=egui::Rect::from_center_size(egui::pos2(plot.left()-34.0,pos.y),egui::vec2(60.0,20.0));
            pt.rect_filled(tag,2.0,Color32::from_rgb(9,11,11)); pt.rect_stroke(tag,2.0,Stroke::new(1.0,zt_border),egui::StrokeKind::Inside);
            pt.text(tag.center(),egui::Align2::CENTER_CENTER,format!("{pv:.1}"),font(17.0),zt_text);
            let ttxt=format_ago(tm); let tw=pt.layout_no_wrap(ttxt.clone(),font(17.0),zt_text).size().x+14.0;
            let tt=egui::Rect::from_center_size(egui::pos2(pos.x,by(325.0)),egui::vec2(tw,20.0));
            pt.rect_filled(tt,2.0,Color32::from_rgb(9,11,11)); pt.rect_stroke(tt,2.0,Stroke::new(1.0,zt_border),egui::StrokeKind::Inside);
            pt.text(tt.center(),egui::Align2::CENTER_CENTER,ttxt,font(17.0),zt_text);
        }
        // Mouse gestures for the chart drag handles.
        let gesture=ui.interact(plot,ui.id().with("chart_pan"),egui::Sense::drag()).on_hover_cursor(egui::CursorIcon::Grab);
        let delta=ui.input(|i|i.pointer.delta());
        if gesture.dragged() {
            let dx=delta.x as f64/plot.width() as f64*(x_max-x_min);
            let dy=delta.y as f64/plot.height() as f64*(y_max-y_min);
            self.chart_view=Some((x_min-dx,x_max-dx));self.price_view=Some((y_min+dy,y_max+dy));
        }
        let yaxis=egui::Rect::from_min_max(egui::pos2(rx(5.0),plot.top()),plot.left_bottom());
        if ui.interact(yaxis,ui.id().with("price_axis"),egui::Sense::drag()).on_hover_cursor(egui::CursorIcon::ResizeVertical).dragged(){
            let mid=(y_min+y_max)/2.0;let half=(y_max-y_min)/2.0*(delta.y as f64*0.01).exp();self.price_view=Some((mid-half,mid+half));
        }
        let xaxis=egui::Rect::from_min_max(plot.left_bottom(),egui::pos2(plot.right(),area.bottom()));
        let axis=ui.interact(xaxis,ui.id().with("time_axis"),egui::Sense::drag()).on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
        let scroll=if gesture.hovered(){ui.input(|i|i.smooth_scroll_delta.y)}else{0.0};
        if axis.dragged() || scroll!=0.0 {let span=((x_max-x_min)*(delta.x as f64*0.01-scroll as f64*0.002).exp()).clamp(2.0,86400.0);self.chart_view=Some((x_max-span,x_max));}
        for (i,label) in ["⟲","+","−"].iter().enumerate(){
            let r=egui::Rect::from_min_size(plot.right_bottom()-egui::vec2(42.0+i as f32*40.0,38.0),egui::vec2(34.0,30.0));
            if ui.put(r,egui::Button::new(*label).fill(Color32::from_gray(30))).clicked(){
                if i==0 {self.chart_view=None;self.price_view=None;}else{let span=((x_max-x_min)*if i==1{0.8}else{1.25}).clamp(2.0,86400.0);self.chart_view=Some((x_max-span,x_max));}
            }
        }
        if self.chart_candles {return;}
        // legend
        let legend=egui::Rect::from_min_max(egui::pos2(rx(678.0),at(0.0,162.0).y),egui::pos2(rx(786.0),at(0.0,212.0).y));
        pt.rect_filled(legend,3.0,BG);
        pt.rect_stroke(legend,3.0,Stroke::new(1.0,Color32::from_gray(85)),egui::StrokeKind::Inside);
        for (i,name,value,col) in [(0,"Ask",a,GREEN),(1,"Bid",b,RED)] {
            let enabled=if i==0{self.chart_ask}else{self.chart_bid};
            let cy=legend.top()+14.0+i as f32*23.0;
            let row=egui::Rect::from_min_max(egui::pos2(legend.left(),cy-11.0),egui::pos2(legend.right(),cy+11.0));
            if ui.interact(row,ui.id().with(("quote_legend",i)),egui::Sense::click()).clicked(){if i==0{self.chart_ask=!self.chart_ask;}else{self.chart_bid=!self.chart_bid;}}
            pt.rect_filled(egui::Rect::from_center_size(egui::pos2(legend.left()+16.0,cy),egui::vec2(13.0,13.0)),1.0,if enabled{col}else{DIM});
            pt.text(egui::pos2(legend.left()+29.0,cy),egui::Align2::LEFT_CENTER,name,font(18.5),if enabled{Color32::WHITE}else{DIM});
            pt.text(egui::pos2(legend.right()-8.0,cy),egui::Align2::RIGHT_CENTER,format!("{value:.1}"),font(18.5),if enabled{Color32::WHITE}else{DIM});
        }
    }

    /// Minutes per candle: the chosen size, or one that gives roughly 150-300 candles for the range.
    /// Sample the selected item's price once a second, for the sub-minute candle sizes.
    fn fine_tick(&mut self) {
        let Some(id) = self.selected.clone() else { return };
        let Some(p) = self.products.get(&id) else { return };
        let (a, b, t) = (p.ask(), p.bid(), now_secs().floor());
        let d = self.fine.entry(id).or_default();
        if d.back().map_or(true, |l| t > l.0) { d.push_back((t, a, b)); }
        while d.len() > 3 * 3600 { d.pop_front(); }
    }

    fn effective_tf(&self) -> i64 {
        if self.candle_tf != 0 { return self.candle_tf; }   // negative = seconds (1s, 15s, 30s)
        for tf in [1i64, 5, 15, 60, 240, 1440] { if self.chart_minutes / tf as f64 <= 300.0 { return tf; } }
        1440
    }

    /// Candles for the chart: the recorder's saved minutes + this session's snapshots, grouped to the chosen size.
    fn candles_for(&mut self, id: &str, h: &[(f64, f64, f64)], t0: f64) -> Vec<candles::Candle> {
        let tf = self.effective_tf();
        if tf < 0 {   // seconds: only this window's per-second samples have that detail
            let fine: Vec<(f64, f64, f64)> = self.fine.get(id).map(|d| d.iter().copied().collect()).unwrap_or_default();
            return candles::from_snapshots_secs(&fine, -tf).into_iter().filter(|c| self.chart_view.is_some() || (t0 - c.t as f64) / 60.0 <= self.chart_minutes).collect();
        }
        let stale = self.candle_cache.get(id).map(|(t, _)| t.elapsed() > Duration::from_secs(30)).unwrap_or(true);
        if stale {
            let want = (self.chart_minutes.max(1440.0) as usize + 60).min(600_000);
            self.candle_cache.insert(id.to_string(), (Instant::now(), candles::load(id, want)));
        }
        let file = &self.candle_cache[id].1;
        let merged = candles::merge(file, &candles::from_snapshots(h));
        let agg = candles::aggregate(&merged, tf * 60);
        agg.into_iter().filter(|c| self.chart_view.is_some() || (t0 - c.t as f64) / 60.0 <= self.chart_minutes).collect()
    }

    /// Price history for the line chart: this session's snapshots, with the recorder's saved minutes (and anything
    /// imported) filling in everything before them, so ranges longer than the app has been open still have data.
    fn line_history(&mut self, id: &str, h: &[(f64, f64, f64)]) -> Vec<(f64, f64, f64)> {
        let stale = self.candle_cache.get(id).map(|(t, _)| t.elapsed() > Duration::from_secs(30)).unwrap_or(true);
        if stale { self.candle_cache.insert(id.to_string(), (Instant::now(), candles::load(id, 600_000))); }
        let first = h.first().map(|r| r.0).unwrap_or(f64::MAX);
        let mut out: Vec<(f64, f64, f64)> = self.candle_cache[id].1.iter().filter(|c| (c.t as f64) < first).map(|c| (c.t as f64, c.ask[3] as f64, c.bid[3] as f64)).collect();
        out.extend_from_slice(h);
        out
    }

    fn ticket(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("ticket_reference").show_separator_line(false).frame(terminal_frame().inner_margin(0)).resizable(true).default_width((ctx.screen_rect().width() * 0.2056).max(300.0)).width_range(280.0..=720.0).show(ctx, |ui| {
            egui::ScrollArea::vertical().id_salt("ticket_scroll").show(ui,|ui| {
            let needed=if self.exchange==0 && self.iceberg {640.0} else if self.exchange==0 && self.kind==Kind::Limit {840.0}else{640.0};
            let height=ui.available_height().max(needed);
            let (full,_)=ui.allocate_exact_size(egui::vec2(ui.available_width(),height),egui::Sense::hover());
            let o=full.min-egui::vec2(4.0,6.0);           // mockup crop coordinates
            let w=(full.width()-343.0).max(-40.0);      // extra width beyond the mockup's
            let at=|x:f32,y:f32|o+egui::vec2(x,y);
            let rr=|x0:f32,y0:f32,x1:f32,y1:f32|egui::Rect::from_min_max(at(x0,y0),at(x1+w,y1));
            let font=|sz:f32|FontId::proportional(sz);
            let bold=|sz:f32|FontId::new(sz,egui::FontFamily::Name("terminal-bold".into()));
            let pt=ui.painter().clone();
            // heading
            let head=rr(6.0,7.0,337.0,38.0);
            gradient(ui,head,Color32::from_rgb(255,194,16),Color32::from_rgb(255,155,0));
            pt.text(at(18.0,23.0),egui::Align2::LEFT_CENTER,"Order Ticket",bold(25.5),Color32::BLACK);
            let sel=self.selected.clone().unwrap_or_default();
            pt.line_segment([at(176.0,12.0),at(176.0,34.0)],Stroke::new(2.0,Color32::BLACK));
            if let Some(ic)=&self.icons { ic.paint(&pt,&sel,egui::Rect::from_center_size(at(204.0,23.0),egui::vec2(22.0,22.0))); }
            { let name=pretty_s(&sel).to_uppercase(); let mut sz=19.5f32;
              while sz>12.0 && pt.layout_no_wrap(name.clone(),font(sz),Color32::BLACK).size().x>(285.0+w-222.0) { sz-=1.0; }
              pt.text(at(222.0,23.0),egui::Align2::LEFT_CENTER,name,font(sz),Color32::BLACK); }
            { let c=at(320.0+w,23.0); let k=Stroke::new(2.0,Color32::BLACK);
              if ui.interact(egui::Rect::from_center_size(c,egui::vec2(30.0,30.0)),ui.id().with("tclose"),egui::Sense::click()).fx_tip("Close ticket (reopen from Layout or F4)").clicked(){self.show_ticket=false;}
              // pop-out style glyph: a box with an arrow leaving the top-right corner
              let q=|x:f32,y:f32|c+egui::vec2(x,y);
              pt.add(egui::Shape::line(vec![q(-2.0,-9.0),q(-10.0,-9.0),q(-10.0,10.0),q(9.0,10.0),q(9.0,2.0)],k));
              pt.line_segment([q(-1.0,1.0),q(10.0,-10.0)],k);
              pt.add(egui::Shape::line(vec![q(2.0,-10.0),q(10.0,-10.0),q(10.0,-2.0)],k)); }
            let Some(p) = self.products.get(&sel).cloned() else {
                if fx::loading_for(ui.ctx(),ui.id().with("tk_loading"),true,0.15) {
                    for (i,wd) in [(0.0f32,1.0f32),(1.0,0.7),(2.0,1.0),(3.0,0.5),(4.0,0.9)] { fx::skeleton(ui,rr(18.0,60.0+i*46.0,18.0+310.0*wd,90.0+i*46.0-8.0),4.0); }
                }
                return
            };
            if self.agent_up && !self.mock && self.last_inventory.elapsed()>Duration::from_secs(4){let _=self.cmd_tx.send(json!({"cmd":"invcount","product":sel}));self.last_inventory=Instant::now();}
            if self.exchange==2 {self.buy=false;}

            // buy / sell
            let buy_r=egui::Rect::from_min_max(at(17.0,51.0),at(172.0+w/2.0,87.0));
            let sell_r=egui::Rect::from_min_max(at(174.0+w/2.0,51.0),at(328.0+w,87.0));
            let buy_resp=ui.interact(buy_r,ui.id().with("buy"),egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
            let sell_resp=ui.interact(sell_r,ui.id().with("sell"),egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
            let g42=Color32::from_gray(42);
            let br=fx_bg(ui,&buy_resp,buy_r,if self.buy{GREEN}else{g42},if self.buy{Color32::from_rgb(90,255,90)}else{Color32::from_gray(66)},if self.buy{GREEN}else{g42},if self.buy{Color32::WHITE}else{GREEN.gamma_multiply(0.8)},3.0);
            let sr=fx_bg(ui,&sell_resp,sell_r,if !self.buy{RED}else{g42},if !self.buy{Color32::from_rgb(255,90,90)}else{Color32::from_gray(66)},if !self.buy{RED}else{g42},if !self.buy{Color32::WHITE}else{RED.gamma_multiply(0.8)},3.0);
            pt.text(br.center(),egui::Align2::CENTER_CENTER,"Buy",if self.buy{bold(22.0)}else{font(22.0)},if self.buy{Color32::BLACK}else{Color32::WHITE});
            pt.text(sr.center(),egui::Align2::CENTER_CENTER,"Sell",if !self.buy{bold(22.0)}else{font(22.0)},if !self.buy{Color32::BLACK}else{Color32::WHITE});
            if buy_resp.clicked() && self.exchange!=2{self.buy=true;}
            if sell_resp.clicked(){self.buy=false;self.iceberg=false;}
            // order type
            pt.text(at(20.0,140.0),egui::Align2::LEFT_CENTER,"Order Type",font(19.5),Color32::WHITE);
            // the three dropdowns share one look: black field, grey outline, orange text, a chevron that turns when open
            {
                let shown=if self.exchange==2{"NPC Sell (/trades)"}else if self.exchange==1{"—"}else if self.iceberg{"Iceberg"}else if self.kind==Kind::Instant{"Market"}else{"Limit Order"};
                fx::dropdown(ui,"otype",rr(115.0,127.0,327.0,153.0),shown,ORANGE,|ui|{
                    if self.exchange==0 {
                        if ui.selectable_label(!self.iceberg&&self.kind==Kind::Instant,"Market").clicked(){self.kind=Kind::Instant;self.iceberg=false;fx::dd_close_all(ui.ctx());}
                        if ui.selectable_label(!self.iceberg&&self.kind==Kind::Limit,"Limit Order").clicked(){self.kind=Kind::Limit;self.iceberg=false;fx::dd_close_all(ui.ctx());}
                        if ui.selectable_label(self.iceberg,"Iceberg").fx_tip("Buys a big amount in slices: places a buy order at the top bid + 0.1, re-quotes it as the price moves, then places the next slice").clicked(){self.iceberg=true;self.buy=true;fx::dd_close_all(ui.ctx());}
                    } else { ui.label("Fixed for this exchange"); }
                });
            }
            pt.text(at(20.0,105.0),egui::Align2::LEFT_CENTER,"Exchange",font(19.5),Color32::WHITE);
            fx::dropdown(ui,"exchange",rr(115.0,92.0,327.0,118.0),["Bazaar","Auction","Trades (NPC shop)"][self.exchange as usize],ORANGE,|ui|{
                for (v,n) in [(0u8,"Bazaar"),(1,"Auction (coming soon)"),(2,"Trades (NPC shop)")] { if ui.selectable_label(self.exchange==v,n).clicked(){self.exchange=v;fx::dd_close_all(ui.ctx());} }
            });
            // source row: where the goods come from when selling
            pt.text(at(20.0,174.0),egui::Align2::LEFT_CENTER,"Source",font(19.5),Color32::WHITE);
            fx::dropdown(ui,"source",rr(115.0,161.0,327.0,187.0),if self.source_sacks{"Sacks"}else{"Inventory"},ORANGE,|ui|{
                if ui.selectable_label(!self.source_sacks,"Inventory").clicked(){self.source_sacks=false;fx::dd_close_all(ui.ctx());}
                if ui.selectable_label(self.source_sacks,"Sacks").fx_tip("Selling pulls the goods out of your sacks first (/gfs)").clicked(){self.source_sacks=true;fx::dd_close_all(ui.ctx());}
            });
            if self.exchange==1 {pt.text(at(18.0,256.0),egui::Align2::LEFT_CENTER,"Auction House — coming soon",font(20.0),DIM);return;}
            if self.exchange==0 && self.iceberg {
                // iceberg order: every setting lives here (same fields as the Queue screen)
                self.ice_form.product=sel.clone();
                let rows:[(&str,&str,u8);6]=[("Total","2m",0),("Price min","0.1",1),("Price max","1.5",2),("Over top bid","0.1",3),("Requote (min)","5",4),("Max per order","71680",5)];
                for (i,(label,hint,which)) in rows.into_iter().enumerate() {
                    let y=205.0+i as f32*32.0;
                    pt.text(at(20.0,y+12.0),egui::Align2::LEFT_CENTER,label,font(18.0),Color32::WHITE);
                    let f=match which {0=>&mut self.ice_form.total,1=>&mut self.ice_form.min,2=>&mut self.ice_form.max,3=>&mut self.ice_form.offset,4=>&mut self.ice_form.requote,_=>&mut self.ice_form.slice};
                    let fr=ui.put(rr(168.0,y,327.0,y+25.0),egui::TextEdit::singleline(f).hint_text(RichText::new(hint).color(Color32::from_gray(85))).horizontal_align(egui::Align::RIGHT).font(font(19.0)));
                    let txt=match which {0=>&self.ice_form.total,1=>&self.ice_form.min,2=>&self.ice_form.max,3=>&self.ice_form.offset,4=>&self.ice_form.requote,_=>&self.ice_form.slice};
                    fx::focus_ring(ui,&fr,iceberg::ice_field_valid(which,txt));
                }
                let parsed=self.ice_parse_form();
                let (msg,col)=match &parsed {
                    Ok(j)=>(format!("Buys up to {} {} in {} slice(s) of up to {}, top bid + {:.1}, never above {:.1}. Worst case {} coins.",grouped(j.total),pretty_s(&j.product),j.total.div_ceil(j.slice.max(1)),grouped(j.slice),j.offset,j.max_price,fmt_coins(j.total as f64*j.max_price)),DIM),
                    Err(e)=>(e.clone(),DIM)};
                ui.put(rr(176.0,399.0,327.0,421.0),egui::Checkbox::new(&mut self.ice_form.on_outbid,"Requote if outbid")).fx_tip("Requotes as soon as someone bids above your order (at most every 30 s), not only on the timer. Prices lag the game by 10–20 s.");
                ui.put(rr(18.0,399.0,170.0,421.0),egui::Checkbox::new(&mut self.ice_form.auto_claim,"Auto-claim")).fx_tip("Off: filled slices stay in escrow until you claim them yourself. The bazaar limits how many orders you can have, filled-but-unclaimed ones included.");
                let sr=rr(18.0,426.0,327.0,470.0);
                ui.put(sr,egui::Label::new(RichText::new(msg).color(col).size(15.0)));
                let go=rr(18.0,476.0,327.0,516.0);
                let can=parsed.is_ok();
                let go_resp=ui.interact(go,ui.id().with("ice_go"),egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                let base=if can{GREEN}else{GREEN.gamma_multiply(0.35)};
                let gr=if can{fx_bg(ui,&go_resp,go,base,Color32::from_rgb(90,255,90),base,Color32::WHITE,3.0)}else{pt.rect_filled(go,3.0,base);go};
                pt.text(gr.center(),egui::Align2::CENTER_CENTER,format!("ICEBERG {}",pretty_s(&sel).to_uppercase()),bold(22.0),Color32::BLACK);
                if can && go_resp.clicked(){self.ice_submit();}
                if let Some((ok,m))=self.ice_form.msg.clone(){ pt.text(at(20.0,532.0),egui::Align2::LEFT_CENTER,m,font(16.0),if ok{GREEN}else{RED}); }
                if !self.agent_up { pt.text(at(20.0,554.0),egui::Align2::LEFT_CENTER,"agent offline: starts placing once it connects",font(15.0),ORANGE); }
                return;
            }
            let base_at=at;
            let at=|x:f32,y:f32|base_at(x,y+56.0);
            let rr=|x0:f32,y0:f32,x1:f32,y1:f32|egui::Rect::from_min_max(at(x0,y0),at(x1+w,y1));
            // quantity
            let mq=self.max_qty();
            if self.exchange==0 && self.buy {self.qty=self.qty.clamp(1,mq);} else if self.exchange==0 {let inv=self.inventory.get(&sel).map(|v|v.0).unwrap_or(0);self.qty=if self.source_sacks{inv+self.sack_row(&pretty(&sel)).map(|r|r.stored).unwrap_or(0)}else{inv};}
            if self.exchange==2 && self.trade_mode==0 { let av=self.sell_avail(); if av>0 && self.qty>av { self.qty=av; } }
            if self.exchange==2 && self.trade_mode>0 {
                pt.text(at(320.0+w,168.0),egui::Align2::RIGHT_CENTER,if self.trade_mode==2{"ALL".into()}else{self.inventory.get(&sel).map(|v|(v.0+v.1).to_string()).unwrap_or("…".into())},bold(21.0),ORANGE);
            } else if self.buy || self.exchange==2 {
                let top=if self.exchange==2{self.sell_avail().max(1)}else{mq};
                ui.put(rr(180.0,155.0,327.0,181.0),egui::DragValue::new(&mut self.qty).range(1..=top));
            } else {pt.text(at(320.0+w,168.0),egui::Align2::RIGHT_CENTER,self.qty.to_string(),font(21.0),DIM);}

            let ql=pt.text(at(20.0,168.0),egui::Align2::LEFT_CENTER,"Qty",font(19.5),Color32::WHITE);
            if ui.interact(ql.expand(4.0),ui.id().with("qtylabel"),egui::Sense::hover()).fx_tip(format!("Selected: {} (scroll here to fine-tune)",self.qty)).hovered() {
                let d=ui.input(|i|i.raw_scroll_delta.y); if d!=0.0 && (self.buy || self.exchange==2){let top=if self.exchange==2{self.sell_avail().max(1)}else{mq};self.qty=((self.qty as i64)+d.signum() as i64).clamp(1,top as i64) as u64;}
            }
            let btn=|ui:&mut egui::Ui,r:egui::Rect,text:&str,id:&str|->bool{
                let resp=ui.interact(r,ui.id().with(id),egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                let rr=fx_bg(ui,&resp,r,Color32::from_gray(36),Color32::from_gray(62),Color32::from_gray(20),ORANGE.gamma_multiply(0.85),3.0);
                ui.painter().text(rr.center(),egui::Align2::CENTER_CENTER,text,FontId::proportional(19.5),Color32::WHITE);
                resp.clicked()
            };
            let bw=(309.0+w-30.0)/7.0;
            for (i,(text,n)) in [("1",1),("16",16),("64",64),("1k",1000),("5k",5000),("10k",10000),("20k",20000)].into_iter().enumerate(){
                let x=18.0+i as f32*(bw+5.0);
                let r=egui::Rect::from_min_max(at(x,184.0),at(x+bw,212.0));
                if btn(ui,r,text,&format!("q{n}")) && (self.exchange==2 || self.buy && n<=mq){self.qty=if self.exchange==2{n.min(self.sell_avail().max(1))}else{n};self.trade_mode=0;}
            }
            if btn(ui,egui::Rect::from_min_max(at(18.0,219.0),at(160.0+w/2.0,249.0)),"Max (inv)","fillinv"){
                if self.exchange==2{self.trade_mode=0;self.qty=self.inventory.get(&sel).map(|v|v.0).unwrap_or(0).max(1);}else if self.buy{self.qty=self.buy_max(self.unit_price(&p)).min(self.inventory.get(&sel).map(|v|v.1).unwrap_or(2304)).max(1);}
            }
            if btn(ui,egui::Rect::from_min_max(at(167.0+w/2.0,219.0),at(327.0+w,249.0)),"MAX","maxq"){
                if self.exchange==2{self.trade_mode=0;self.qty=self.sell_avail().max(1);}else if self.buy{self.qty=self.buy_max(self.unit_price(&p));}
            }
            if self.exchange==2 && self.trade_mode>0 {pt.text(at(60.0,168.0),egui::Align2::LEFT_CENTER,if self.trade_mode==1{"fill inventory"}else{"all sacks"},font(15.0),ORANGE);}
            // limit price (not in the mockup: only shown for limit orders, pushes the rest down)
            let mut dy=0.0;
            if self.exchange==0 && self.kind==Kind::Limit {
                let key=(sel.clone(),self.buy);
                if self.limit_for.as_ref()!=Some(&key){ self.limit_price=if self.buy{p.bid()+0.1}else{p.ask()-0.1}; self.limit_for=Some(key); }
                pt.text(at(20.0,272.0),egui::Align2::LEFT_CENTER,"Limit price",font(19.5),DIM);
                ui.put(rr(180.0,259.0,327.0,286.0),egui::DragValue::new(&mut self.limit_price).speed(0.1).fixed_decimals(1));
                let (bid,ask)=(p.bid(),p.ask()); let spread=(ask-bid).max(0.0); let r1=|x:f64|(x*10.0).round()/10.0;
                let presets:Vec<(&str,f64)>=if self.buy{vec![("Top",bid),("Top +0.1",bid+0.1),("+5%",bid+spread*0.05),("+10%",bid+spread*0.10)]}else{vec![("Best",ask),("Best −0.1",ask-0.1),("−5%",ask-spread*0.05),("−10%",ask-spread*0.10)]};
                let bw=(310.0+w)/4.0;
                for (i,(t,v)) in presets.into_iter().enumerate() { if btn(ui,egui::Rect::from_min_max(at(18.0+bw*i as f32,293.0),at(18.0+bw*(i as f32+1.0)-4.0,319.0)),t,&format!("lp{i}")) {self.limit_price=r1(v).max(0.1);} }
                dy=70.0;
            }
            let ay=|y:f32|at(0.0,y+dy).y;
            let line=|y:f32|pt.line_segment([egui::pos2(at(18.0,0.0).x,ay(y)),egui::pos2(at(328.0+w,0.0).x,ay(y))],Stroke::new(1.0,Color32::from_gray(90)));
            line(259.0);
            let unit=self.unit_price(&p);
            let estimate_qty=if self.exchange==2 && self.trade_mode>0 {self.inventory.get(&sel).map(|v|v.0+v.1).unwrap_or(0)}else{self.qty};
            let value=unit*estimate_qty as f64;
            fx::tip_rect(ui,egui::Rect::from_center_size(egui::pos2(at(80.0,0.0).x,ay(276.0)),egui::vec2(120.0,24.0)),"unitprice","Price for one item: what you pay (or get) at the moment. Follows the market.");
            fx::tip_rect(ui,egui::Rect::from_center_size(egui::pos2(at(70.0,0.0).x,ay(309.0)),egui::vec2(110.0,24.0)),"esttotal","Unit price times the quantity. A guess: the real price can move before the order lands.");
            pt.text(egui::pos2(at(20.0,0.0).x,ay(276.0)),egui::Align2::LEFT_CENTER,"Unit. Price",font(20.0),Color32::WHITE);
            pt.text(egui::pos2(at(324.0+w,0.0).x,ay(276.0)),egui::Align2::RIGHT_CENTER,format!("{:.1}",fx::count_to(ctx,egui::Id::new("tk_unit"),unit)),bold(21.0),ORANGE);   // follows the price; the total below updates at once as you type
            pt.text(egui::pos2(at(20.0,0.0).x,ay(309.0)),egui::Align2::LEFT_CENTER,"Est. Total",font(20.0),Color32::WHITE);
            pt.text(egui::pos2(at(324.0+w,0.0).x,ay(309.0)),egui::Align2::RIGHT_CENTER,fmt_coins(value),bold(21.0),ORANGE);
            line(326.0);
            let checks=self.checks(unit);   // still gates the button, just no longer listed
            let ok=checks.iter().all(|c|c.0);
            let extra=-150.0;
            let mut go=egui::Rect::from_min_max(egui::pos2(at(18.0,0.0).x,ay(511.0+extra)),egui::pos2(at(330.0+w,0.0).x,ay(560.0+extra)));
            if go.bottom()>full.bottom()-6.0 {go=go.translate(egui::vec2(0.0,full.bottom()-6.0-go.bottom()));}
            let col=if self.buy{GREEN}else{RED};
            let go_hit=go;
            let go_resp=ui.interact(go_hit,ui.id().with("go"),egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
            let base=if ok{col}else{col.gamma_multiply(0.35)};
            let go=if ok{fx_bg(ui,&go_resp,go_hit,base,if self.buy{Color32::from_rgb(90,255,90)}else{Color32::from_rgb(255,90,90)},base,Color32::WHITE,3.0)}else{pt.rect_filled(go_hit,3.0,base);go_hit};
            let quantity=if self.exchange==2 && self.trade_mode>0 {if self.trade_mode==1{"INV".into()}else{"ALL".into()}}else{self.qty.to_string()};
            let verb=if self.kind==Kind::Instant&&self.exchange==0{if self.buy{"BIN"}else{"SIN"}}else if self.buy{"BUY"}else{"SELL"};
            let label=format!("{} {} {}",verb,quantity,pretty_s(&sel).to_uppercase());
            let (main,tail)=label.split_once("  ").unwrap_or((&label,""));
            let mut label_size=23.5;
            while label_size>14.0 && pt.layout_no_wrap(main.to_string(),bold(label_size),Color32::BLACK).size().x+70.0>go.width()-20.0 {label_size-=1.0;}
            let g1=pt.layout_no_wrap(main.to_string(),bold(label_size),Color32::BLACK);
            let g2=pt.layout_no_wrap(tail.to_string(),font(21.0),Color32::BLACK);
            let total=g1.size().x+14.0+g2.size().x;
            let x=go.center().x-total/2.0;
            pt.galley(egui::pos2(x,go.center().y-g1.size().y/2.0),g1.clone(),Color32::BLACK);
            pt.galley(egui::pos2(x+g1.size().x+14.0,go.center().y-g2.size().y/2.0),g2,Color32::BLACK);
            if ok && go_resp.clicked(){self.submit();}
            if self.exchange==0 {
                // "Auto requote": keeps the order at the top of the book until the whole quantity is bought / sold
                let ar=egui::Rect::from_min_max(egui::pos2(go.left(),go.bottom()+8.0),egui::pos2(go.right(),go.bottom()+8.0+34.0));
                let ar_ok=ok&&self.qty>0&&self.agent_up;
                let ar_resp=ui.interact(ar,ui.id().with("autorq"),egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                let ar_base=Color32::from_gray(if ar_ok{36}else{26});
                let ar_r=if ar_ok{fx_bg(ui,&ar_resp,ar,ar_base,Color32::from_gray(62),Color32::from_gray(20),ORANGE.gamma_multiply(0.85),3.0)}else{pt.rect_filled(ar,3.0,ar_base);ar};
                pt.text(ar_r.center(),egui::Align2::CENTER_CENTER,"AUTO REQUOTE",bold(19.0),if ar_ok{ORANGE}else{DIM});
                fx::tip_rect(ui,ar,"autorq_tip",if self.buy{"Places the buy order at the top of the book and keeps it there, re-quoting whenever someone bids higher, until the whole quantity is bought (never above the current ask)"}else{"Lists everything for sale at the top of the book and re-lists whenever someone undercuts, until it is all sold (never below the current top bid)"});
                if ar_ok && ar_resp.clicked(){let (b,q)=(self.buy,self.qty);self.ticket_auto_requote(&sel,b,q);}
            }
            if value>CONFIRM_ABOVE { pt.text(egui::pos2(at(20.0,0.0).x,go.bottom()+if self.exchange==0{58.0}else{14.0}),egui::Align2::LEFT_CENTER,format!("orders over {} ask for confirmation",fmt_coins(CONFIRM_ABOVE)),font(16.0),DIM); }
            });
        });
    }

    fn bottom(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("bottom_reference_matched").show_separator_line(false).frame(terminal_frame().stroke(Stroke::new(1.0,Color32::from_gray(140))).inner_margin(egui::Margin{left:3,right:5,top:0,bottom:4})).resizable(true).default_height(162.0).height_range(110.0..=640.0).show(ctx, |ui| {
            // Fill the panel: egui saves a panel's height from its content, so short content would shrink it back after a drag.
            ui.set_min_height(ui.available_height());
            {
                let (row,_)=ui.allocate_exact_size(egui::vec2(ui.available_width(),29.0),egui::Sense::hover());
                let row=row.translate(egui::vec2(0.0,-2.0));
                let x0=row.left()-6.0;
                for (i,(a,b,text)) in [(9.0,128.0,"Blotter"),(130.0,252.0,"Positions"),(254.0,364.0,"Escrow"),(367.0,462.0,"Logs"),(464.0,559.0,"Chat")].into_iter().enumerate() {
                    let r=egui::Rect::from_min_max(egui::pos2(x0+a,row.top()+1.0),egui::pos2(x0+b,row.top()+28.0));
                    let sel=self.tab==i;
                    if sel { gradient(ui,r,Color32::from_rgb(255,194,16),Color32::from_rgb(255,155,0)); }
                    else { gradient(ui,r,Color32::from_gray(36),Color32::from_gray(10)); ui.painter().rect_stroke(r,2.0,Stroke::new(1.0,Color32::from_gray(70)),egui::StrokeKind::Inside); }
                    ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,text,if sel{FontId::new(21.0,egui::FontFamily::Name("terminal-bold".into()))}else{FontId::proportional(18.0)},if sel{Color32::BLACK}else{Color32::WHITE});
                    if ui.interact(r,ui.id().with(("btab",i)),egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).clicked(){self.tab=i;}
                }
                {   // a thin underline that slides to the selected tab
                    let tabs=[(9.0f32,128.0f32),(130.0,252.0),(254.0,364.0),(367.0,462.0),(464.0,559.0)];
                    let (a,b)=tabs[self.tab.min(4)];
                    let (ux0,ux1)=fx::slide_indicator(ctx,ui.id().with("btab_ind"),x0+a,x0+b);
                    ui.painter().rect_filled(egui::Rect::from_min_max(egui::pos2(ux0,row.top()+27.0),egui::pos2(ux1,row.top()+29.5)),0.0,Color32::WHITE);
                }
                ui.painter().text(egui::pos2(row.right()-44.0,row.top()+15.0),egui::Align2::RIGHT_CENTER,"BLT",FontId::proportional(18.5),Color32::WHITE);
                { let xr=egui::Rect::from_center_size(egui::pos2(row.right()-14.0,row.top()+15.0),egui::vec2(24.0,24.0));
                  let hv=ui.interact(xr,ui.id().with("bclose"),egui::Sense::click()).fx_tip("Close panel (reopen from Layout or the F-keys)");
                  ui.painter().text(xr.center(),egui::Align2::CENTER_CENTER,"×",FontId::proportional(26.0),if hv.hovered(){RED}else{Color32::WHITE});
                  if hv.clicked(){self.show_bottom=false;} }
                ui.painter().line_segment([egui::pos2(row.left()-3.0,row.bottom()),egui::pos2(row.right()+5.0,row.bottom())],Stroke::new(1.0,Color32::from_gray(80)));
            }
            let (content_op,content_off)=fx::content_in(ctx,ui.id().with("bcontent"),self.tab as u64);
            ui.scope(|ui|{ ui.set_opacity(content_op); ui.add_space(content_off);
            match self.tab {
                0 => {
                    egui::Frame::NONE.inner_margin(egui::Margin{left:14,right:0,top:-5,bottom:0}).show(ui,|ui| {
                        ui.spacing_mut().item_spacing.x=0.0;
                        let blotter_sel=self.blotter_sel; let mut blotter_pick=None;
                        TableBuilder::new(ui).striped(true).sense(egui::Sense::click())
                            .column(Column::exact(123.0)).column(Column::exact(85.0)).column(Column::exact(106.0))
                            .column(Column::exact(93.0)).column(Column::exact(100.0)).column(Column::exact(96.0))
                            .column(Column::exact(115.0)).column(Column::remainder())
                            .header(30.0,|mut header|{
                                for text in ["Time","Side","Symbol","Qty","Price","Type","Status","Verified"]{header.col(|ui|{
                                    let r=ui.max_rect();
                                    ui.painter().text(r.left_center()+egui::vec2(0.0,1.0),egui::Align2::LEFT_CENTER,text,FontId::proportional(20.5).clone(),Color32::WHITE);
                                    ui.painter().line_segment([egui::pos2(r.left()-11.0,r.bottom()+1.0),egui::pos2(r.right()+60.0,r.bottom()+1.0)],Stroke::new(1.0,Color32::from_gray(55)));
                                });}
                            }).body(|body|{
                                body.rows(26.0,self.trades.len(),|mut row|{
                                    let t=&self.trades[row.index()];
                                    row.col(|ui|{ui.label(&t.time);});
                                    row.col(|ui|{ui.label(RichText::new(t.side).color(if t.side=="BUY"{GREEN}else{RED}));});
                                    row.col(|ui|{icon_cell(ui,&self.icons,&t.id,&pretty_s(&t.id));});
                                    row.col(|ui|{ui.label(t.qty.to_string());});
                                    row.col(|ui|{ui.label(format!("{:.1}",t.price));});
                                    row.col(|ui|{ui.label(if t.kind==Kind::Npc{"NPC"}else if t.kind==Kind::Instant{"Instant"}else{"Limit"});});
                                    row.col(|ui|{match &t.status {
                                        Status::Working=>{ui.label(RichText::new("Sending…").color(ORANGE));},
                                        Status::Resting=>{ui.label(RichText::new("Resting (open)").color(Color32::from_rgb(90,170,255)));},
                                        Status::Filled=>{ui.label(RichText::new("Filled").color(GREEN));},
                                        Status::Rejected(m)=>{ui.add(egui::Label::new(RichText::new(format!("Rejected: {m}")).color(RED)).truncate()).fx_tip(m);},
                                    }});
                                    let idx=row.index();
                                    row.col(|ui|{
                                        if t.status==Status::Filled {let(mark,color)=if t.sig.verified(){("✔ verified",GREEN)}else{("◐ unverified",ORANGE)};ui.label(RichText::new(mark).color(color)).fx_tip(t.sig.describe());}
                                        else{ui.label(RichText::new("—").color(DIM));}
                                    });
                                    let resp=row.response();
                                    if resp.clicked() { blotter_pick=Some(idx); }
                                    let painter=resp.ctx.layer_painter(resp.layer_id);
                                    fx::row_bg_p(&resp.ctx,&painter,egui::Id::new(("blotter_row",idx)),resp.rect,resp.hovered(),blotter_sel==Some(idx));
                                });
                            });
                        if blotter_pick.is_some() { self.blotter_sel=blotter_pick; }
                    });
                }
                1 => {
                    ui.horizontal(|ui| {
                        if ui.fx_button("Refresh holdings").clicked() {
                            for id in self.positions.iter().filter(|(_, v)| v.0 != 0).map(|(k, _)| k.clone()).collect::<Vec<_>>() {
                                let _ = self.cmd_tx.send(json!({"cmd":"holdings","product":id}));
                            }
                        }
                        ui.label(RichText::new("reads your real inventory + sacks in-game (first lookup of an item scans the sacks)").color(DIM).small());
                    });
                    let mut gr=fx::GridRows::new(ui.ctx(),egui::Id::new("tbl_pos"),1);
                    let colors=gr.colors(ui.ctx(),self.positions.len()+1);
                    let grid_out=egui::Grid::new("pos").striped(true).num_columns(7).spacing([22.0, 3.0]).with_row_color({let c=colors; move |i,_|c.get(i).copied().flatten()}).show(ui, |ui| {
 gr.row_start(ui);
                        for t in ["Symbol", "Qty", "Avg cost", "Mark (bid)", "Unrealized P&L", "In inventory", "In sacks"] { ui.label(RichText::new(t).color(Color32::WHITE).strong()); }
                        ui.end_row(); gr.row_start(ui);
                        let mut rows: Vec<_> = self.positions.iter().filter(|(_, v)| v.0 != 0).collect();
                        rows.sort_by_key(|(k, _)| (*k).clone());
                        for (id, (q, avg)) in rows {
                            let mark = self.products.get(id).map(|p| p.bid()).unwrap_or(0.0);
                            let pnl = (mark - avg) * *q as f64;
                            icon_cell(ui,&self.icons,id,&pretty_s(id)); ui.label(q.to_string()); ui.label(format!("{avg:.1}")); ui.label(format!("{mark:.1}"));
                            ui.label(RichText::new(fmt_coins(pnl)).color(if pnl >= 0.0 { GREEN } else { RED }).strong());
                            match self.holdings.get(id) {
                                Some((i, sk)) => { ui.label(i.to_string()); ui.label(sk.map(|x| x.to_string()).unwrap_or("—".into())); }
                                None => { ui.label("—"); ui.label("—"); }
                            }
                            ui.end_row(); gr.row_start(ui);
                        }
                    });
                    gr.finish(ui,grid_out.response.rect,3.0);
                }
                2 => { egui::ScrollArea::vertical().id_salt("escrow_scroll").auto_shrink([false,false]).show(ui,|ui|{
                    ui.horizontal(|ui| {
                        if ui.fx_button_en(!self.busy(&["orders"]),"Refresh escrow").clicked() { let _ = self.cmd_tx.send(json!({"cmd":"orders"})); }
                        if ui.fx_button_en(!self.busy(&["claim"]),"Claim coins").clicked() { let _ = self.cmd_tx.send(json!({"cmd":"claim"})); }
                        let claimable: Vec<OrderRow> = self.orders.iter().filter(|o| o.claimable && o.to_claim > 0).cloned().collect();
                        if ui.add_enabled(!claimable.is_empty() && self.agent_up && !self.busy(&["claimorder"]), egui::Button::new(format!("Claim all ({})", claimable.len())))
                            .fx_tip("Claims every filled order in turn: the goods from buy orders and the coins from sell offers").clicked() {
                            for o in &claimable {
                                let id = o.product.to_uppercase().replace(' ', "_");
                                let _ = self.cmd_tx.send(json!({"cmd":"claimorder","product":id,"side":o.side,"price":o.price,"sack_on_full":self.auto_sack}));
                            }
                            self.say(ORANGE, format!("→ claim all: {} order(s) with something in escrow", claimable.len()));
                        }
                        let open_orders=self.orders.iter().filter(|o|!(o.total>0&&o.filled>=o.total)).count();
                        if ui.add_enabled(open_orders>0 && self.agent_up && !self.busy(&["cancel"]), egui::Button::new(format!("Cancel all ({open_orders})")))
                            .fx_tip("Cancels every open order (anything already filled stays in escrow to claim)").clicked() {
                            let all: Vec<OrderRow> = self.orders.iter().filter(|o|!(o.total>0&&o.filled>=o.total)).cloned().collect();   // a completely filled order has nothing left to cancel
                            let _ = self.cmd_tx.send(json!({"cmd":"cancel","orders":all.iter().map(|o|json!({"product":o.product,"side":o.side,"price":o.price})).collect::<Vec<_>>()}));
                            self.say(ORANGE, format!("→ cancel all: {} order(s)", all.len()));
                            self.orders.retain(|o|o.total>0&&o.filled>=o.total);
                        }
                        ui.label(RichText::new("opens the in-game bazaar menus briefly").color(DIM).small());
                    });
                    ui.horizontal(|ui| {
                        ui.label("Flip sell price:");
                        let mut auto = self.flip_price <= 0.0;
                        if ui.fx_checkbox(&mut auto, "auto (best offer − 0.1)").changed() { self.flip_price = if auto { 0.0 } else { 1.0 }; }
                        if !auto { ui.add(egui::DragValue::new(&mut self.flip_price).speed(0.1).fixed_decimals(1).range(0.1..=1.0e9)); }
                    });
                    ui.label(RichText::new("Claim all repeats until the escrow is empty (one click only pulls what fits: free slots × 64; the compactor and sacks then free space).").color(DIM).small());
                    let (claim_busy,flip_busy,cancel_busy)=(self.busy(&["claimorder"]),self.busy(&["fliporder"]),self.busy(&["cancel"]));
                    let mut cancel: Option<OrderRow> = None;
                    let mut claim: Option<OrderRow> = None;
                    let mut flip: Option<OrderRow> = None;
                    let mut gr=fx::GridRows::new(ui.ctx(),egui::Id::new("tbl_orders"),1);
                    let colors=gr.colors(ui.ctx(),self.orders.len()+1);
                    let grid_out=egui::Grid::new("orders").striped(true).num_columns(9).spacing([20.0, 3.0]).with_row_color({let c=colors; move |i,_|c.get(i).copied().flatten()}).show(ui, |ui| {
 gr.row_start(ui);
                        for t in ["Side", "Product", "Amount", "Filled", "In escrow", "Price", "", "", ""] { ui.label(RichText::new(t).color(Color32::WHITE).strong()); }
                        ui.end_row(); gr.row_start(ui);
                        for o in &self.orders {
                            ui.label(RichText::new(&o.side).color(if o.side == "BUY" { GREEN } else { RED }).strong());
                            icon_cell(ui,&self.icons,&o.product.to_uppercase().replace(' ',"_"),&short(&o.product));
                            ui.label(o.amount.map(|a| fmt_coins(a as f64)).unwrap_or("—".into()));
                            ui.label(format!("{}/{}", fmt_coins(o.filled as f64), fmt_coins(o.total as f64)));
                            ui.label(RichText::new(if o.to_claim > 0 { fmt_coins(o.to_claim as f64) } else { "—".into() }).color(if o.to_claim > 0 { ORANGE } else { DIM }));
                            ui.label(o.price.map(|p| format!("{p:.1}")).unwrap_or("—".into()));
                            if o.claimable { if ui.fx_small_button_en(!claim_busy,"Claim all").clicked() { claim = Some(o.clone()); } } else { ui.label(""); }
                            if o.claimable && o.side == "BUY" { if ui.fx_small_button_en(!flip_busy,"Flip").fx_tip("sell straight from escrow — no inventory needed").clicked() { flip = Some(o.clone()); } } else { ui.label(""); }
                            if ui.fx_small_button_en(!cancel_busy,"Cancel").clicked() { cancel = Some(o.clone()); }
                            ui.end_row(); gr.row_start(ui);
                        }
                    });
                    gr.finish(ui,grid_out.response.rect,3.0);
                    if self.orders.is_empty() && self.orders_at.is_none() && self.agent_up && fx::loading_for(ui.ctx(),egui::Id::new("escrow_loading"),true,0.15) {
                        for k in 0..4 { let (r,_)=ui.allocate_exact_size(egui::vec2(ui.available_width().min(900.0),22.0),egui::Sense::hover()); fx::skeleton(ui,egui::Rect::from_min_size(r.min,egui::vec2(r.width()*(0.55+0.1*(k%3) as f32),18.0)),3.0); ui.add_space(3.0); }
                    }
                    if let Some(o) = flip {
                        let id = o.product.to_uppercase().replace(' ', "_");
                        let auto = self.products.get(&id).map(|p| ((p.ask() - 0.1) * 10.0).round() / 10.0).unwrap_or(0.0);
                        let sell = if self.flip_price > 0.0 { self.flip_price } else { auto };
                        if sell > 0.0 {
                            let _ = self.cmd_tx.send(json!({"cmd":"fliporder","product":id,"side":o.side,"price":o.price,"sell_price":sell}));
                            self.say(ORANGE, format!("→ flip {} {} at {:.1}", o.side, o.product, sell));
                        } else { self.say(RED, "flip: no price available"); }
                    }
                    if let Some(o) = claim {
                        let id = o.product.to_uppercase().replace(' ', "_");
                        let _ = self.cmd_tx.send(json!({"cmd":"claimorder","product":id,"side":o.side,"price":o.price,"sack_on_full":self.auto_sack}));
                        self.say(ORANGE, format!("→ claim all {} {} ({} in escrow)", o.side, o.product, fmt_coins(o.to_claim as f64)));
                    }
                    if let Some(o) = cancel {
                        let _ = self.cmd_tx.send(json!({"cmd":"cancel","product":o.product,"side":o.side,"price":o.price}));
                        self.orders.retain(|x| !(x.side == o.side && x.product == o.product && x.price == o.price));
                        self.say(ORANGE, format!("→ cancel {} {} @ {:?}", o.side, o.product, o.price));
                    }
                }); }
                3 => {
                    egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false,false]).show(ui, |ui| {
                        for (c, s) in &self.log { ui.label(RichText::new(s).color(*c)); }
                    });
                }
                _ => { self.chat_ui(ui); }
            }
            });
        });
    }

    /// Switch this window to another account: the link moves to that account's server and everything shown is replaced by its state.
    fn switch_account(&mut self, ctx: &egui::Context, name: &str) {
        if !self.is_client() || name == bb::profile::name() || !bb::profile::all().iter().any(|e| e.name == name) { return; }
        bb::profile::set_name(name);
        server::LINK_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.link_up.store(false, std::sync::atomic::Ordering::Relaxed);
        self.agent_up = false; self.purse = None; self.bank = None; self.halted = false; self.spent_today = 0.0;
        self.orders.clear(); self.orders_at = None; self.jobs.clear(); self.trades.clear(); self.inventory.clear(); self.inv_all.clear(); self.inv_slots.clear(); self.inv_free = None;
        self.sacks.clear(); self.sacks_at = None; self.holdings.clear(); self.positions.clear(); self.inflight.clear(); self.claiming.clear(); self.remote_queue.clear();
        self.log.clear(); self.chat.clear(); self.confirm = None; self.ice_confirm = None; self.ice_edit = None; self.bank_dlg = None; self.sell_inv_confirm = None;
        self.worth_hist.clear(); self.worth_others.clear(); self.worth_hist_at = None; self.worth_last_t = 0.0;
        if let Ok(mut g) = bb::SACK_API.lock() { *g = None; }
        if let Ok(mut l) = self.cmd_tx.log.lock() { l.clear(); }
        self.link_state = "switching account…".into();
        self.snap_last.clear();
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(if bb::profile::is_main() { "Blockberg".to_string() } else { format!("Blockberg ({name})") }));
        self.say(ORANGE, format!("switched to account: {name}"));
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if let Some(n) = self.pending_switch.take() { self.switch_account(ctx, &n); }
        if let Some(pos) = self.acct_menu {
            let cur = bb::profile::name();
            let mut pick: Option<String> = None; let mut close = false;
            let resp = egui::Area::new(egui::Id::new("acct_menu")).order(egui::Order::Foreground).fixed_pos(pos).pivot(egui::Align2::RIGHT_TOP).show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(170.0);
                    for e in bb::profile::all() {
                        let sel = e.name == cur;
                        if ui.selectable_label(sel, RichText::new(format!("{}{}", if sel { "● " } else { "   " }, e.name.to_uppercase())).size(18.0)).clicked() { if !sel { pick = Some(e.name.clone()); } close = true; }
                    }
                });
            });
            if self.ui_time - self.acct_menu_t > 0.3 && ctx.input(|i| i.pointer.any_click()) && !resp.response.hovered() && !close { close = true; }   // not on the very click that opened it
            if let Some(n) = pick { self.switch_account(ctx, &n); }
            if close { self.acct_menu = None; }
        }
        self.npc_calc_dialog(ctx);
        if let Some(mut d) = self.bank_dlg.clone() {
            let (mut go, mut cancel) = (false, false);
            egui::Window::new("Bank").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.set_min_width(400.0);
                ui.label(RichText::new(format!("Account: {}", if bb::profile::is_main() { "main".to_string() } else { bb::profile::name() })).color(DIM));
                ui.horizontal(|ui| { ui.selectable_value(&mut d.coop, true, "Co-op bank"); ui.selectable_value(&mut d.coop, false, "Personal bank"); });
                ui.horizontal(|ui| { ui.selectable_value(&mut d.deposit, true, "Deposit"); ui.selectable_value(&mut d.deposit, false, "Withdraw"); });
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut d.mode, 0, if d.deposit { "Whole purse" } else { "Everything" });
                    ui.selectable_value(&mut d.mode, 1, "Half");
                    ui.selectable_value(&mut d.mode, 2, "Amount");
                    ui.selectable_value(&mut d.mode, 3, if d.deposit { "Keep in purse" } else { "Top purse up to" });
                });
                if d.mode >= 2 { ui.add(egui::TextEdit::singleline(&mut d.text).hint_text("e.g. 5m, 250k").desired_width(160.0)); }
                let cmd = d.command();
                match &cmd { Ok(_) => { ui.label(RichText::new(d.summary()).color(ORANGE)); } Err(e) => { ui.label(RichText::new(e.clone()).color(DIM)); } }
                ui.label(RichText::new("Uses /bank in game (needs the Booster Cookie buff). Opens the bank menu briefly.").color(DIM).size(14.0));
                if !self.agent_up { ui.label(RichText::new("agent offline").color(RED)); }
                ui.horizontal(|ui| {
                    if ui.add_enabled(cmd.is_ok() && self.agent_up && !self.halted && !self.busy(&["bank"]), egui::Button::new(if d.deposit { "  Deposit  " } else { "  Withdraw  " })).clicked() { go = true; }
                    if ui.fx_button("  Close  ").clicked() { cancel = true; }
                });
            });
            if go { if let Ok(c) = d.command() { self.say(ORANGE, format!("→ bank: {}", d.summary())); let _ = self.cmd_tx.send(c); } self.bank_dlg = None; }
            else if cancel { self.bank_dlg = None; } else { self.bank_dlg = Some(d); }
        }
        if let Some(rows) = self.sell_inv_confirm.clone() {
            let mut go = false; let mut cancel = false;
            egui::Window::new("Sell inventory to the bazaar?").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.set_min_width(420.0);
                ui.label(RichText::new("Each bazaar item in your inventory becomes a sell offer at the lowest current offer minus 0.1 (a sell offer lists the whole stack of that item).").color(Color32::WHITE));
                ui.label(RichText::new("Sacks are not touched. The agent reads the inventory fresh when it starts, so this list is a preview from the last read.").color(DIM).size(15.0));
                ui.add_space(4.0);
                if rows.is_empty() { ui.label(RichText::new("The last inventory read shows nothing the bazaar trades.").color(ORANGE)); }
                let mut total = 0.0;
                for (id, n, p) in rows.iter().take(10) { ui.label(format!("{}  ×{}  @ {:.1}", pretty_s(id), grouped(*n), p)); }
                if rows.len() > 10 { ui.label(RichText::new(format!("…and {} more item type(s)", rows.len() - 10)).color(DIM)); }
                for (_, n, p) in &rows { total += *n as f64 * *p; }
                if !rows.is_empty() { ui.label(RichText::new(format!("About {} coins if they all sell", fmt_coins(total))).color(ORANGE)); }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.fx_button("  List them  ").clicked() { go = true; }
                    if ui.fx_button("  Cancel  ").clicked() { cancel = true; }
                });
            });
            if go { self.sell_inv_confirm = None; self.send_sell_inventory(); }
            if cancel { self.sell_inv_confirm = None; }
        }
        if self.show_workflows {
            let mut open = true;
            egui::Window::new("Workflows").open(&mut open).default_width(700.0).resizable(true).show(ctx, |ui| { self.workflows_ui(ui); });
            self.show_workflows = open;
        }
        if let Some(o) = self.confirm.clone() {
            egui::Window::new("Confirm large order").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                let side = if matches!(o.cmd, "instabuy" | "buyorder") { "BUY" } else { "SELL" };
                ui.label(RichText::new(format!("{side} {} x{}", pretty_s(&o.id), o.qty)).size(18.0).strong().color(if side == "BUY" { GREEN } else { RED }));
                ui.label(format!("@ {:.1}   total ~{} coins", o.unit, fmt_coins(o.unit * o.qty as f64)));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.fx_button("  Send order  ").clicked() { self.confirm = None; self.dispatch(o.clone()); }
                    if ui.fx_button("  Cancel  ").clicked() { self.confirm = None; }
                });
            });
        }
        self.ice_edit_dialog(ctx);
        if let Some(j) = self.ice_confirm.clone() {
            egui::Window::new("Start iceberg order?").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.label(RichText::new(format!("BUY {} {}", fmt_coins(j.total as f64), pretty_s(&j.product))).size(18.0).strong().color(GREEN));
                ui.label(format!("{} per order at top bid +{:.1}, max price {:.1}, requote every {}", grouped(j.slice), j.offset, j.max_price, iceberg::fmt_dur(j.requote_secs as f64)));
                ui.label(format!("Worst case this spends ~{} coins over time.", fmt_coins(j.total as f64 * j.max_price)));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.fx_button("  Start  ").clicked() { self.ice_confirm = None; self.ice_add(j.clone()); self.ice_form.msg = Some((true, "started".into())); }
                    if ui.fx_button("  Cancel  ").clicked() { self.ice_confirm = None; }
                });
            });
        }
        if self.show_about {
            egui::Window::new("About Blockberg").open(&mut self.show_about).show(ctx, |ui| {
                ui.label("Blockberg — SkyBlock bazaar terminal.");
                ui.label("Prices: Hypixel public bazaar API. Orders: Minescript agent in Minecraft.");
                ui.label("Bid = what you get when you insta-sell. Ask = what you pay when you insta-buy.");
                ui.label("F12 halts all trading.");
                ui.separator();
                ui.label("Press ` (backtick) to jump to the command line. Commands:");
                ui.label("  <SYMBOL>            select an item (e.g. COAL, ENCHANTED SEEDS)");
                ui.label("  <SYMBOL> BUY 160    pre-fill the ticket   (SELL works too)");
                ui.label("  WATCH DEPTH TICKET  show / hide panels (also F2 F3 F4)");
                ui.label("  BLT ESCROW PORT LOGS CHAT   open a bottom tab (F5 F6 F10 F8)");
                ui.label("  WF                  workflows (F7)");
                ui.label("  QUEUE / ICE         bot queue and iceberg orders      INV   inventory      MAIN   back");
                ui.label("  HALT / RESUME       trading kill switch");
                ui.label("  LAYOUT RESET        restore the default layout");
            });
        }
    }
}


// ---------- command line, layout, chat, workflows ----------
impl Gui {
    /// Show (or, for the toggle panels, flip) a panel by name. Used by F-keys, the top tabs and the command line.
    fn open_panel(&mut self, what: &str) {
        match what {
            "watch" => self.show_watch = !self.show_watch,
            "depth" => self.show_depth = !self.show_depth,
            "ticket" => self.show_ticket = !self.show_ticket,
            "workflows" => self.show_workflows = !self.show_workflows,
            "queue" => self.open_screen(Screen::Queue),
            "inventory" => self.open_screen(Screen::Inventory),
            "blotter" => { self.tab = 0; self.show_bottom = true; }
            "positions" => { self.tab = 1; self.show_bottom = true; }
            "escrow" => { self.tab = 2; self.show_bottom = true; }
            "logs" => { self.tab = 3; self.show_bottom = true; }
            "chat" => { self.tab = 4; self.show_bottom = true; }
            _ => {}
        }
    }

    fn reset_layout(&mut self) {
        self.show_watch = true; self.show_depth = true; self.show_ticket = true; self.show_bottom = true;
        self.show_workflows = false; self.tab = 0;
    }

    /// The command line: functions first (HELP, CHAT, HALT ...), otherwise a symbol, optionally followed by BUY/SELL <qty>.
    fn run_command(&mut self) {
        let raw = self.search.trim().to_string();
        self.search.clear();
        if raw.is_empty() { return; }
        // "5 + 5 =" (or any arithmetic) answers in the top bar
        if let Some(v) = calc_eval(&raw) {
            let expr = raw.trim().trim_end_matches('=').trim().to_string();
            self.calc = Some((expr, calc_fmt(v)));
            return;
        }
        let up = raw.to_uppercase().replace("<GO>", "");
        let toks: Vec<&str> = up.split_whitespace().collect();
        let first = toks.first().copied().unwrap_or("");
        match first {
            "HELP" | "H" => { self.show_about = true; return; }
            "ACCT" | "ACCOUNT" | "SWITCH" => {   // ACCT alt1  (name as listed in agents.json); ACCT alone opens the menu
                let want = raw.split_whitespace().nth(1).map(|n| n.to_lowercase());
                match want { Some(n) if bb::profile::all().iter().any(|e| e.name == n) => self.pending_switch = Some(n), Some(n) => self.say(RED, format!("no account called {n}")), None => { self.acct_menu = Some(egui::pos2(ctx_right_hint(), 60.0)); self.acct_menu_t = self.ui_time; } }
                return;
            }
            "HALT" => { self.set_halted(true); return; }
            "RESUME" => { self.set_halted(false); return; }
            "WATCH" | "MON" | "MONITOR" => { self.open_panel("watch"); return; }
            "DEPTH" | "BOOK" => { self.open_panel("depth"); return; }
            "TICKET" | "TRAD" => { self.open_panel("ticket"); return; }
            "BLT" | "BLOTTER" => { self.open_panel("blotter"); return; }
            "BANK" | "DEPOSIT" | "WITHDRAW" => { self.bank_dlg = Some(BankDlg { coop: true, deposit: first != "WITHDRAW", mode: 0, text: String::new() }); return; }
            "PORT" | "PRT" | "PORTFOLIO" | "WORTH" | "NW" => { if self.screen != Screen::Portfolio { self.open_screen(Screen::Portfolio); } return; }
            "POS" => { self.open_panel("positions"); return; }
            "ESCROW" | "ORD" | "OMON" => { self.open_panel("escrow"); return; }
            "LOGS" | "LOG" => { self.open_panel("logs"); return; }
            "CHAT" | "MSG" => { self.open_panel("chat"); return; }
            "AUTOFLIP" | "FLIP" | "AUTO" => { if self.screen != Screen::AutoFlip { self.open_screen(Screen::AutoFlip); } return; }
            "SCREENER" | "SCREEN" | "BROWSE" | "CATALOG" => { if self.screen != Screen::Screener { self.open_screen(Screen::Screener); } return; }
            "WF" | "FLOW" | "WORKFLOWS" => { self.open_panel("workflows"); return; }
            "QUEUE" | "Q" | "ICE" | "ICEBERG" => { self.screen = Screen::Queue; if self.ice_form.product.is_empty() { self.ice_form.product = self.selected.clone().unwrap_or_default(); } return; }
            "INV" | "INVENTORY" => { if self.screen != Screen::Inventory { self.open_screen(Screen::Inventory); } return; }
            "MAIN" | "BACK" => { self.screen = Screen::Main; return; }
            "LAYOUT" => { if toks.get(1) == Some(&"RESET") { self.reset_layout(); self.say(DIM, "layout reset"); } return; }
            "CLAIM" => { let _ = self.cmd_tx.send(json!({"cmd":"claim"})); return; }
            _ => {}
        }
        // "<SYMBOL words> [BUY|SELL [qty]]"
        let act = toks.iter().position(|t| *t == "BUY" || *t == "SELL");
        let sym: Vec<&str> = match act { Some(i) => toks[..i].to_vec(), None => toks.clone() };
        self.search = sym.join(" ");
        self.add_from_search();
        if let Some(i) = act {
            self.buy = toks[i] == "BUY";
            if let Some(q) = toks.get(i + 1).and_then(|t| t.replace(',', "").parse::<u64>().ok()) { self.qty = q.clamp(1, self.max_qty()); }
            self.show_ticket = true;
        }
    }

    fn layout_path() -> Option<std::path::PathBuf> {
        Some(crate::bb::profile::data_dir().join("layout.json"))
    }

    fn load_layout(&mut self) {
        if cfg!(test) { return; }   // tests start from the defaults and never read your real layout
        let Some(v) = Self::layout_path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { return };
        self.show_watch = v["watch"].as_bool().unwrap_or(true);
        self.show_depth = v["depth"].as_bool().unwrap_or(true);
        self.show_ticket = v["ticket"].as_bool().unwrap_or(true);
        self.show_bottom = v["bottom"].as_bool().unwrap_or(true);
        self.tab = v["tab"].as_u64().unwrap_or(0).min(4) as usize;
        self.auto_claim = v["auto_claim"].as_bool().unwrap_or(true);
        self.watch_mode = v["watch_mode"].as_u64().unwrap_or(0).min(2) as u8;
        self.ice_auto_remove = v["ice_auto_remove"].as_bool().unwrap_or(true);
        if let Some(b) = v["reduced_motion"].as_bool() { self.reduced_motion = b; }
        self.screener.hidden = v["screener_hidden"].as_u64().unwrap_or(0) as u32;
        if !self.is_client() {
            let old = v["auto_pickup"].as_bool().unwrap_or(false);   // the single switch of the earlier version: it meant both
            self.auto_stash = v["auto_stash"].as_bool().unwrap_or(old);
            self.auto_lf = v["auto_lost_found"].as_bool().unwrap_or(old);
            self.auto_coins = v["auto_coins"].as_bool().unwrap_or(true);
            self.auto_sack = v["auto_sack"].as_bool().unwrap_or(false);
            self.auto_sleep = v["auto_sleep"].as_bool().unwrap_or(false);
            self.quick_book = v["quick_book"].as_bool().unwrap_or(false);
            self.agent_sleeping = v["agent_sleeping"].as_bool().unwrap_or(false);
        }
        if let Some(m)=std::env::var("BLOCKBERG_WATCH_MODE").ok().and_then(|s|s.parse::<u8>().ok()) { self.watch_mode=m.min(2); }
    }

    fn save_layout(&self) {
        if cfg!(test) { return; }   // ...and never write it
        if let Some(p) = Self::layout_path() {
            if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
            let v = json!({"watch":self.show_watch,"depth":self.show_depth,"ticket":self.show_ticket,"bottom":self.show_bottom,"tab":self.tab,"auto_claim":self.auto_claim,"watch_mode":self.watch_mode,"ice_auto_remove":self.ice_auto_remove,"reduced_motion":self.reduced_motion,"auto_stash":self.auto_stash,"auto_lost_found":self.auto_lf,"auto_sack":self.auto_sack,"auto_sleep":self.auto_sleep,"quick_book":self.quick_book,"agent_sleeping":self.agent_sleeping,"screener_hidden":self.screener.hidden,"auto_coins":self.auto_coins});
            let _ = std::fs::write(p, v.to_string());
        }
    }

    fn push_chat(&mut self, raw: &str) {
        let text = raw.replace("\\n", "  ↵  ");
        self.chat.push(ChatLine { ts: clock().get(..5).unwrap_or("").to_string(), segs: parse_mc(&text) });
        if self.chat.len() > 600 { self.chat.remove(0); }
    }

    fn send_chat(&mut self) {
        let t = self.chat_input.trim().to_string();
        if t.is_empty() { return; }
        if !self.agent_up { self.say(RED, "chat: agent offline"); return; }
        if t.starts_with('\\') { self.say(RED, "chat: backslash commands are blocked"); return; }
        let _ = self.cmd_tx.send(json!({"cmd":"say","text":t}));
        self.chat_input.clear();
    }

    fn chat_ui(&mut self, ui: &mut egui::Ui) {
        let avail = ui.available_height();
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).max_height((avail - 40.0).max(40.0)).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            if self.chat.is_empty() {
                ui.label(RichText::new(if self.agent_up { "waiting for chat…  (needs the updated in-game agent)" } else { "agent offline: chat appears when it connects" }).color(DIM));
            }
            for l in &self.chat {
                let mut job = egui::text::LayoutJob::default();
                job.wrap.max_width = ui.available_width();
                job.append(&format!("{}  ", l.ts), 0.0, egui::TextFormat { font_id: FontId::proportional(17.0), color: DIM, ..Default::default() });
                for (c, t) in &l.segs { job.append(t, 0.0, egui::TextFormat { font_id: FontId::proportional(18.5), color: *c, ..Default::default() }); }
                ui.label(job);
            }
        });
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 96.0).max(120.0);
            let r = ui.add(egui::TextEdit::singleline(&mut self.chat_input).desired_width(w).hint_text("Type a message…   /commands work   ·   Enter sends"));
            fx::focus_ring(ui, &r, None);
            let send = ui.fx_button("Send").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            if send { self.send_chat(); r.request_focus(); }
        });
    }

    fn workflows_ui(&mut self, ui: &mut egui::Ui) {
                    ui.label(RichText::new("Compact & sell:  buy → compactor merges 160:1 into your sack → take from sack → sell at /trades").color(ORANGE).strong());
                    ui.horizontal(|ui| {
                        ui.label("Buy");
                        { let fr=ui.add(egui::TextEdit::singleline(&mut self.wf_base).desired_width(150.0)); fx::focus_ring(ui,&fr,if self.wf_base.trim().is_empty(){None}else{Some(self.products.contains_key(&self.wf_base.trim().to_uppercase()))}); }
                        ui.label("→");
                        { let fr=ui.add(egui::TextEdit::singleline(&mut self.wf_ench).desired_width(190.0)); fx::focus_ring(ui,&fr,if self.wf_ench.trim().is_empty(){None}else{Some(self.products.contains_key(&self.wf_ench.trim().to_uppercase()))}); }
                        ui.label("Units");
                        ui.add(egui::DragValue::new(&mut self.wf_units).range(160..=2304).speed(160.0));
                        let ask = self.products.get(&self.wf_base).map(|p| p.ask()).unwrap_or(0.0);
                        ui.label(RichText::new(format!("≈ {} coins", fmt_coins(ask * (self.wf_units - self.wf_units % 160) as f64))).color(DIM));
                        if ui.add_enabled(!self.wf_running && self.agent_up && !self.halted, egui::Button::new(RichText::new("Run workflow").strong().color(Color32::BLACK)).fill(GREEN)).clicked() { self.run_workflow(); }
                    });
                    for (name, st, msg) in &self.wf_steps {
                        let (mark, col) = match st { None => ("…", DIM), Some(true) => ("✔", GREEN), Some(false) => ("✘", RED) };
                        ui.label(RichText::new(format!("{mark}  {name}   {msg}")).color(col));
                    }
                    if self.wf_running { ui.label(RichText::new("running — the in-game menus open and close on their own").color(ORANGE)); }
                    if let Some(r) = &self.wf_result { ui.label(RichText::new(r).strong().color(if r.starts_with("completed") { GREEN } else { RED })); }
                
    }
}

struct ChatLine { ts: String, segs: Vec<(Color32, String)> }

/// Minecraft colour codes (§a, §c ...) to coloured text segments.
fn parse_mc(s: &str) -> Vec<(Color32, String)> {
    let base = Color32::from_gray(215);
    let (mut out, mut col, mut buf) = (Vec::new(), base, String::new());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '§' {
            let Some(code) = it.next() else { break };
            if !buf.is_empty() { out.push((col, std::mem::take(&mut buf))); }
            col = match code.to_ascii_lowercase() {
                '0' => Color32::from_gray(120), '1' => Color32::from_rgb(70, 90, 255), '2' => Color32::from_rgb(0, 170, 0), '3' => Color32::from_rgb(0, 170, 170),
                '4' => Color32::from_rgb(190, 0, 0), '5' => Color32::from_rgb(170, 0, 170), '6' => Color32::from_rgb(255, 170, 0), '7' => Color32::from_gray(190),
                '8' => Color32::from_gray(130), '9' => Color32::from_rgb(90, 90, 255), 'a' => Color32::from_rgb(85, 255, 85), 'b' => Color32::from_rgb(85, 255, 255),
                'c' => Color32::from_rgb(255, 85, 85), 'd' => Color32::from_rgb(255, 85, 255), 'e' => Color32::from_rgb(255, 255, 85), 'f' => Color32::WHITE,
                'r' => base, _ => col,
            };
        } else { buf.push(c); }
    }
    if !buf.is_empty() { out.push((col, buf)); }
    out
}

/// Item icons: one PNG atlas of 16x16 tiles built from Minecraft's own textures (see assets/icons.json).
struct Icons { tex: egui::TextureHandle, cols: usize, tile: f32, aw: f32, ah: f32, index: HashMap<String, usize> }

fn load_icons(ctx: &egui::Context) -> Option<Icons> {
    let img = image::load_from_memory(include_bytes!("../../assets/icons.png")).ok()?.to_rgba8();
    let (w, h) = (img.width() as usize, img.height() as usize);
    let color = egui::ColorImage::from_rgba_unmultiplied([w, h], img.as_raw());
    let tex = ctx.load_texture("item_icons", color, egui::TextureOptions::NEAREST);
    let meta: Value = serde_json::from_str(include_str!("../../assets/icons.json")).ok()?;
    let index = meta["index"].as_object()?.iter().filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n as usize))).collect();
    Some(Icons { tex, cols: meta["cols"].as_u64()? as usize, tile: meta["size"].as_f64()? as f32, aw: w as f32, ah: h as f32, index })
}

impl Icons {
    fn has(&self, id: &str) -> bool { self.index.contains_key(id) }
    fn paint(&self, p: &egui::Painter, id: &str, rect: egui::Rect) {
        let i = self.index.get(id).copied().unwrap_or(0);
        let (cx, cy) = ((i % self.cols) as f32, (i / self.cols) as f32);
        let uv = egui::Rect::from_min_max(egui::pos2(cx * self.tile / self.aw, cy * self.tile / self.ah), egui::pos2((cx + 1.0) * self.tile / self.aw, (cy + 1.0) * self.tile / self.ah));
        p.image(self.tex.id(), rect, uv, Color32::WHITE);
    }
}

/// An item's icon followed by its name, for table cells.
fn icon_cell(ui: &mut egui::Ui, icons: &Option<Icons>, id: &str, text: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        if let Some(ic) = icons {
            let (r, _) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
            ic.paint(ui.painter(), id, r);
        }
        ui.label(text);
    });
}

/// Candle size in minutes (negative sizes are seconds: -15 = 15 s).
fn tf_minutes(tf: i64) -> f64 { if tf < 0 { -tf as f64 / 60.0 } else { tf as f64 } }

fn tf_name(minutes: i64) -> String {
    match minutes { m if m < 0 => format!("{}s", -m), m if m >= 1440 => format!("{}D", m / 1440), m if m >= 60 => format!("{}h", m / 60), m => format!("{m}m") }
}

/// Little candlestick glyph for the chart-type button.
/// What an NPC pays you for one (Hypixel items API, assets/npc_sell.json; refresh with tools/npc_prices.py).
fn npc_sell_of(id: &str) -> Option<f64> {
    static T: std::sync::OnceLock<HashMap<String,f64>> = std::sync::OnceLock::new();
    T.get_or_init(|| serde_json::from_str(include_str!("../../assets/npc_sell.json")).unwrap_or_default()).get(id).copied()
}
/// What an NPC shop charges you for one (NotEnoughUpdates repo, assets/npc_buy.json); None if no shop sells it.
fn npc_buy_of(id: &str) -> Option<f64> {
    static T: std::sync::OnceLock<HashMap<String,f64>> = std::sync::OnceLock::new();
    T.get_or_init(|| serde_json::from_str(include_str!("../../assets/npc_buy.json")).unwrap_or_default()).get(id).copied()
}
fn npc_fmt(v: Option<f64>) -> String { v.map(|x| if x>=1000.0 { fmt_coins(x) } else if x.fract()==0.0 { format!("{x:.0}") } else { format!("{x:.1}") }).unwrap_or_else(|| "—".into()) }

fn candle_icon(pt: &egui::Painter, c: egui::Pos2) {
    for (dx, top, bot, up) in [(-8.0f32, -5.0f32, 6.0f32, false), (0.0, -9.0, 3.0, true), (8.0, -3.0, 8.0, true)] {
        let col = if up { GREEN } else { RED };
        pt.line_segment([c + egui::vec2(dx, top - 3.0), c + egui::vec2(dx, bot + 3.0)], Stroke::new(1.5, col));
        pt.rect_filled(egui::Rect::from_min_max(c + egui::vec2(dx - 3.0, top), c + egui::vec2(dx + 3.0, bot)), 0.0, col);
    }
}

fn grouped(n: u64) -> String {
    let raw=n.to_string(); let len=raw.len();
    raw.chars().enumerate().flat_map(|(i,c)|if i>0 && (len-i)%3==0 {vec![',',c]} else {vec![c]}).collect()
}

fn depth(ui: &mut egui::Ui, col: egui::Rect, title: &str, levels: &[Level], c: Color32, loading: bool) {
    // mockup pixels: y measured from the panel's heading bottom (= 34), x from the column's left/right edge
    let pt=ui.painter().with_clip_rect(col);
    let (l,r,t)=(col.left(),col.right(),col.top()-34.0);
    let font=|sz:f32|FontId::proportional(sz);
    let head=egui::Rect::from_min_max(egui::pos2(l,t+36.0),egui::pos2(r,t+58.0));
    let (top,bottom)=if c==RED{(Color32::from_rgb(120,14,22),Color32::from_rgb(70,6,12))}else{(Color32::from_rgb(18,100,30),Color32::from_rgb(8,58,16))};
    gradient(ui,head,top,bottom);
    pt.text(egui::pos2(l+13.0,head.center().y),egui::Align2::LEFT_CENTER,title,font(19.5),Color32::WHITE);
    let lab=egui::Rect::from_min_max(egui::pos2(l,t+60.0),egui::pos2(r,t+83.0));
    pt.rect_filled(lab,0.0,Color32::from_gray(10));
    pt.text(egui::pos2(l+13.0,lab.center().y),egui::Align2::LEFT_CENTER,"Price",font(19.5),Color32::WHITE);
    pt.text(egui::pos2(r-24.0,lab.center().y),egui::Align2::RIGHT_CENTER,"Qty",font(19.5),Color32::WHITE);
    let (cell,bar)=if c==RED{(Color32::from_rgb(85,12,18),Color32::from_rgb(140,22,32))}else{(Color32::from_rgb(10,70,20),Color32::from_rgb(20,125,38))};
    if loading && fx::loading_for(ui.ctx(),egui::Id::new(("depth_loading",title)),true,0.15) {
        for i in 0..7 {
            let cy=t+95.0+i as f32*19.8;
            if cy+8.0>col.bottom(){break;}
            fx::skeleton(ui,egui::Rect::from_min_max(egui::pos2(l+10.0,cy-8.0),egui::pos2(l+10.0+(col.width()-30.0)*(0.5+0.08*(i%4) as f32),cy+8.0)),3.0);
        }
    }
    let max_len=(col.width()*0.48).max(40.0);
    let lmax=levels.iter().take(8).map(|v|v.amount).max().unwrap_or(1).max(1001) as f64;
    let step=if c==RED{20.2}else{19.4};
    // ---- animations (the depth-lab guide): every visible level is keyed by its price so it keeps its identity while the book moves
    let now=ui.input(|i|i.time);
    let anim=!fx::reduced();
    let akey=egui::Id::new(("depth_anim",title));
    let mut st:DepthAnim=ui.ctx().data_mut(|m|m.get_temp::<DepthAnim>(akey)).unwrap_or_default();
    let shown:Vec<&Level>=levels.iter().take(8).collect();
    let sig:Vec<(i64,u64,u64)>=shown.iter().map(|v|((v.price*10.0).round() as i64,v.amount,v.orders)).collect();
    if anim && st.sig!=sig && !shown.is_empty() {
        let first=st.sig.is_empty();
        if !first {
            st.updated=now;   // the whole book flashes once per update
            // levels that left the visible book slide out to the right and fade
            for (k,a,o) in st.sig.iter() { if !sig.iter().any(|x|x.0==*k) { if let Some(r)=st.rows.get(k) { st.gone.push(DepthGone{price:*k,amount:*a,orders:*o,y:r.y_to,len:r.len_to,t:now}); } } }
            if st.sig.first().map(|x|x.0)!=sig.first().map(|x|x.0) { st.best=now; }   // a new best price glows
            // the best level shrinking at the same price is a fill: pulse it
            if let (Some(a),Some(b))=(st.sig.first(),sig.first()) { if a.0==b.0 && b.1<a.1 { st.trade=now; } }
        }
        for (i,(k,a,_)) in sig.iter().enumerate() {
            let y=i as f32*step;
            let len=(max_len*((*a as f64/lmax).sqrt()) as f32).max(0.0);
            match st.rows.get_mut(k) {
                Some(r)=>{
                    let (cy,cl)=(r.y(now),r.len(now));
                    if r.y_to!=y { r.y_from=cy; r.y_to=y; r.t_move=now; }
                    if (r.len_to-len).abs()>0.1 { r.len_from=cl; r.len_to=len; r.t_len=now; }
                    if r.amount!=*a { r.flash=if *a>r.amount{1}else{-1}; r.t_flash=now; r.amount=*a; }
                }
                None=>{ st.rows.insert(*k,DepthRow{y_from:y,y_to:y,t_move:-9.0,len_from:if first{len}else{0.0},len_to:len,t_len:if first{-9.0}else{now},amount:*a,flash:0,t_flash:-9.0,born:if first{-9.0}else{now}}); }
            }
        }
        st.rows.retain(|k,_|sig.iter().any(|x|x.0==*k));
        st.sig=sig.clone();
    }
    if !anim { st=DepthAnim{sig:sig.clone(),..Default::default()}; }
    st.gone.retain(|g|now-g.t<DEPTH_ANIM);
    let busy=anim && (now-st.updated<DEPTH_ANIM*1.5 || !st.gone.is_empty() || st.rows.values().any(|r|now-r.t_move<DEPTH_ANIM||now-r.t_len<DEPTH_ANIM||now-r.t_flash<DEPTH_ANIM||now-r.born<DEPTH_ANIM));
    // update flash: the column heading blinks amber
    if anim && now-st.updated<DEPTH_ANIM*1.5 {
        let k=1.0-((now-st.updated)/(DEPTH_ANIM*1.5)) as f32;
        let head=egui::Rect::from_min_max(egui::pos2(l,t+36.0),egui::pos2(r,t+58.0));
        pt.rect_filled(head,0.0,Color32::from_rgba_unmultiplied(255,185,0,(110.0*k) as u8));
        pt.circle_filled(egui::pos2(r-12.0,head.center().y),4.0,Color32::from_rgba_unmultiplied(255,210,80,(255.0*k) as u8));
    }
    for g in st.gone.iter() {
        let k=fx::ease_out(((now-g.t)/DEPTH_ANIM) as f32);
        let cy=t+95.0+g.y; let dx=18.0*k; let a=((1.0-k)*255.0) as u8;
        if cy+8.0>col.bottom(){continue;}
        let w=Color32::from_rgba_unmultiplied(255,255,255,a);
        pt.rect_filled(egui::Rect::from_min_max(egui::pos2(l+61.0+dx,cy-8.5),egui::pos2(l+61.0+dx+g.len,cy+8.5)),0.0,bar.gamma_multiply(1.0-k));
        pt.text(egui::pos2(l+20.0+dx,cy),egui::Align2::LEFT_CENTER,format!("{:.1}",g.price as f64/10.0),font(18.0),w);
        pt.text(egui::pos2(r-47.0+dx,cy),egui::Align2::RIGHT_CENTER,grouped(g.amount),font(18.0),w);
        pt.text(egui::pos2(r-9.0+dx,cy),egui::Align2::RIGHT_CENTER,format!("({})",g.orders),font(18.0),w);
    }
    for (i,v) in shown.iter().enumerate() {
        let key=(v.price*10.0).round() as i64;
        let ra=st.rows.get(&key).copied();
        let (yoff,len,born_k)=match (anim,ra) { (true,Some(r))=>(r.y(now),r.len(now),((now-r.born)/DEPTH_ANIM).clamp(0.0,1.0) as f32), _=>(i as f32*step,(max_len*((v.amount as f64/lmax).sqrt()) as f32).max(0.0),1.0) };
        let enter=fx::ease_out(born_k);
        let cy=t+95.0+yoff-12.0*(1.0-enter);
        if cy+8.0>col.bottom(){continue;}
        let fade=|c:Color32|c.gamma_multiply(enter);
        let row=egui::Rect::from_min_max(egui::pos2(l+10.0,cy-9.0),egui::pos2(r-2.0,cy+9.0));
        let rid=egui::Id::new(("depthrow",title,i));
        let rr_=ui.interact(row.intersect(col),rid,egui::Sense::click());
        let sel_key=egui::Id::new(("depthsel",title));
        if rr_.clicked() { ui.ctx().data_mut(|m|m.insert_temp(sel_key,i)); }
        let selected=ui.ctx().data_mut(|m|m.get_temp::<usize>(sel_key))==Some(i);
        fx::row_bg(ui,rid,row,rr_.hovered(),selected);
        // best-price glow and fill pulse (amber), on the top row
        if anim && i==0 {
            let g=now-st.best; if g<DEPTH_ANIM { pt.rect_filled(row,0.0,Color32::from_rgba_unmultiplied(255,185,0,(68.0*(1.0-g/DEPTH_ANIM)) as u8)); }
            let tr=now-st.trade; if tr<DEPTH_ANIM { let k=(tr/DEPTH_ANIM) as f32; pt.rect_filled(row,0.0,Color32::from_rgba_unmultiplied(255,185,0,(85.0*(1.0-k)) as u8)); pt.rect_stroke(row.expand(8.0*(1.0-k)),2.0,Stroke::new(2.0,Color32::from_rgba_unmultiplied(255,185,0,(80.0*(1.0-k)) as u8)),egui::StrokeKind::Outside); }
        }
        pt.rect_filled(egui::Rect::from_min_max(egui::pos2(l+10.0,cy-8.5),egui::pos2(l+61.0,cy+8.5)),0.0,fade(cell));
        if len>0.5 { gradient(ui,egui::Rect::from_min_max(egui::pos2(l+61.0,cy-8.5),egui::pos2(l+61.0+len,cy+8.5)),fade(bar),fade(bar.gamma_multiply(0.75))); }
        // size change: green flash when it grew, red when it shrank
        if let (true,Some(ra))=(anim,ra) { let f=now-ra.t_flash; if f<DEPTH_ANIM && ra.flash!=0 { let a=(107.0*(1.0-f/DEPTH_ANIM)) as u8; let fc=if ra.flash>0{Color32::from_rgba_unmultiplied(49,222,142,a)}else{Color32::from_rgba_unmultiplied(255,77,101,a)}; pt.rect_filled(row.shrink2(egui::vec2(0.0,2.0)),0.0,fc); } }
        let w=fade(Color32::WHITE);
        pt.text(egui::pos2(l+20.0,cy),egui::Align2::LEFT_CENTER,format!("{:.1}",v.price),font(18.0),w);
        pt.text(egui::pos2(r-47.0,cy),egui::Align2::RIGHT_CENTER,grouped(fx::count_to(ui.ctx(),egui::Id::new(("dq",title,key)),v.amount as f64).round() as u64),font(18.0),w);
        pt.text(egui::pos2(r-9.0,cy),egui::Align2::RIGHT_CENTER,format!("({})",v.orders),font(18.0),w);
    }
    ui.ctx().data_mut(|m|m.insert_temp(akey,st));
    if busy { ui.ctx().request_repaint(); }
}

const DEPTH_ANIM: f64 = 0.65;
#[derive(Clone, Copy, Default)]
struct DepthRow { y_from: f32, y_to: f32, t_move: f64, len_from: f32, len_to: f32, t_len: f64, amount: u64, flash: i8, t_flash: f64, born: f64 }
impl DepthRow {
    fn y(&self, now: f64) -> f32 { let k = fx::ease_out(((now - self.t_move) / DEPTH_ANIM) as f32); self.y_from + (self.y_to - self.y_from) * k }
    fn len(&self, now: f64) -> f32 { let k = fx::ease_out(((now - self.t_len) / DEPTH_ANIM) as f32); self.len_from + (self.len_to - self.len_from) * k }
}
#[derive(Clone, Copy)]
struct DepthGone { price: i64, amount: u64, orders: u64, y: f32, len: f32, t: f64 }
#[derive(Clone, Default)]
struct DepthAnim { sig: Vec<(i64, u64, u64)>, rows: HashMap<i64, DepthRow>, gone: Vec<DepthGone>, updated: f64, best: f64, trade: f64 }

/// The agent's address: the first argument that is not a flag, or BLOCKBERG_AGENT, or the default.
fn agent_addr_from_args() -> String {
    if std::env::var("BLOCKBERG_MOCK").is_ok() { return "mockup".into(); }   // screenshots / tests: never touches the real agent
    std::env::args().skip(1).find(|a| !a.starts_with("--")).or_else(|| std::env::var("BLOCKBERG_AGENT").ok()).unwrap_or_else(|| crate::bb::profile::entry().agent)
}

/// `blockberg-gui --server`: no window; the engine runs here until it is stopped.
fn run_server() -> ! {
    let Some(lock) = server::take_engine_lock() else { eprintln!("blockberg-server: another Blockberg engine is already running"); std::process::exit(3); };
    server::init_journal();
    server::journal_event(json!({"ev": "boot"}));
    let ctx = egui::Context::default();
    let unresolved = server::journal_unresolved();
    let mut g = Gui::with_context(&ctx, agent_addr_from_args(), server::Mode::Server);
    g._lock = Some(lock);
    g.say(GREEN, "server started");
    if !unresolved.is_empty() { g.say(ORANGE, format!("before this start the server was in the middle of: {} (it will check the real order menu before doing anything)", unresolved.join("; "))); }
    loop {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| g.server_tick()));
        if r.is_err() { g.say(RED, "an internal error was caught: the server carries on"); }
        std::thread::sleep(Duration::from_millis(50));
    }
}

static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
/// `BLOCKBERG_TIMING=1` prints how long each start-up step took (for finding slow starts).
fn tlog(what: &str) { if std::env::var_os("BLOCKBERG_TIMING").is_some() { eprintln!("[start {:>6.0} ms] {what}", T0.get_or_init(Instant::now).elapsed().as_secs_f64()*1000.0); } }

fn main() -> eframe::Result {
    tlog("main");
    if std::env::args().any(|a| a == "--server") { run_server(); }
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_decorations(false).with_maximized(true).with_inner_size(std::env::var("BLOCKBERG_SIZE").ok().and_then(|v|{let (w,h)=v.split_once('x')?;Some([w.parse::<f32>().ok()?,h.parse::<f32>().ok()?])}).unwrap_or([1672.0, 941.0])).with_min_inner_size([1100.0, 650.0]).with_title("Blockberg").with_app_id("blockberg-gui").with_icon(std::sync::Arc::new(eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon.png")).expect("icon"))),
        ..Default::default()
    };
    eframe::run_native(&if bb::profile::is_main(){"Blockberg".to_string()}else{format!("Blockberg ({})",bb::profile::name())}, opts, Box::new(|cc| Ok(Box::new(Gui::new(cc)))))
}

fn format_ago(m: f64) -> String {
    if m.abs()>=2880.0 {format!("{:.1}d",m/1440.0)} else if m.abs()>=120.0 {format!("{:.1}h",m/60.0)} else {format!("{m:.0}m")}
}

#[cfg(test)]
mod gui_tests {
    use super::*;
    fn gui() -> (Gui,mpsc::UnboundedReceiver<Value>) {
        let mut g=Gui::with_context(&egui::Context::default(),"mockup".into(),server::Mode::Standalone);
        g.products=mock_products();g.selected=Some("COAL".into());g.agent_up=true;g.halted=false;
        let (tx,rx)=mpsc::unbounded_channel();g.cmd_tx=CmdTx::new(tx);(g,rx)
    }
    #[test]
    fn npc_modes_send_agent_protocol_and_use_actual_fill() {
        for (mode,name) in [(0,"qty"),(1,"fill"),(2,"all")] {
            let (mut g,mut rx)=gui();g.exchange=2;g.trade_mode=mode;g.qty=64;g.inventory.insert("COAL".into(),(64,0));
            g.submit();let cmd=rx.try_recv().unwrap();
            assert_eq!(cmd["cmd"],"tradesellqty");assert_eq!(cmd["product"],"COAL");assert_eq!(cmd["mode"],name);
            assert_eq!(cmd["qty"],if mode==0{json!(64)}else{Value::Null});
            g.on_agent(json!({"type":"fill","trades":true,"sold":32,"gained":64.0,"signals":{"chat":true,"inventory":true}}));
            assert_eq!(g.trades[0].qty,32);assert_eq!(g.trades[0].price,2.0);assert!(g.trades[0].sig.verified());
            assert_eq!(g.positions["COAL"].0,-32);
        }
    }
    #[test]
    fn clear_blotter_keeps_trades_that_are_still_referenced() {
        let (mut g,_rx)=gui();
        for i in 0..4 { g.trades.push(Trade{time:clock(),side:"BUY",id:format!("X{i}"),qty:1,price:1.0,kind:Kind::Limit,status:Status::Resting,sig:Sig::default()}); }
        g.inflight.push_back(3);
        g.clear_blotter();
        assert_eq!(g.trades.len(),1);assert_eq!(g.trades[0].id,"X3");assert_eq!(g.inflight[0],0);
    }
    #[test]
    fn an_error_from_a_poll_does_not_reject_the_order_being_placed() {
        let (mut g,_rx)=gui();
        g.trades.push(Trade{time:clock(),side:"BUY",id:"COAL".into(),qty:1,price:1.0,kind:Kind::Limit,status:Status::Working,sig:Sig::default()});
        g.inflight.push_back(0);
        g.on_agent(json!({"type":"error","msg":"no BUY order for Coal","cmd":"claimorder"}));
        assert_eq!(g.inflight.len(),1);assert!(g.trades[0].status==Status::Working);
        g.on_agent(json!({"type":"error","msg":"rejected","cmd":"buyorder"}));
        assert!(g.inflight.is_empty());assert!(matches!(g.trades[0].status,Status::Rejected(_)));
    }
    #[test]
    fn search_finds_items_without_exact_typing() {
        let (mut g,_rx)=gui();
        let base=g.products["COAL"].clone();
        for id in ["ENCHANTED_SEEDS","SEEDS","ENCHANTED_COAL_BLOCK","ENCHANTED_COAL"] { g.products.insert(id.into(),base.clone()); }
        assert_eq!(g.search_items("ench seed")[0],"ENCHANTED_SEEDS");
        assert_eq!(g.search_items("enchanted_seeds")[0],"ENCHANTED_SEEDS");
        assert_eq!(g.search_items("seeds")[0],"SEEDS");
        assert_eq!(g.search_items("coal blk")[0],"ENCHANTED_COAL_BLOCK");
        assert!(g.search_items("ecs").contains(&"ENCHANTED_SEEDS".to_string()),"skipped letters still find it");
        assert!(g.search_items("zzzz").is_empty());
    }
    #[test]
    fn ticket_iceberg_starts_a_job_and_validates() {
        let (mut g,_rx)=gui();
        g.iceberg=true;g.ice_form.product="COAL".into();
        g.ice_form.total="".into();g.ice_form.max="2".into();g.ice_submit();
        assert!(g.jobs.is_empty());assert!(matches!(&g.ice_form.msg,Some((false,_))),"missing total is refused");
        g.ice_form.total="1000".into();g.ice_form.max="".into();g.ice_submit();assert!(g.jobs.is_empty(),"missing max price is refused");
        g.ice_form.max="2".into();g.ice_submit();
        assert_eq!(g.jobs.len(),1);assert_eq!(g.jobs[0].total,1000);assert_eq!(g.jobs[0].product,"COAL");
        g.ice_form.total="2m".into();g.ice_form.max="1.5".into();g.ice_submit();
        assert!(g.ice_confirm.is_some(),"a big worst-case cost asks first");assert_eq!(g.jobs.len(),1);
        g.ice_form.max="3".into();g.ice_form.min="5".into();g.ice_confirm=None;g.ice_submit();assert!(g.ice_confirm.is_none(),"min above max is refused");
    }
    #[test]
    fn an_unanswered_placement_pauses_the_iceberg_instead_of_placing_twice() {
        let (mut g,mut rx)=gui();
        g.purse=Some(1e9);
        g.ice_form.product="COAL".into();g.ice_form.total="1000".into();g.ice_form.max="50".into();g.ice_submit();
        g.ice_tick();
        let cmd=rx.try_recv().expect("first order sent");assert_eq!(cmd["cmd"],"buyorder");
        g.inflight_since=Some(Instant::now()-Duration::from_secs(310));
        g.ice_tick();
        assert_eq!(g.jobs[0].state,iceberg::JobState::Paused);
        for _ in 0..3 { g.ice_tick(); }
        assert!(rx.try_recv().is_err(),"nothing else may be placed while it is unknown what happened to the first order");
    }
    #[test]
    fn a_dropped_agent_connection_sends_a_placing_iceberg_to_find_its_order() {
        let (mut g,mut rx)=gui();
        g.purse=Some(1e9);
        g.ice_form.product="COAL".into();g.ice_form.total="1000".into();g.ice_form.max="50".into();g.ice_submit();
        g.ice_tick();
        assert_eq!(rx.try_recv().expect("first order sent")["cmd"],"buyorder");
        assert!(!g.inflight.is_empty());
        g.ice_agent_lost();
        assert!(g.inflight.is_empty(),"the answer will never come");
        assert_eq!(g.jobs[0].state,iceberg::JobState::Adopting,"it looks for its order instead of waiting five minutes and pausing");
    }
    #[test]
    fn the_flip_score_rewards_profit_per_hour_and_punishes_thin_markets() {
        let (mut g,_rx)=gui();
        g.purse=Some(50_000_000.0);
        let lv=|p:f64|vec![Level{price:p,amount:100_000,orders:1}];
        // a liquid item: bid 500, ask 650, 1.68M traded each way per week (10,000 an hour)
        g.products.insert("LIQUID".into(),Product{sell_summary:lv(500.0),buy_summary:lv(650.0),quick_status:Quick{sell_price:500.0,buy_price:650.0,sell_moving_week:1_680_000,buy_moving_week:1_680_000}});
        // the same prices but almost nobody trades it (2 an hour)
        g.products.insert("THIN".into(),Product{sell_summary:lv(500.0),buy_summary:lv(650.0),quick_status:Quick{sell_price:500.0,buy_price:650.0,sell_moving_week:336,buy_moving_week:336}});
        // a tiny margin
        g.products.insert("TINY".into(),Product{sell_summary:lv(500.0),buy_summary:lv(512.0),quick_status:Quick{sell_price:500.0,buy_price:512.0,sell_moving_week:1_680_000,buy_moving_week:1_680_000}});
        let l=g.flip_est_for_tests("LIQUID").expect("a liquid item scores");
        let t=g.flip_est_for_tests("THIN").expect("a thin item still has a number");
        assert_eq!(l.route,"bazaar");
        assert!((l.net-(649.9*0.9875-500.1)).abs()<1e-6,"profit per unit after tax");
        assert!((l.units_h-6000.0).abs()<1e-6,"60% of 10,000 an hour");
        assert!(l.pph<l.net*6000.0&&l.pph>0.7*l.net*6000.0,"profit per hour is the profit per unit less what chasing the book costs, times units per hour");
        assert!(l.life>0.0&&l.erosion>0.0&&l.erosion<l.margin);
        assert!(l.score>t.score+2.0,"{} vs {}: a market that barely trades must score far lower",l.score,t.score);
        assert!(g.flip_est_for_tests("TINY").is_none(),"under a 10% margin is not a flip");
        assert!(l.score>=1.0&&l.score<=10.0);
        g.products.get_mut("LIQUID").unwrap().quick_status.sell_moving_week=0;
        assert!(g.flip_est_for_tests("LIQUID").is_none(),"nothing fills the buy order: no flip");
    }
    #[test]
    fn the_flip_button_fills_the_auto_flip_form_and_a_missing_sack_costs_a_little() {
        let (mut g,_rx)=gui();
        g.purse=Some(50_000_000.0);
        let lv=|p:f64|vec![Level{price:p,amount:100_000,orders:1}];
        g.products.insert("LIQUID".into(),Product{sell_summary:lv(500.0),buy_summary:lv(650.0),quick_status:Quick{sell_price:500.0,buy_price:650.0,sell_moving_week:1_680_000,buy_moving_week:1_680_000}});
        g.flip_prefill("LIQUID");
        assert_eq!(g.flip_form.product,"LIQUID");
        assert_eq!(g.screen,Screen::AutoFlip);
        let total:u64=g.flip_form.total.parse().expect("a size is suggested");
        assert!(total>=64&&g.flip_form.slice.parse::<u64>().unwrap()<=total);
        assert_eq!(g.flip_form.exchange,0,"no NPC buys it: the bazaar route");
        assert_eq!(g.flip_form.margin,"10");
        // an NPC route: the same item, but an NPC pays 620
        let base=g.flip_est_for_tests("LIQUID").unwrap();
        assert!(!base.no_sack);
        // with sacks scanned that do not hold it, the NPC route loses a little; the bazaar route does not
        g.sacks=vec![SackInfo{name:"Other Sack".into(),copies:1,cap_each:1000,items:vec![("Coal".into(),0)]}];
        let after=g.flip_est_for_tests("LIQUID").unwrap();
        assert_eq!(after.route,"bazaar");
        assert!((after.score-base.score).abs()<1e-9,"a bazaar flip goes from escrow and needs no sack");
    }
    #[test]
    fn the_portfolio_adds_the_whole_coop_and_counts_the_bank_once() {
        use portfolio::{WPoint,OtherWorth,merge_coop};
        let p=|t:f64,purse:f64,bank:f64,items:f64|WPoint{t,purse,bank,items,orders:0.0};
        let own=vec![p(0.0,100.0,10.0,1.0),p(300.0,110.0,10.0,1.0),p(600.0,120.0,12.0,1.0)];
        let other=OtherWorth{name:"alt1".into(),points:vec![p(100.0,50.0,10.0,5.0),p(400.0,60.0,10.0,5.0)],..Default::default()};
        let m=merge_coop(&own,&[other]);
        assert_eq!(m.first().unwrap().purse,100.0,"before the other account started recording: only this one");
        let at300=m.iter().find(|x|(x.t-300.0).abs()<1.0).unwrap();
        assert_eq!((at300.purse,at300.items,at300.bank),(160.0,6.0,10.0),"purses and items add up, the shared bank is counted once");
        let last=m.last().unwrap();
        assert_eq!((last.purse,last.bank),(180.0,12.0),"an account that stopped recording keeps its last values");
    }
    #[test]
    fn calculator_answers_arithmetic_and_ignores_everything_else() {
        assert_eq!(calc_eval("5 + 5 ="),Some(10.0));
        assert_eq!(calc_eval("5+5"),Some(10.0));
        assert_eq!(calc_eval("2m / 71680"),Some(2_000_000.0/71680.0));
        assert_eq!(calc_eval("(1,000 + 500) * 2"),Some(3000.0));
        assert_eq!(calc_eval("2^10"),Some(1024.0));
        assert_eq!(calc_eval("-3 * 4"),Some(-12.0));
        assert_eq!(calc_eval("64 x 160"),Some(10240.0));
        assert_eq!(calc_eval("1/0"),None);
        for t in ["seeds","BUY 64","ench seeds","queue","5k","","5 +","(1+2","coal-block"] { assert_eq!(calc_eval(t),None,"{t}"); }
        assert_eq!(calc_fmt(10.0),"10");assert_eq!(calc_fmt(1_500_000.0),"1,500,000 (1.50m)");assert_eq!(calc_fmt(2.5),"2.5");
        let (mut g,_rx)=gui();g.search="5 + 5 =".into();g.run_command();
        assert_eq!(g.calc,Some(("5 + 5".into(),"10".into())));assert!(g.search.is_empty());
    }
    #[test]
    fn npc_sell_needs_goods_in_the_chosen_source() {
        let (mut g,mut rx)=gui();g.exchange=2;g.qty=10;g.source_sacks=false;
        g.submit();assert!(rx.try_recv().is_err(),"nothing held: must not send");
        g.inventory.insert("COAL".into(),(5,0));g.qty=10;g.submit();assert!(rx.try_recv().is_err(),"more than held: must not send");
        g.qty=5;g.submit();assert_eq!(rx.try_recv().unwrap()["from_sacks"],false);
    }
    #[test]
    fn unsupported_exchange_and_blocked_npc_never_send() {
        for (exchange,agent_up,halted,symbol) in [(1,true,false,"COAL"),(2,false,false,"COAL"),(2,true,true,"COAL"),(2,true,false,"NO_NPC_ITEM")] {
            let (mut g,mut rx)=gui();g.exchange=exchange;g.agent_up=agent_up;g.halted=halted;
            if symbol!="COAL" {g.products.insert(symbol.into(),g.products["COAL"].clone());g.selected=Some(symbol.into());}
            g.submit();assert!(rx.try_recv().is_err());assert!(g.trades.is_empty());
        }
    }
    #[test]
    fn max_buy_respects_purse_cap_and_exchange_limit() {
        let (mut g,_)=gui();g.purse=Some(100.0);assert_eq!(g.buy_max(5.0),20);
        g.purse=Some(1_000_000_000.0);assert_eq!(g.buy_max(1.0),MAX_QTY_PER_ORDER);
    }
}

#[cfg(test)]
mod iceberg_tests {
    use super::*;
    fn gui() -> (Gui,mpsc::UnboundedReceiver<Value>) {
        let mut g=Gui::with_context(&egui::Context::default(),"mockup".into(),server::Mode::Standalone);
        g.products=mock_products();g.selected=Some("COAL".into());g.agent_up=true;g.halted=false;
        let (tx,rx)=mpsc::unbounded_channel();g.cmd_tx=CmdTx::new(tx);(g,rx)
    }
    fn job(total:u64,slice:u64,max:f64)->IceJob{let mut j=IceJob::new(0,"COAL".into(),total,slice,0.1,max,0.1,300);j.auto_claim=true;j}
    fn drain(rx:&mut mpsc::UnboundedReceiver<Value>)->Vec<Value>{let mut v=vec![];while let Ok(x)=rx.try_recv(){v.push(x);}v}

    #[test]
    fn parses_quantities() {
        assert_eq!(iceberg::parse_qty("2m"),Some(2_000_000));
        assert_eq!(iceberg::parse_qty("1,500,000"),Some(1_500_000));
        assert_eq!(iceberg::parse_qty("500K"),Some(500_000));
        assert_eq!(iceberg::parse_qty("1.5m"),Some(1_500_000));
        assert_eq!(iceberg::parse_qty("abc"),None);
        assert_eq!(iceberg::parse_qty("0"),None);
    }

    #[test]
    fn places_top_bid_plus_offset_then_next_slice_when_filled() {
        let (mut g,mut rx)=gui();
        g.ice_add(job(100_000,40_000,3.0));
        g.ice_tick();
        let c=drain(&mut rx);
        assert_eq!(c.len(),1);assert_eq!(c[0]["cmd"],"buyorder");assert_eq!(c[0]["product"],"COAL");assert_eq!(c[0]["qty"],40_000);
        assert!((c[0]["price"].as_f64().unwrap()-1.5).abs()<1e-9);   // bid 1.4 + 0.1
        // nothing else goes out while the agent is still placing it
        g.ice_tick();assert!(drain(&mut rx).is_empty());
        g.on_agent(json!({"type":"placed","msg":"ok","order":{"filled":0,"total":40_000}}));
        g.ice_tick();
        assert_eq!(g.jobs[0].state,iceberg_state_resting());
        // the order fills: chat line, then the next slice goes out at once
        g.on_agent(json!({"type":"bazaar","msg":"[Bazaar] Your Buy Order for 40000x Coal was filled!"}));
        g.ice_tick();
        assert_eq!(g.jobs[0].filled,40_000);
        g.ice_tick();
        let c=drain(&mut rx);
        assert!(c.iter().any(|x|x["cmd"]=="buyorder"&&x["qty"]==40_000),"{c:?}");
    }

    fn iceberg_state_resting()->iceberg::JobState{iceberg::JobState::Resting}

    #[test]
    fn requote_is_one_agent_action_and_keeps_the_partial_fill() {
        let (mut g,mut rx)=gui();
        g.ice_add(job(100_000,40_000,3.0));
        g.ice_tick();
        let c=drain(&mut rx);
        assert_eq!(c[0]["top_plus"],true);assert_eq!(c[0]["check"],false);assert_eq!(c[0]["max_price"],3.0);
        g.on_agent(json!({"type":"placed","msg":"ok","price":1.5}));
        g.ice_tick();
        g.jobs[0].placed_at=Instant::now()-Duration::from_secs(301);
        g.ice_tick();
        let c=drain(&mut rx);
        assert_eq!(c.len(),1,"{c:?}");
        let r=&c[0];
        assert_eq!(r["cmd"],"requote");assert_eq!(r["product"],"COAL");assert_eq!(r["slice"],40_000);assert_eq!(r["remaining"],100_000);assert_eq!(r["max_price"],3.0);
        assert_eq!(g.jobs[0].state,iceberg::JobState::Requoting);
        // agent: 5,000 had filled; the old order is cancelled and a new one rests at 1.6
        g.on_agent(json!({"type":"requoted","product":"COAL","result":"cancelled","filled":5000,"total":40000,"qty":40000,"placed":"[Bazaar] Buy Order Setup!","price":1.6}));
        assert_eq!(g.jobs[0].filled,5_000);assert_eq!(g.jobs[0].requotes,1);
        assert_eq!(g.jobs[0].state,iceberg::JobState::Resting);
        assert_eq!(g.jobs[0].cur_qty,40_000);assert!((g.jobs[0].cur_price-1.6).abs()<1e-9);
        assert_eq!(g.positions["COAL"].0,5_000);          // the partial fill is booked
        g.ice_tick();assert!(drain(&mut rx).is_empty());   // nothing else is sent: it is already resting
    }

    #[test]
    fn requote_failure_to_place_still_books_the_cancel() {
        let (mut g,mut rx)=gui();
        g.ice_add(job(100_000,40_000,3.0));
        g.ice_tick();drain(&mut rx);
        g.on_agent(json!({"type":"placed","msg":"ok","price":1.5}));g.ice_tick();
        g.jobs[0].placed_at=Instant::now()-Duration::from_secs(301);g.ice_tick();drain(&mut rx);
        g.on_agent(json!({"type":"requoted","product":"COAL","result":"cancelled","filled":0,"total":40000,"qty":40000,"place_error":"top order +0.1 is 3.2, above your max 3"}));
        assert_eq!(g.jobs[0].state,iceberg::JobState::Idle);assert_eq!(g.jobs[0].filled,0);
        assert!(g.jobs[0].note.contains("above your max"));
    }

    #[test]
    fn stop_cancels_without_placing_another() {
        let (mut g,mut rx)=gui();
        g.ice_add(job(100_000,40_000,3.0));
        g.ice_tick();drain(&mut rx);
        g.on_agent(json!({"type":"placed","msg":"ok","price":1.5}));g.ice_tick();
        g.ice_stop(0);g.ice_tick();
        let c=drain(&mut rx);assert_eq!(c[0]["cmd"],"requote");assert_eq!(c[0]["remaining"],0);
        g.on_agent(json!({"type":"requoted","product":"COAL","result":"cancelled","filled":0,"total":40000,"qty":0}));
        assert_eq!(g.jobs[0].state,iceberg::JobState::Stopped);
    }

    #[test]
    fn next_order_goes_out_before_the_claim_even_with_the_checkbox_off() {
        let (mut g,mut rx)=gui();g.auto_claim=false;
        g.ice_add(job(100_000,40_000,3.0));
        g.ice_tick();drain(&mut rx);
        g.on_agent(json!({"type":"placed","msg":"ok","price":1.5}));g.ice_tick();
        // fill: the claim is held back
        g.on_agent(json!({"type":"bazaar","msg":"[Bazaar] Your Buy Order for 40000x Coal was filled!"}));
        assert!(drain(&mut rx).iter().all(|x|x["cmd"]!="claimorder"),"claim must wait");
        // the job books the fill and places the next order straight away
        g.ice_tick();g.ice_tick();
        let c=drain(&mut rx);
        assert_eq!(c.len(),1,"{c:?}");assert_eq!(c[0]["cmd"],"buyorder");
        // only once that order is resting does the claim go out
        g.on_agent(json!({"type":"placed","msg":"ok","price":1.5}));g.ice_tick();
        let c=drain(&mut rx);
        assert!(c.iter().any(|x|x["cmd"]=="claimorder"&&x["side"]=="BUY"),"{c:?}");
        assert!(g.ice_deferred.is_empty());
    }

    #[test]
    fn a_held_back_claim_is_not_forgotten_when_no_order_follows() {
        let (mut g,mut rx)=gui();
        g.ice_add(job(40_000,40_000,3.0));                // this slice completes the job
        g.ice_tick();drain(&mut rx);
        g.on_agent(json!({"type":"placed","msg":"ok","price":1.5}));g.ice_tick();
        g.on_agent(json!({"type":"bazaar","msg":"[Bazaar] Your Buy Order for 40000x Coal was filled!"}));
        g.ice_tick();g.ice_tick();
        let c=drain(&mut rx);
        assert!(c.iter().any(|x|x["cmd"]=="claimorder"),"nothing follows, so claim now: {c:?}");
    }

    // ----- seamless restart -----
    fn reopened(old:&Gui)->(Gui,mpsc::UnboundedReceiver<Value>){
        let saved:Value=serde_json::from_str(&old.ice_json(true)).unwrap();
        let (jobs,next,claims)=Gui::ice_parse(&saved);
        let (mut g,rx)=gui();g.jobs=jobs;g.next_job_id=next;g.restore_claims=claims;(g,rx)
    }
    fn resting_job()->(Gui,mpsc::UnboundedReceiver<Value>){
        let (mut g,mut rx)=gui();
        g.ice_add(job(100_000,40_000,3.0));g.ice_tick();drain(&mut rx);
        g.on_agent(json!({"type":"placed","msg":"ok","price":1.5}));g.ice_tick();
        g.jobs[0].placed_at=Instant::now()-Duration::from_secs(100);
        (g,rx)
    }
    const OPEN_ROW:fn(u64)->Value=|f|json!({"type":"orders","orders":[{"side":"BUY","product":"Coal","amount":40000,"price":1.5,"filled":f,"total":40000,"claimable":f>0,"to_claim":f}]});

    #[test]
    fn restart_takes_over_the_resting_order_and_keeps_the_requote_clock() {
        let (g1,_)=resting_job();
        let (mut g,mut rx)=reopened(&g1);
        assert_eq!(g.jobs[0].state,iceberg::JobState::Adopting);
        g.ice_tick();
        assert_eq!(drain(&mut rx)[0]["cmd"],"orders");            // looks for its order, places nothing
        g.on_agent(OPEN_ROW(7_000));
        assert_eq!(g.jobs[0].state,iceberg::JobState::Resting);
        assert_eq!((g.jobs[0].cur_qty,g.jobs[0].cur_filled),(40_000,7_000));
        let left=g.ice_requote_in(&g.jobs[0]).unwrap();assert!(left>150.0&&left<=200.0,"{left}");
        g.ice_tick();assert!(drain(&mut rx).is_empty());          // no second order on top of it
        assert_eq!(g.trades.iter().filter(|t|t.status==Status::Resting).count(),1);
    }

    #[test]
    fn restart_books_an_order_that_filled_while_closed_then_places_the_next() {
        let (g1,_)=resting_job();
        let (mut g,mut rx)=reopened(&g1);
        g.ice_tick();drain(&mut rx);
        g.on_agent(OPEN_ROW(40_000));
        g.ice_tick();g.ice_tick();
        assert_eq!(g.jobs[0].filled,40_000);
        let c=drain(&mut rx);
        assert!(c.iter().any(|x|x["cmd"]=="buyorder"),"{c:?}");
        assert!(c.iter().all(|x|x["cmd"]!="claimorder"),"claim waits for the new order: {c:?}");
    }

    #[test]
    fn restart_with_the_order_gone_carries_on_without_double_counting() {
        let (g1,_)=resting_job();
        let (mut g,mut rx)=reopened(&g1);
        g.ice_tick();drain(&mut rx);
        g.on_agent(json!({"type":"orders","orders":[]}));
        assert_eq!(g.jobs[0].state,iceberg::JobState::Idle);assert_eq!(g.jobs[0].filled,0);
        g.ice_tick();
        assert!(drain(&mut rx).iter().any(|x|x["cmd"]=="buyorder"));
    }

    #[test]
    fn without_auto_claim_a_filled_slice_stays_in_escrow_and_the_next_one_goes_out() {
        let (mut g,mut rx)=gui();g.purse=Some(1e9);
        let mut j=job(100_000,40_000,3.0);j.auto_claim=false;g.ice_add(j);
        g.ice_tick();assert!(drain(&mut rx).iter().any(|x|x["cmd"]=="buyorder"));
        g.on_agent(json!({"type":"placed","msg":"[Bazaar] Buy Order Setup!","price":1.0}));
        g.on_agent(json!({"type":"bazaar","msg":"[Bazaar] Your Buy Order for 40000x Coal was filled!"}));
        g.ice_tick();g.ice_tick();
        let mut cmds=drain(&mut rx);
        g.on_agent(json!({"type":"placed","msg":"[Bazaar] Buy Order Setup!","price":1.0}));g.ice_tick();g.ice_tick();
        cmds.extend(drain(&mut rx));
        assert!(!cmds.iter().any(|x|x["cmd"]=="claimorder"||x["cmd"]=="claim"),"nothing may be claimed: {cmds:?}");
        assert!(cmds.iter().any(|x|x["cmd"]=="buyorder"),"the next slice is placed");
        assert!(g.ice_deferred.is_empty()&&g.claiming.is_empty());
        let (jobs,_,_)=Gui::ice_parse(&serde_json::from_str::<Value>(&g.ice_json(false)).unwrap());assert!(!jobs[0].auto_claim,"the setting survives a restart");
    }
    #[test]
    fn a_finished_iceberg_removes_itself_but_a_stopped_one_stays() {
        let (mut g,_rx)=gui();
        g.ice_add(job(100,100,3.0));g.ice_add(job(100,100,3.0));
        g.jobs[0].state=iceberg::JobState::Done;g.jobs[0].state_at=Instant::now()-Duration::from_secs(20);
        g.jobs[1].state=iceberg::JobState::Stopped;g.jobs[1].state_at=Instant::now()-Duration::from_secs(20);
        g.ice_auto_remove=false;g.ice_tick();assert_eq!(g.jobs.len(),2,"switched off: nothing is removed");
        g.ice_auto_remove=true;g.ice_tick();assert_eq!(g.jobs.len(),1);assert_eq!(g.jobs[0].state,iceberg::JobState::Stopped);
        g.ice_add(job(100,100,3.0));let n=g.jobs.len();g.jobs[n-1].state=iceberg::JobState::Done;g.jobs[n-1].state_at=Instant::now();
        g.ice_tick();assert_eq!(g.jobs.len(),n,"a job that just finished stays on screen for a few seconds");
    }
    #[test]
    fn manual_actions_show_up_in_the_queue_until_the_agent_answers() {
        let (mut g,_rx)=gui();
        let _=g.cmd_tx.send(json!({"cmd":"orders"}));
        let _=g.cmd_tx.send(json!({"cmd":"claimorder","product":"ENCHANTED_SEEDS","side":"BUY","price":1.5}));
        let _=g.cmd_tx.send(json!({"cmd":"subscribe_chat"}));   // not shown: it is plumbing
        let tags=|g:&Gui|g.queue_rows().iter().filter(|r|r.tag=="SENT").map(|r|r.text.clone()).collect::<Vec<_>>();
        let t=tags(&g);assert_eq!(t.len(),2,"{t:?}");assert!(t.iter().any(|x|x.contains("Ench Seeds")));
        g.on_agent(json!({"type":"orders","orders":[]}));
        assert_eq!(tags(&g).len(),1);assert!(g.queue_rows().iter().any(|r|r.tag=="DONE"&&r.text.contains("order menu")));
        g.on_agent(json!({"type":"error","msg":"no BUY order for Ench Seeds","cmd":"claimorder"}));
        assert!(tags(&g).is_empty());assert!(g.queue_rows().iter().any(|r|r.tag=="FAIL"&&r.detail.contains("no BUY order")));
    }
    #[test]
    fn opening_the_client_refreshes_escrow_inventory_and_sacks_once() {
        let (mut g,mut rx)=gui();g.sync_in_mock=true;
        g.startup_sync();
        let c=drain(&mut rx);let names:Vec<&str>=c.iter().filter_map(|x|x["cmd"].as_str()).collect();
        assert_eq!(names,vec!["orders","invall","sackscan"]);
        g.startup_sync();assert!(drain(&mut rx).is_empty(),"only once per run");
        let (mut g2,mut rx2)=gui();g2.sync_in_mock=true;g2.purse=Some(1e9);
        g2.ice_form.product="COAL".into();g2.ice_form.total="1000".into();g2.ice_form.max="5".into();g2.ice_submit();g2.ice_tick();let _=drain(&mut rx2);
        g2.startup_sync();
        let n:Vec<String>=drain(&mut rx2).iter().filter_map(|x|x["cmd"].as_str().map(|s|s.to_string())).collect();
        assert!(!n.contains(&"sackscan".to_string()),"a running iceberg keeps the game to itself: {n:?}");
    }
    fn pass(g:&mut Gui,ctx:&egui::Context,t:f64){let mut i=egui::RawInput::default();i.time=Some(t);g.ui_time=t;g.toasts_derive();let _=ctx.run(i,|c|g.toasts_sync(c));}
    #[test]
    fn a_toast_shows_success_only_after_the_agent_really_answered() {
        let (mut g,_rx)=gui();let ctx=egui::Context::default();
        let _=g.cmd_tx.send(json!({"cmd":"claimorder","product":"SEEDS","side":"BUY","price":1.0}));
        pass(&mut g,&ctx,1.0);
        assert_eq!(g.toasts.len(),1);assert_eq!(g.toasts[0].kind,0,"still working");assert!(g.busy(&["claimorder"]));
        g.on_agent(json!({"type":"error","msg":"no BUY order for Seeds","cmd":"claimorder"}));
        pass(&mut g,&ctx,1.5);
        assert_eq!(g.toasts[0].kind,2,"a failure is reported as one");assert!(g.toasts[0].text.contains("no BUY order"));assert!(!g.busy(&["claimorder"]));
        let _=g.cmd_tx.send(json!({"cmd":"claimorder","product":"SEEDS","side":"BUY","price":1.0}));
        pass(&mut g,&ctx,2.0);g.on_agent(json!({"type":"claimed","msg":"ok","product":"SEEDS","side":"BUY"}));pass(&mut g,&ctx,2.5);
        assert!(g.toasts.iter().any(|t|t.kind==1));
        // background requests never pop a notification
        let n=g.toasts.len();let _=g.cmd_tx.send(json!({"cmd":"orders","auto":true}));pass(&mut g,&ctx,3.0);assert_eq!(g.toasts.len(),n);
    }
    #[test]
    fn the_same_order_cannot_be_sent_twice_while_it_is_being_sent() {
        let (mut g,mut rx)=gui();g.purse=Some(1e9);g.selected=Some("COAL".into());g.qty=5;g.kind=Kind::Limit;g.limit_price=1.0;g.buy=true;
        g.limit_for=Some(("COAL".into(),true));
        g.submit();let first=drain(&mut rx);assert!(first.iter().any(|x|x["cmd"]=="buyorder"),"{first:?}");
        g.submit();let second=drain(&mut rx);assert!(second.iter().all(|x|x["cmd"]!="buyorder"),"no duplicate: {second:?}");
        assert!(g.toasts.iter().any(|t|t.kind==2&&t.text.contains("already being sent")));
    }
    fn client_with_wire()->(Gui,mpsc::UnboundedReceiver<Value>){
        let mut c=Gui::with_context(&egui::Context::default(),"mockup".into(),server::Mode::Client);
        let (tx,rx)=mpsc::unbounded_channel();
        c.link_up.store(true,std::sync::atomic::Ordering::Relaxed);
        c.op_tx=Some(tx.clone());c.cmd_tx=CmdTx::new_remote(tx,c.link_up.clone());
        (c,rx)
    }
    #[test]
    fn the_app_shows_exactly_what_the_server_knows() {
        let (mut s,_rx)=gui();s.purse=Some(1_234_567.0);s.agent_up=true;
        s.ice_add(job(1000,400,5.0));s.ice_tick();
        s.on_agent(json!({"type":"placed","msg":"ok","price":1.5}));s.ice_tick();
        s.orders=vec![OrderRow{side:"BUY".into(),product:"Coal".into(),amount:Some(400),price:Some(1.5),filled:100,total:400,claimable:true,to_claim:100}];
        s.inventory.insert("COAL".into(),(64,2000));s.inv_all=vec![InvItem{id:"COAL".into(),count:64,fits:2000}];
        let _=s.cmd_tx.send(json!({"cmd":"claimorder","product":"COAL","side":"BUY","price":1.5}));
        let snap=s.snapshot_json();
        let (mut c,_w)=client_with_wire();
        c.apply_snapshot(&snap);
        assert_eq!(c.purse,Some(1_234_567.0));assert!(c.agent_up);
        assert_eq!(c.jobs.len(),1);assert_eq!(c.jobs[0].state,iceberg::JobState::Resting);assert_eq!(c.jobs[0].cur_qty,400);assert_eq!(c.jobs[0].total,1000);
        assert_eq!(c.trades.len(),s.trades.len());assert!(c.trades[0].status==Status::Resting);
        assert_eq!(c.orders.len(),1);assert_eq!(c.orders[0].to_claim,100);
        assert_eq!(c.inventory["COAL"],(64,2000));assert_eq!(c.inv_all[0].count,64);
        assert_eq!(c.queue_rows().iter().map(|r|r.tag).collect::<Vec<_>>(),s.queue_rows().iter().map(|r|r.tag).collect::<Vec<_>>(),"the queue is the server's queue");
        assert!(c.busy(&["claimorder"]),"pending actions are known to the app, so its buttons grey out");
    }
    #[test]
    fn what_the_app_does_reaches_the_server_and_nothing_runs_in_the_app() {
        let (mut s,mut srx)=gui();s.agent_up=true;s.purse=Some(1e9);
        let (mut c,mut wire)=client_with_wire();
        // start an iceberg from the app
        c.ice_add(job(1000,400,5.0));
        let ops=drain(&mut wire);assert_eq!(ops.len(),1);assert_eq!(ops[0]["op"],"ice_add");assert!(c.jobs.is_empty(),"the app does not create jobs itself");
        s.handle_op(ops[0].clone());s.ice_tick();
        assert_eq!(s.jobs.len(),1);assert!(drain(&mut srx).iter().any(|x|x["cmd"]=="buyorder"));
        // pause it from the app
        c.apply_snapshot(&s.snapshot_json());c.ice_pause(0);
        let ops=drain(&mut wire);assert_eq!(ops[0]["do"],"pause");s.handle_op(ops[0].clone());
        assert_eq!(s.jobs[0].state,iceberg::JobState::Paused);
        // an agent command typed in the app goes through the server
        let _=c.cmd_tx.send(json!({"cmd":"orders"}));let ops=drain(&mut wire);assert_eq!(ops[0]["op"],"agent");assert_eq!(ops[0]["msg"]["cmd"],"orders");
        s.handle_op(ops[0].clone());assert!(drain(&mut srx).iter().any(|x|x["cmd"]=="orders"));
        // while the server is not reachable nothing is sent (a stale command must never run after a reconnect)
        c.link_up.store(false,std::sync::atomic::Ordering::Relaxed);
        let _=c.cmd_tx.send(json!({"cmd":"claim"}));c.set_halted(true);
        assert!(drain(&mut wire).is_empty());
    }
    #[test]
    fn a_restart_waits_for_running_work_and_lets_nothing_new_start() {
        let (mut s,mut rx)=gui();s.agent_up=true;s.purse=Some(1e9);
        s.ice_add(job(1000,400,5.0));s.ice_tick();   // placing the first order: an action is running
        assert!(drain(&mut rx).iter().any(|x|x["cmd"]=="buyorder"));
        s.restart_req=true;s.draining=true;
        assert_eq!(s.restart_waiting()>0,true,"the placement in flight has to finish first");
        s.on_agent(json!({"type":"placed","msg":"ok","price":1.5}));
        for _ in 0..3 { s.ice_tick(); }
        assert_eq!(s.restart_waiting(),0,"the running action finished: the restart may go now");
        // the filled slice would normally be followed by the next order at once: not while draining
        s.on_agent(json!({"type":"bazaar","msg":"[Bazaar] Your Buy Order for 400x Coal was filled!"}));
        for _ in 0..4 { s.ice_tick(); }
        assert!(drain(&mut rx).iter().all(|x|x["cmd"]!="buyorder"),"nothing new starts once a restart is queued");
        // and a new command from an app is turned away with a message
        s.handle_op(json!({"op":"agent","msg":{"cmd":"claim"}}));
        assert!(drain(&mut rx).is_empty());assert!(s.toasts.iter().any(|t|t.kind==2));
    }
    #[test]
    fn the_journal_says_what_was_unfinished_at_the_last_stop() {
        let j=|v:Value|v.to_string();
        let text=[j(json!({"ev":"boot"})),j(json!({"ev":"send","cmd":"buyorder","product":"SEEDS","qty":10})),j(json!({"ev":"reply","type":"placed"})),
            j(json!({"ev":"send","cmd":"claimorder","product":"SEEDS","side":"BUY"})),j(json!({"ev":"send","cmd":"requote","product":"SEEDS"}))].join("\n");
        let open=server::journal_unresolved_from(&text);
        assert_eq!(open.len(),2,"{open:?}");assert!(open[0].contains("claimorder"));assert!(open[1].contains("requote"));
        // a restart settles everything before it
        let after=format!("{text}\n{}\n{}",j(json!({"ev":"restart"})),j(json!({"ev":"boot"})));
        assert!(server::journal_unresolved_from(&after).is_empty());
    }
    #[test]
    fn a_price_alert_fires_once_when_its_level_is_reached() {
        let (mut g,_rx)=gui();   // COAL bid in the mock data is 1.4
        g.alerts.push(Alert{id:"COAL".into(),above:true,price:2.0,fired:false});
        g.alerts.push(Alert{id:"COAL".into(),above:false,price:1.0,fired:false});
        g.alerts.push(Alert{id:"COAL".into(),above:false,price:1.5,fired:false});
        g.check_alerts();
        assert_eq!(g.alerts.iter().map(|a|a.fired).collect::<Vec<_>>(),vec![false,false,true],"only the level that was really reached");
        let n=g.toasts.len();g.check_alerts();assert_eq!(g.toasts.len(),n,"it does not fire again");
        g.products.get_mut("COAL").map(|p|{p.quick_status.sell_price=2.5;p.sell_summary[0].price=2.5;});
        g.check_alerts();assert!(g.alerts[0].fired);
    }
    #[test]
    fn the_right_click_menu_actions_do_what_they_say() {
        let (mut g,_rx)=gui();let ctx=egui::Context::default();
        g.apply_menu_act(&ctx,"WHEAT",MenuAct::Info);assert!(g.info_open&&g.info_item.as_deref()==Some("WHEAT"));
        g.show_ticket=false;g.apply_menu_act(&ctx,"WHEAT",MenuAct::Trade);assert!(g.show_ticket&&g.selected.as_deref()==Some("WHEAT"));
        g.apply_menu_act(&ctx,"WHEAT",MenuAct::Alert);assert!(g.alert_dlg.as_ref().map_or(false,|d|d.id=="WHEAT"));
        let n=g.watch.len();g.apply_menu_act(&ctx,"WHEAT",MenuAct::Remove);assert_eq!(g.watch.len(),n-1);assert!(!g.watch.contains(&"WHEAT".to_string()));
    }
    #[test]
    fn selling_the_inventory_lists_each_item_just_under_the_best_offer() {
        let (mut g,mut rx)=gui();   // COAL ask in the mock data is 5.9
        assert_eq!(g.undercut_price("COAL"),Some(5.8));assert_eq!(g.undercut_price("NO_SUCH_ITEM"),None);
        g.inv_all=vec![InvItem{id:"COAL".into(),count:640,fits:0},InvItem{id:"SOME_RANDOM_THING".into(),count:3,fits:0}];
        let pre=g.sell_inv_preview();assert_eq!(pre,vec![("COAL".to_string(),640,5.8)],"only items the bazaar trades");
        g.send_sell_inventory();
        let c=drain(&mut rx);let m=c.iter().find(|x|x["cmd"]=="sellinv").expect("sent");
        assert_eq!(m["prices"]["COAL"],5.8);assert!(m["prices"].get("SOME_RANDOM_THING").is_none());
        g.on_agent(json!({"type":"sellinv","placed":[{"id":"COAL","qty":640,"price":5.8}],"failed":[],"skipped":[]}));
        assert!(g.toasts.iter().any(|t|t.kind==1&&t.text.contains("1 sell offer")));
        g.on_agent(json!({"type":"sellinv","placed":[],"failed":[{"id":"COAL","err":"cooldown"}],"skipped":[]}));
        assert!(g.toasts.iter().any(|t|t.kind==2));
        g.on_agent(json!({"type":"insert_sacks","moved":2304}));assert!(g.toasts.iter().any(|t|t.text.contains("2,304")));
        g.sacks=vec![SackInfo{name:"Large Enchanted Mining Sack".into(),copies:1,cap_each:20160,items:vec![("Enchanted Coal".into(),0)]}];
        g.inv_all=vec![InvItem{id:"COAL".into(),count:10,fits:0},InvItem{id:"SEEDS".into(),count:3,fits:0}];
        g.on_agent(json!({"type":"insert_sacks","moved":0}));
        assert!(g.toasts.iter().any(|t|t.text.contains("none of your sacks takes: Coal, Seeds")),"{:?}",g.toasts.iter().map(|t|t.text.clone()).collect::<Vec<_>>());
    }
    #[test]
    fn auto_sack_inserts_inventory_after_a_claim() {
        let (mut g,mut rx)=gui();g.agent_up=true;g.auto_sack=false;   // gui() reads the real saved layout: do not depend on it
        let claimed=json!({"type":"claimed","msg":"claimed","product":"ENCHANTED_HARD_STONE","side":"BUY"});
        g.on_agent(claimed.clone());assert!(g.sack_due.is_none(),"off: a claim schedules nothing");
        g.auto_sack=true;g.on_agent(claimed.clone());assert!(g.sack_due.is_some(),"a claim schedules the insert");
        g.auto_sack_tick();assert!(drain(&mut rx).is_empty(),"not due yet (the claim settles first)");
        g.sack_due=Some(Instant::now()-Duration::from_secs(1));g.halted=true;
        g.auto_sack_tick();assert!(drain(&mut rx).is_empty(),"waits while trading is halted");assert!(g.sack_due.is_some());
        g.halted=false;g.sack_due=Some(Instant::now()-Duration::from_secs(1));
        g.auto_sack_tick();let c=drain(&mut rx);assert_eq!(c.len(),1);assert_eq!(c[0]["cmd"],"insert_sacks");assert!(g.sack_due.is_none());
        g.on_agent(claimed);g.sack_due=Some(Instant::now()-Duration::from_secs(1));
        g.auto_sack_tick();assert!(drain(&mut rx).is_empty(),"never twice within 20 s");assert!(g.sack_due.is_some(),"but it is not forgotten");
    }
    #[test]
    fn the_stash_and_lost_and_found_run_on_their_own_every_twenty_minutes_and_wait_for_a_quiet_moment() {
        let (mut g,mut rx)=gui();g.agent_up=true;g.purse=Some(1e9);
        g.auto_pickup_tick();assert!(drain(&mut rx).is_empty(),"off: nothing happens");
        g.auto_stash=true;g.stash_next=Instant::now()+Duration::from_secs(600);
        g.auto_pickup_tick();assert!(drain(&mut rx).is_empty(),"not due yet");
        g.stash_next=Instant::now()-Duration::from_secs(1);
        g.ice_add(job(1000,400,5.0));g.ice_tick();let _=drain(&mut rx);   // an order is being placed right now
        g.auto_pickup_tick();assert!(drain(&mut rx).is_empty(),"waits while an order is being placed");
        assert!(g.stash_next>Instant::now(),"and looks again soon");
        g.on_agent(json!({"type":"placed","msg":"ok","price":1.5}));g.ice_tick();
        g.stash_next=Instant::now()-Duration::from_secs(1);
        g.auto_pickup_tick();let c=drain(&mut rx);
        assert_eq!(c.len(),1,"{c:?}");assert_eq!(c[0]["cmd"],"pickup");assert_eq!(c[0]["stash"],true);assert_eq!(c[0]["lost_found"],false,"only the stash, its switch is the one that is on");
        assert!(g.stash_next>Instant::now()+Duration::from_secs(19*60),"next one in 20 minutes");
        // Lost & Found has its own switch and its own clock
        g.auto_lf=true;g.lf_next=Instant::now()-Duration::from_secs(1);g.on_agent(json!({"type":"pickup","stash":"nothing in the stash"}));
        g.auto_pickup_tick();let c=drain(&mut rx);assert_eq!(c.len(),1);assert_eq!(c[0]["lost_found"],true);assert_eq!(c[0]["stash"],false);
        g.on_agent(json!({"type":"pickup","stash":"took 5,000 item(s) in 6 round(s), 31s - the stash is now empty","lost_found":""}));
        assert!(g.toasts.iter().any(|t|t.kind==1&&t.text.contains("Picked up")));
    }
    #[test]
    fn restart_remembers_unsent_claims_and_a_paused_job_stays_paused() {
        let (mut g1,mut rx1)=resting_job();
        g1.on_agent(json!({"type":"bazaar","msg":"[Bazaar] Your Buy Order for 40000x Coal was filled!"}));   // claim held back
        let _=drain(&mut rx1);
        let (g,_)=reopened(&g1);
        assert_eq!(g.restore_claims.len(),1);assert_eq!(g.restore_claims[0].0,"COAL");
        let (mut g2,_)=resting_job();g2.ice_pause(0);
        let (mut g3,mut rx3)=reopened(&g2);
        g3.ice_tick();drain(&mut rx3);g3.on_agent(OPEN_ROW(0));
        assert_eq!(g3.jobs[0].state,iceberg::JobState::Paused);assert!(g3.jobs[0].cur_trade.is_some());
        g3.ice_tick();assert!(drain(&mut rx3).is_empty());
    }

    #[test]
    fn a_file_from_the_old_version_restores_paused_never_placing_a_second_order() {
        let old=json!({"next_id":2,"jobs":[{"id":1,"product":"COAL","total":100000,"slice":40000,"min":0.1,"max":3.0,"offset":0.1,"requote":300,"filled":0,"state":"Ready","requotes":0,"slices_done":0,"run_secs":5.0}]});
        let (jobs,_,_)=Gui::ice_parse(&old);
        assert_eq!(jobs[0].state,iceberg::JobState::Paused);
    }

    #[test]
    fn waits_instead_of_chasing_above_the_max_price() {
        let (mut g,mut rx)=gui();
        g.ice_add(job(1000,1000,1.4));          // bid 1.4 + 0.1 = 1.5 is above 1.4
        g.ice_tick();
        assert!(drain(&mut rx).is_empty());
        assert!(g.jobs[0].note.contains("above your max"),"{}",g.jobs[0].note);
    }

    #[test]
    fn respects_purse_and_finishes() {
        let (mut g,mut rx)=gui();
        g.purse=Some(30.0);                      // 20 items at 1.5
        g.ice_add(job(1000,1000,3.0));
        g.ice_tick();
        assert_eq!(drain(&mut rx)[0]["qty"],20);
        let (mut g,mut rx)=gui();
        g.ice_add(job(10,10,3.0));g.jobs[0].filled=10;
        g.ice_tick();
        assert!(drain(&mut rx).is_empty());assert_eq!(g.jobs[0].state,iceberg::JobState::Done);
    }

    #[test]
    fn queue_lists_the_requote_countdown() {
        let (mut g,mut rx)=gui();
        g.ice_add(job(100_000,40_000,3.0));g.ice_tick();drain(&mut rx);
        g.on_agent(json!({"type":"placed","msg":"ok","order":{"filled":0,"total":40_000}}));g.ice_tick();
        let rows=g.queue_rows();
        let req=rows.iter().find(|r|r.tag=="REQ").expect("requote row");
        let w=req.when.unwrap();assert!(w>290.0&&w<=300.0,"{w}");
    }

    #[test]
    fn watchlist_survives_a_restart() {
        let dir=std::env::temp_dir().join(format!("bb-watch-test-{}",std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let old=std::env::var_os("HOME");
        std::env::set_var("HOME",&dir);
        let (mut g,_)=gui();
        g.mock=false;                                   // mock sessions never write
        g.watch=vec!["SEEDS".into(),"ENCHANTED_SEEDS".into()];g.selected=Some("ENCHANTED_SEEDS".into());
        g.watch_autosave();
        let loaded=iceberg::load_watch();
        if let Some(h)=old{std::env::set_var("HOME",h);}
        let _=std::fs::remove_dir_all(&dir);
        assert_eq!(loaded,Some((vec!["SEEDS".to_string(),"ENCHANTED_SEEDS".to_string()],Some("ENCHANTED_SEEDS".to_string()))));
    }

    #[test]
    fn sacks_add_capacity_and_search_by_name() {
        let (mut g,_)=gui();
        g.on_sacks(&json!({"sacks":[
            {"name":"Large Mining Sack","copies":8,"cap_each":20160,"items":[{"name":"Coal","stored":100}]},
            {"name":"Small Agronomy Sack","copies":1,"cap_each":2240,"items":[{"name":"Seeds","stored":300}]},
            {"name":"Large Enchanted Agronomy Sack","copies":1,"cap_each":20160,"items":[{"name":"Enchanted Seeds","stored":5}]}]}));
        let coal=g.sack_row("coal").unwrap();assert_eq!((coal.stored,coal.cap),(100,161_280));
        let seedy:Vec<_>=g.sack_rows().into_iter().filter(|r|r.name.to_lowercase().contains("seed")).map(|r|r.name).collect();
        assert_eq!(seedy,vec!["Enchanted Seeds","Seeds"]);
    }

    #[test]
    fn iaof_is_sack_room_minus_incoming() {
        let (mut g,_)=gui();
        g.on_sacks(&json!({"sacks":[{"name":"Small Agronomy Sack","copies":1,"cap_each":1000,"items":[{"name":"Potato","stored":0}]}]}));
        g.orders=vec![OrderRow{side:"BUY".into(),product:"Potato".into(),amount:Some(500),price:Some(1.0),filled:0,total:500,claimable:false,to_claim:0}];
        let r=g.sack_row("Potato").unwrap();
        let (_,_,escrow,buys,_)=g.inv_figures("POTATO");
        assert_eq!((r.cap-r.stored).saturating_sub(escrow+buys),500);
    }

    #[test]
    fn recipe_chain_multiplies_down_to_the_raw_item() {
        assert_eq!(iceberg::raw_chain("ENCHANTED_SEEDS"),("SEEDS".to_string(),160,1));
        assert_eq!(iceberg::raw_chain("BOX_OF_SEEDS"),("SEEDS".to_string(),25_600,2));
        assert_eq!(iceberg::raw_chain("SEEDS"),("SEEDS".to_string(),1,0));
        assert_eq!(20_160u64*iceberg::raw_chain("ENCHANTED_SEEDS").1,3_225_600);
        let (g,_)=gui();assert_eq!(g.id_for_name("Enchanted Seeds"),"ENCHANTED_SEEDS");
    }

    #[test]
    fn editing_a_running_iceberg_changes_its_settings(){
        let (mut g,_rx)=resting_job();
        let id=g.jobs[0].id;
        let e=|total:u64,max:f64,now:bool|iceberg::IceEdit{id,total,slice:20_000,min:0.5,max,offset:0.2,requote_secs:120,auto_claim:true,on_outbid:true,outbid_gap:45,requote_now:now};
        assert!(g.ice_edit(&e(0,5.0,false)).is_err(),"zero total refused");
        assert!(g.ice_edit(&e(100_000,0.1,false)).is_err(),"max below min refused");
        assert!(g.ice_edit(&e(200_000,5.0,true)).is_ok());
        let j=&g.jobs[0];
        assert_eq!((j.total,j.slice,j.max_price,j.requote_secs,j.outbid_gap),(200_000,20_000,5.0,120,45));
        assert!(j.auto_claim&&j.on_outbid&&j.offset==0.2);
        assert!(j.placed_at.elapsed()>=Duration::from_secs(120),"requote now makes it due");
    }

    #[test]
    fn npc_sales_are_counted_towards_the_daily_limit(){
        let (mut g,_rx)=gui();
        assert_eq!(g.npc_sold_today(),0.0);
        g.on_agent(json!({"type":"chat","text":"§aYou sold Enchanted Hard Stone x64 for 1,000 Coins!","ts":0.0}));
        g.on_agent(json!({"type":"chat","text":"[Bazaar] Sold 64x Coal for 5,000 coins","ts":0.0}));   // not an NPC sale
        g.on_agent(json!({"type":"fill","trades":true,"sold":32,"gained":777.0,"signals":{}}));   // the reply does not count: the chat line did
        g.on_agent(json!({"type":"bulksell","msg":"x","items":[],"expected":0.0,"gained":500.0}));   // the bazaar's Sell Inventory Now: not an NPC sale
        g.on_agent(json!({"type":"fill","msg":"bazaar fill","signals":{}}));
        assert_eq!(g.npc_sold_today(),1000.0,"only /trades sales count towards the NPC limit");
        g.npc_sold_day-=1;
        assert_eq!(g.npc_sold_today(),0.0,"a new UTC day starts again at zero");
    }

    #[test]
    fn block_uncrafting_is_not_a_craft_cost(){
        let (g,_rx)=gui();
        for id in ["REDSTONE","COAL","DIAMOND","IRON_INGOT","WHEAT"] { assert!(g.craft_cost(id,true).is_none(),"{id} is not crafted from its own block"); }
    }

    #[test]
    fn a_stale_order_list_does_not_bring_cancelled_orders_back(){
        let (mut g,mut rx)=gui();
        g.on_agent(json!({"type":"orders","orders":[{"side":"BUY","product":"Coal","amount":10,"price":1.5,"filled":0,"total":10,"claimable":false,"to_claim":0}]}));
        assert_eq!(g.orders.len(),1);
        let _=g.cmd_tx.send(json!({"cmd":"cancel","product":"Coal","side":"BUY","price":1.5}));drain(&mut rx);
        g.orders.clear();
        g.on_agent(json!({"type":"orders","orders":[{"side":"BUY","product":"Coal","amount":10,"price":1.5,"filled":0,"total":10,"claimable":false,"to_claim":0}]}));
        assert!(g.orders.is_empty(),"a list read before the cancel finished is ignored");
    }

    #[test]
    fn a_stop_does_not_loop_when_the_order_it_wants_to_cancel_is_gone(){
        let (mut g,_rx)=gui();
        g.purse=Some(1e9);
        g.ice_form.product="COAL".into();g.ice_form.total="1000".into();g.ice_form.max="50".into();g.ice_submit();
        g.ice_tick();
        g.jobs[0].state=iceberg::JobState::Requoting;g.jobs[0].stopping=true;
        g.ice_on_error("no open BUY order for Coal found",Some("requote"));
        assert_eq!(g.jobs[0].state,iceberg::JobState::Stopped,"nothing is left to cancel: it is stopped, not sent to cancel again");
    }

    #[test]
    fn a_partly_filled_bazaar_order_is_flipped_once_a_quarter_of_it_is_in(){
        let (mut g,mut rx)=gui();
        g.orders_at=Some(Instant::now());
        let mut j=autoflip::FlipJob::new("COAL".into());j.total=100_000;j.slice=40_000;j.max_price=3.0;j.margin_pct=2.0;
        g.flip_add(j);
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();drain(&mut rx);
        // 40,000 ordered at 1.5: 3,000 filled is worth 4,500 coins: too little to bother with
        g.orders=vec![OrderRow{side:"BUY".into(),product:"Coal".into(),amount:None,price:Some(1.5),filled:3_000,total:40_000,claimable:true,to_claim:3_000}];
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        assert!(!drain(&mut rx).iter().any(|m|m["cmd"]=="fliporder"),"too little filled");
        // 20,000 filled is worth 30,000 coins: still under a million, nothing yet
        g.orders[0].filled=20_000;g.orders[0].to_claim=20_000;
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        assert!(!drain(&mut rx).iter().any(|m|m["cmd"]=="fliporder"),"under a million coins");
        // a pricey item: 1,000,000 coins worth of a quarter of the order
        g.orders=vec![OrderRow{side:"BUY".into(),product:"Coal".into(),amount:None,price:Some(50.0),filled:25_000,total:100_000,claimable:true,to_claim:25_000}];
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        let sent=drain(&mut rx);
        let f=sent.iter().find(|m|m["cmd"]=="fliporder").expect("the filled part is flipped without waiting for the rest");
        assert_eq!(f["price"].as_f64(),Some(50.0));
    }

    #[test]
    fn the_ticket_auto_requote_keeps_a_sell_offer_on_top_until_it_is_all_sold(){
        let (mut g,mut rx)=gui();
        g.orders_at=Some(Instant::now());g.purse=Some(1e9);
        let lv=|p:f64,a:u64|vec![Level{price:p,amount:a,orders:1}];
        g.products.insert("COAL".into(),Product{sell_summary:lv(5.0,1000),buy_summary:lv(6.0,1000),quick_status:Quick{sell_price:5.0,buy_price:6.0,sell_moving_week:1_000_000,buy_moving_week:1_000_000}});
        g.ticket_auto_requote("COAL",false,100);
        assert_eq!(g.flips.len(),1);assert!(g.flips[0].sell_only);
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        let sent=drain(&mut rx);
        let o=sent.iter().find(|m|m["cmd"]=="selloffer").expect("the first offer goes up");
        assert_eq!((o["qty"].as_u64(),o["price"].as_f64(),o["from_sacks"].as_bool()),(Some(100),Some(5.9),Some(true)),"best ask 6.0 minus 0.1, from the sacks too");
        // (the agent has answered: nothing is in flight any more)
        g.inflight.clear();g.cmd_tx.log.lock().unwrap().clear();
        // it shows up in the order list; nobody undercuts it yet
        g.orders=vec![OrderRow{side:"SELL".into(),product:"Coal".into(),amount:None,price:Some(5.9),filled:0,total:100,claimable:false,to_claim:0}];
        g.flips[0].sell_at=Instant::now()-Duration::from_secs(60);
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        assert!(!drain(&mut rx).iter().any(|m|m["cmd"]=="requote_sell"),"on top: nothing to do");
        // someone lists at 5.7 (their offer is in the book next to ours): re-list at 5.6
        g.products.get_mut("COAL").unwrap().buy_summary=vec![Level{price:5.7,amount:500,orders:1},Level{price:5.9,amount:100,orders:1}];
        g.flips[0].sell_at=Instant::now()-Duration::from_secs(60);
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        let sent=drain(&mut rx);
        let r=sent.iter().find(|m|m["cmd"]=="requote_sell").expect("undercut: re-list");
        assert_eq!((r["price"].as_f64(),r["new_price"].as_f64()),(Some(5.9),Some(5.6)));
        // it never goes below the floor (the top bid, 5.0): a rival at 5.0 means 5.0 at most
        g.flips[0].pending=None;
        g.orders[0].price=Some(5.6);
        g.products.get_mut("COAL").unwrap().buy_summary=vec![Level{price:4.0,amount:500,orders:1},Level{price:5.6,amount:100,orders:1}];
        g.flips[0].sell_at=Instant::now()-Duration::from_secs(60);
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        let sent=drain(&mut rx);
        let r=sent.iter().find(|m|m["cmd"]=="requote_sell").expect("re-list");
        assert_eq!(r["new_price"].as_f64(),Some(5.0),"the floor");
        // everything sold: the offer is gone from the list
        g.flips[0].pending=None;
        g.orders.clear();g.orders_at=Some(Instant::now());
        g.flips[0].placed_at=Instant::now()-Duration::from_secs(120);g.flips[0].sell_at=Instant::now()-Duration::from_secs(120);
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        assert_eq!(g.flips[0].state,autoflip::FlipState::Done);
    }

    #[test]
    fn auto_requote_on_an_offer_that_was_already_listed_never_lists_a_second_one(){
        let (mut g,mut rx)=gui();
        g.orders_at=Some(Instant::now());g.purse=Some(1e9);
        let lv=|p:f64,a:u64|vec![Level{price:p,amount:a,orders:1}];
        g.products.insert("COAL".into(),Product{sell_summary:lv(5.0,1000),buy_summary:lv(6.0,1000),quick_status:Quick{sell_price:5.0,buy_price:6.0,sell_moving_week:1_000_000,buy_moving_week:1_000_000}});
        g.orders=vec![OrderRow{side:"SELL".into(),product:"Coal".into(),amount:None,price:Some(7.0),filled:0,total:100,claimable:false,to_claim:0}];
        g.ticket_auto_requote("COAL",false,100);
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        assert!(!drain(&mut rx).iter().any(|m|m["cmd"]=="selloffer"),"an offer is already up: no second one");
        g.orders.clear();g.orders_at=Some(Instant::now());g.flips[0].placed_at=Instant::now()-Duration::from_secs(120);g.flips[0].sell_at=Instant::now()-Duration::from_secs(120);
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        assert!(!drain(&mut rx).iter().any(|m|m["cmd"]=="selloffer"),"it sold: done, not listed again");
        assert_eq!(g.flips[0].state,autoflip::FlipState::Done);
    }

    #[test]
    fn goods_a_requote_handed_to_the_inventory_are_listed_for_sale_by_a_bazaar_flip(){
        let (mut g,mut rx)=gui();
        g.orders_at=Some(Instant::now());g.purse=Some(1e10);
        let lv=|p:f64,a:u64|vec![Level{price:p,amount:a,orders:1}];
        g.products.insert("GEMS".into(),Product{sell_summary:lv(2_000_000.0,1000),buy_summary:lv(2_200_000.0,1000),quick_status:Quick{sell_price:2_000_000.0,buy_price:2_200_000.0,sell_moving_week:100_000,buy_moving_week:100_000}});
        let mut j=autoflip::FlipJob::new("GEMS".into());j.total=64;j.slice=64;j.max_price=2_100_000.0;j.margin_pct=2.0;
        g.flip_add(j);
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();drain(&mut rx);
        g.inflight.clear();g.cmd_tx.log.lock().unwrap().clear();
        g.inv_all=vec![InvItem{id:"GEMS".into(),count:27,fits:2000}];
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        let sent=drain(&mut rx);
        let o=sent.iter().find(|m|m["cmd"]=="selloffer").expect("27 items worth 54M sit in the inventory: they are listed");
        assert_eq!(o["qty"].as_u64(),Some(27));
        assert!(o["price"].as_f64().unwrap()>=2_000_000.0);
        assert_eq!(g.flips[0].flipped_units,27,"booked as flipped");
        // and not again straight away
        g.inflight.clear();g.cmd_tx.log.lock().unwrap().clear();
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        assert!(!drain(&mut rx).iter().any(|m|m["cmd"]=="selloffer"));
    }

    #[test]
    fn the_ticket_auto_requote_on_a_buy_starts_an_iceberg_that_requotes_when_outbid(){
        let (mut g,_rx)=gui();
        g.purse=Some(1e9);
        let lv=|p:f64,a:u64|vec![Level{price:p,amount:a,orders:1}];
        g.products.insert("COAL".into(),Product{sell_summary:lv(5.0,1000),buy_summary:lv(6.0,1000),quick_status:Quick{sell_price:5.0,buy_price:6.0,sell_moving_week:1_000_000,buy_moving_week:1_000_000}});
        g.ticket_auto_requote("COAL",true,500);
        assert_eq!(g.jobs.len(),1);
        let j=&g.jobs[0];
        assert_eq!((j.total,j.on_outbid,j.auto_claim),(500,true,true));
        assert!(j.max_price>=6.0,"never above the ask, but allowed up to it");
    }

    #[test]
    fn a_coop_partners_top_bid_is_joined_not_outbid_and_its_offers_are_not_undercuts(){
        let (mut g,mut rx)=gui();
        g.purse=Some(1e9);
        let lv=|p:f64,a:u64|vec![Level{price:p,amount:a,orders:1}];
        g.products.insert("COAL".into(),Product{sell_summary:lv(5.0,1000),buy_summary:vec![Level{price:6.0,amount:100,orders:1},Level{price:6.5,amount:500,orders:1}],quick_status:Quick{sell_price:5.0,buy_price:6.0,sell_moving_week:1_000_000,buy_moving_week:1_000_000}});
        // the partner holds the top bid (5.0) and the best offer (6.0, all 100 of that level)
        g.on_agent(json!({"type":"orders","orders":[
            {"side":"BUY","product":"Coal","amount":100,"price":5.0,"filled":0,"total":100,"claimable":false,"to_claim":0,"owner":"partner","mine":false},
            {"side":"SELL","product":"Coal","amount":100,"price":6.0,"filled":0,"total":100,"claimable":false,"to_claim":0,"owner":"partner","mine":false}]}));
        assert!(g.orders.is_empty(),"the partner's orders are not ours");
        assert_eq!(g.coop_join("COAL",10.0),Some(5.0),"join the partner at 5.0 instead of 5.1");
        assert_eq!(g.coop_join("COAL",4.0),None,"not above our max");
        assert_eq!(g.flip_other_ask("COAL"),6.5,"the partner's offer at 6.0 is not someone else's: the next one (6.5) is");
        // an iceberg places its first order at the partner's price
        g.ice_form.product="COAL".into();g.ice_form.total="100".into();g.ice_form.max="10".into();g.ice_submit();
        g.ice_tick();
        let sent=drain(&mut rx);
        let b=sent.iter().find(|m|m["cmd"]=="buyorder").expect("placed");
        assert_eq!((b["top_plus"].as_bool(),b["price"].as_f64()),(Some(false),Some(5.0)));
        // an outsider bids above the partner: back to the normal top + 0.1
        g.products.get_mut("COAL").unwrap().sell_summary=lv(5.5,1000);
        assert_eq!(g.coop_join("COAL",10.0),None);
    }

    #[test]
    fn auto_flip_buys_flips_and_relists(){
        let (mut g,mut rx)=gui();
        g.orders_at=Some(Instant::now());
        // pricing: ask 5.9 - 0.1; a bought price of 5.8 with 2% margin and 1.25% tax needs 5.9/0.9875 -> above the ask: the floor wins
        let mut j=autoflip::FlipJob::new("COAL".into());j.total=100_000;j.slice=40_000;j.max_price=3.0;j.floor=0.0;j.margin_pct=2.0;
        assert_eq!(g.flip_sell_price(&j,1.0),5.8,"best ask 5.9 minus 0.1");
        assert_eq!(g.flip_sell_price(&j,5.8),6.0,"margin floor 5.8*1.02/0.9875 = 5.99 -> 6.0");
        g.flip_add(j);
        assert_eq!(g.flips.len(),1);
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        assert_eq!(g.jobs.len(),1,"the first cycle starts an iceberg");assert!(!g.jobs[0].auto_claim,"slices stay in escrow");
        assert_eq!(g.flips[0].ice_id,g.jobs[0].id);
        drain(&mut rx);
        // a filled slice in escrow: not flipped at once (the iceberg books the fill first) ...
        g.orders=vec![OrderRow{side:"BUY".into(),product:"Coal".into(),amount:None,price:Some(1.5),filled:40_000,total:40_000,claimable:true,to_claim:40_000}];
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        assert!(!drain(&mut rx).iter().any(|m|m["cmd"]=="fliporder"),"waits 15 s");
        for t in g.flips[0].seen.values_mut(){*t=Instant::now()-Duration::from_secs(20);}
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        let sent=drain(&mut rx);
        let f=sent.iter().find(|m|m["cmd"]=="fliporder").expect("flip sent");
        assert_eq!((f["product"].as_str(),f["side"].as_str(),f["price"].as_f64()),(Some("COAL"),Some("BUY"),Some(1.5)));
        assert_eq!(f["sell_price"].as_f64(),Some(5.8));
        g.on_agent(json!({"type":"flipped","msg":"Order Flipped","product":"COAL","side":"BUY","price":1.5,"sell_price":5.8,"qty":40_000}));
        assert_eq!(g.flips[0].flipped_units,40_000);assert!(g.flips[0].est_profit()>0.0);assert!(g.flips[0].pending.is_none());
        // someone lists under our sell offer (ask 5.9 is below a 7.0 offer): re-list it
        g.orders=vec![OrderRow{side:"SELL".into(),product:"Coal".into(),amount:None,price:Some(7.0),filled:0,total:40_000,claimable:false,to_claim:0}];
        g.flips[0].requote_max_qty=100_000;g.flips[0].sell_at=Instant::now()-Duration::from_secs(120);
        drain(&mut rx);
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        let sent=drain(&mut rx);
        let r=sent.iter().find(|m|m["cmd"]=="requote_sell").expect("sell requote sent");
        assert_eq!((r["price"].as_f64(),r["new_price"].as_f64()),(Some(7.0),Some(5.8)));
        g.on_agent(json!({"type":"requoted_sell","product":"COAL","result":"cancelled","returned":40_000,"price":5.8,"placed":"ok"}));
        assert_eq!(g.flips[0].sell_requotes,1);
        // a failed flip counts, three in a row pause the job
        for _ in 0..3 { g.flips[0].pending=Some((Instant::now(),"flip")); g.flip_on_error("no Order Flipped confirmation",Some("fliporder")); }
        assert_eq!(g.flips[0].state,autoflip::FlipState::Paused);
        let back=autoflip::FlipJob::from_json(&g.flips[0].to_json());
        assert_eq!((back.total,back.cycles,back.flipped_units),(100_000,1,40_000),"survives a restart");
    }

    #[test]
    fn auto_flip_does_not_undercut_its_own_offers(){
        let (mut g,mut rx)=gui();
        g.orders_at=Some(Instant::now());
        let mut j=autoflip::FlipJob::new("COAL".into());j.total=1000;j.max_price=3.0;j.cycles=1;j.requote_max_qty=100_000;
        g.flip_add(j);g.flips[0].ice_id=0;g.flips[0].cycles_done=1;   // buying is over: only the sell side matters
        g.flips[0].sell_at=Instant::now()-Duration::from_secs(300);
        let sell=|price:f64,total:u64|OrderRow{side:"SELL".into(),product:"Coal".into(),amount:None,price:Some(price),filled:0,total,claimable:false,to_claim:0};
        g.orders=vec![sell(5.9,323)];
        assert_eq!(g.flip_other_ask("COAL"),6.0,"our 323 at 5.9 are not someone else's");
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        assert!(!drain(&mut rx).iter().any(|m|m["cmd"]=="requote_sell"),"an offer under the others is not undercut");
        g.orders=vec![sell(5.9,323),sell(7.0,500)];
        g.flips[0].sell_at=Instant::now()-Duration::from_secs(300);g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        let r=drain(&mut rx).into_iter().find(|m|m["cmd"]=="requote_sell").expect("the 7.0 offer is above the 6.0 level of other sellers");
        assert_eq!((r["price"].as_f64(),r["new_price"].as_f64()),(Some(7.0),Some(5.9)));
    }

    #[test]
    fn coins_are_claimed_every_ten_minutes_when_there_are_sell_offers(){
        let (mut g,mut rx)=gui();
        let sell=OrderRow{side:"SELL".into(),product:"Coal".into(),amount:None,price:Some(5.9),filled:100,total:500,claimable:true,to_claim:100};
        g.auto_coins=true;g.coins_next=Instant::now()+Duration::from_secs(600);
        g.orders=vec![sell.clone()];g.auto_coins_tick();assert!(drain(&mut rx).is_empty(),"not due yet");
        g.coins_next=Instant::now()-Duration::from_secs(1);g.orders.clear();g.auto_coins_tick();
        assert!(drain(&mut rx).is_empty(),"no sell offer: nothing to collect");assert!(g.coins_next>Instant::now()+Duration::from_secs(9*60));
        g.coins_next=Instant::now()-Duration::from_secs(1);g.orders=vec![sell.clone()];g.auto_coins_tick();
        let sent=drain(&mut rx);assert_eq!(sent.iter().map(|m|m["cmd"].as_str().unwrap_or("")).collect::<Vec<_>>(),vec!["claim","orders"]);
        g.coins_next=Instant::now()-Duration::from_secs(1);g.agent_up=false;g.auto_coins_tick();
        assert!(drain(&mut rx).is_empty(),"agent offline: waits");assert!(g.coins_next<Instant::now()+Duration::from_secs(60),"and looks again soon");
        g.agent_up=true;g.auto_coins=false;g.coins_next=Instant::now()-Duration::from_secs(1);g.auto_coins_tick();assert!(drain(&mut rx).is_empty(),"switched off");
    }

    #[test]
    fn auto_flip_projects_a_time_and_records_finished_cycles(){
        let (mut g,_rx)=gui();
        let lv=|p:f64,a:u64|vec![Level{price:p,amount:a,orders:1}];
        g.products.insert("TESTITEM".into(),Product{sell_summary:lv(100.0,10_000),buy_summary:lv(120.0,10_000),quick_status:Quick{sell_price:100.0,buy_price:120.0,sell_moving_week:168_000,buy_moving_week:336_000}});
        let mut j=autoflip::FlipJob::new("TESTITEM".into());j.total=2000;j.slice=1000;j.max_price=150.0;j.margin_pct=0.0;
        let e=g.flip_eta(&j).expect("has volume");
        // buying fills at 1000/h (168000/168): 2 slices = 2 h, plus 25 s placing each; selling 2000/h
        assert!((e.buy_secs-(7200.0+2.0*autoflip::PLACE_SECS)).abs()<1.0,"buy phase {}",e.buy_secs);
        assert!(e.total_secs>e.buy_secs && e.total_secs<e.buy_secs+2000.0,"selling the last slice takes about half an hour: {}",e.total_secs);
        assert!(e.net_per_unit>0.0&&e.curve.windows(2).all(|w|w[1].1>=w[0].1),"profit only grows");
        assert_eq!(autoflip::fmt_eta(7200.0),"2.0 h");
        // no trading volume: no estimate
        g.products.get_mut("TESTITEM").unwrap().quick_status.sell_moving_week=0;assert!(g.flip_eta(&j).is_none());
        // a finished cycle becomes a history row with both times
        g.flip_add(j.clone());
        g.flips[0].ice_id=0;g.flips[0].cycles_done=1;g.flips[0].flipped_units=2000;g.flips[0].buy_cost=2000.0*100.0;g.flips[0].list_value=2000.0*118.0;
        let now=now_secs();
        g.flips[0].pending_cycles=vec![autoflip::PendingCycle{n:1,start_t:now-5000.0,proj_secs:7000.0,proj_buy:6000.0,buy_done_t:Some(now-1000.0),..Default::default()}];
        g.orders_at=Some(Instant::now());g.orders=vec![];
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        assert_eq!(g.flip_cycles.len(),1,"nothing left to sell: the cycle is done");
        let c=&g.flip_cycles[0];
        assert_eq!((c.units,c.n),(2000,1));assert!((c.actual_secs-5000.0).abs()<5.0&&(c.actual_buy-4000.0).abs()<5.0&&c.proj_secs==7000.0);
        assert!(c.profit>0.0&&(c.buy_avg-100.0).abs()<1e-6);
        assert!(g.flips[0].pending_cycles.is_empty());
    }

    #[test]
    fn auto_flip_can_sell_through_the_trades_menu(){
        let (mut g,mut rx)=gui();
        g.orders_at=Some(Instant::now());
        let mut j=autoflip::FlipJob::new("ENCHANTED_COAL".into());j.total=2000;j.slice=2000;j.max_price=300.0;j.margin_pct=2.0;j.exchange=1;j.tax_pct=0.0;j.requote_max_qty=1500;
        g.flip_add(j);g.flips[0].cycles_done=1;g.flips[0].ice_id=0;   // buying is over
        let row=OrderRow{side:"BUY".into(),product:"Enchanted Coal".into(),amount:None,price:Some(100.0),filled:2000,total:2000,claimable:true,to_claim:2000};
        g.orders=vec![row];
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();assert!(drain(&mut rx).is_empty(),"waits 15 s before it touches a fresh slice");
        for t in g.flips[0].seen.values_mut(){*t=Instant::now()-Duration::from_secs(20);}
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        let sent=drain(&mut rx);
        let c=sent.iter().find(|m|m["cmd"]=="claimorder").expect("claims the slice first");
        assert_eq!((c["side"].as_str(),c["sack_on_full"].as_bool()),(Some("BUY"),Some(true)));
        g.on_agent(json!({"type":"claimed","msg":"claimed","product":"ENCHANTED_COAL","side":"BUY","price":100.0,"signals":{},"info":{"claimed_units":2600}}));
        assert_eq!((g.flips[0].backlog,g.flips[0].flipped_units),(2600,2600),"the claim also took a second order at the same price: all of it is counted");assert!(g.orders.is_empty(),"the claimed slice leaves the list");
        // the claim never counts as a blotter order of its own
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        let sent=drain(&mut rx);
        let t=sent.iter().find(|m|m["cmd"]=="tradesellqty").expect("then it sells to the NPC");
        assert_eq!((t["qty"].as_u64(),t["from_sacks"].as_bool(),t["mode"].as_str()),(Some(1500),Some(true),Some("qty")),"in rounds of the max per sell order");
        g.on_agent(json!({"type":"fill","msg":"sold","trades":true,"sold":1500,"gained":240_000.0,"signals":{}}));
        g.on_agent(json!({"type":"chat","text":"You sold Enchanted Coal x64 for 240,000 Coins!","ts":0.0}));
        assert_eq!(g.flips[0].backlog,1100);assert_eq!(g.npc_sold_today(),240_000.0,"the game's chat line counts towards the daily NPC limit");
        // the daily NPC limit holds the rest back
        g.npc_sold=NPC_SELL_CAP-1000.0;g.npc_sold_day=Gui::utc_day();
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        assert!(!drain(&mut rx).iter().any(|m|m["cmd"]=="tradesellqty"));assert!(g.flips[0].note.contains("daily limit"));
        g.npc_sold=0.0;g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        let sent=drain(&mut rx);assert_eq!(sent.iter().find(|m|m["cmd"]=="tradesellqty").map(|m|m["qty"].as_u64()),Some(Some(1100)));
        g.on_agent(json!({"type":"fill","msg":"sold","trades":true,"sold":1100,"gained":176_000.0,"signals":{}}));
        assert_eq!(g.flips[0].backlog,0);
    }

    #[test]
    fn trades_flip_can_sell_everything_in_one_go(){
        let (mut g,mut rx)=gui();
        let mut j=autoflip::FlipJob::new("ENCHANTED_COAL".into());j.total=2000;j.max_price=300.0;j.margin_pct=2.0;j.exchange=1;j.sell_all=true;j.requote_max_qty=100;
        g.flip_add(j);g.flips[0].cycles_done=1;g.flips[0].ice_id=0;g.flips[0].backlog=2500;
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        let sent=drain(&mut rx);let t=sent.iter().find(|m|m["cmd"]=="tradesellqty").expect("sells");
        assert_eq!((t["mode"].as_str(),t["qty"].is_null(),t["from_sacks"].as_bool()),(Some("all"),true,Some(true)),"no per-sale limit");
        g.on_agent(json!({"type":"fill","msg":"sold","trades":true,"sold":2700,"gained":432_000.0,"signals":{}}));
        assert_eq!(g.flips[0].backlog,0,"it sold more than the flip's own goods: nothing is left waiting");
    }

    #[test]
    fn the_projected_pnl_scales_to_any_number_of_cycles(){
        // one cycle: 0 until t=100, then +1000 by t=200; the next cycle starts 60 s later
        let curve=vec![(0.0,0.0),(100.0,0.0),(200.0,1000.0)];
        let brute=|n:usize,t:f64|->f64{(0..n).map(|c|{let x=t-c as f64*60.0;if x<=100.0{0.0}else if x>=200.0{1000.0}else{(x-100.0)*10.0}}).sum()};
        for n in [1usize,2,10,11,25,300] {
            for t in [0.0,50.0,150.0,260.0,500.0,(n as f64-1.0)*60.0+200.0] {
                let got=autoflip::projected_profit(&curve,60.0,200.0,n,t);
                assert!((got-brute(n,t)).abs()<1e-6,"n={n} t={t}: {got} vs {}",brute(n,t));
            }
        }
        assert_eq!(autoflip::projected_profit(&curve,60.0,200.0,25,(24.0)*60.0+200.0),25_000.0,"25 cycles earn 25 times one cycle (it used to stop at 10)");
    }

    #[test]
    fn trades_flip_sells_what_fits_under_the_limit_and_pauses_buying_when_it_is_used_up(){
        let (mut g,mut rx)=gui();
        g.orders_at=Some(Instant::now());
        let mut j=autoflip::FlipJob::new("ENCHANTED_COAL".into());j.total=10_000;j.slice=5000;j.max_price=300.0;j.margin_pct=2.0;j.exchange=1;j.tax_pct=0.0;j.requote_max_qty=100_000;j.cycles=3;
        g.flip_add(j);
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        assert_eq!(g.jobs.len(),1,"the first buy iceberg runs");let ice=g.jobs[0].id;
        // room for 3,000 items (160 each) of a 5,000 backlog: sells the whole stacks that fit
        g.flips[0].backlog=5000;g.npc_sold=NPC_SELL_CAP-3000.0*160.0;g.npc_sold_day=Gui::utc_day();drain(&mut rx);
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        let sent=drain(&mut rx);let t=sent.iter().find(|m|m["cmd"]=="tradesellqty").expect("sells part of it");
        assert_eq!((t["qty"].as_u64(),t["mode"].as_str()),(Some(2944),Some("qty")),"3,000 rounded down to whole stacks");
        // the limit is used up: buying pauses, no new cycle starts, and it all comes back with the reset
        g.flips[0].pending=None;g.flips[0].backlog=2000;g.npc_sold=NPC_SELL_CAP;
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        assert!(g.flips[0].cap_blocked,"blocked");
        assert_eq!(g.jobs.iter().find(|x|x.id==ice).unwrap().state,iceberg::JobState::Paused,"the buy iceberg waits");
        assert!(!drain(&mut rx).iter().any(|m|m["cmd"]=="tradesellqty"),"nothing is sold");
        g.npc_sold=0.0;   // a new UTC day
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        assert!(!g.flips[0].cap_blocked&&!g.flips[0].paused_ice,"unblocked");
        assert_ne!(g.jobs.iter().find(|x|x.id==ice).unwrap().state,iceberg::JobState::Paused,"the buy iceberg carries on");
    }

    #[test]
    fn a_lost_sale_reply_does_not_stall_a_trades_flip(){
        let (mut g,mut rx)=gui();
        g.orders_at=Some(Instant::now());
        let mut j=autoflip::FlipJob::new("ENCHANTED_COAL".into());j.total=2000;j.max_price=300.0;j.margin_pct=2.0;j.exchange=1;j.tax_pct=0.0;j.sell_all=true;
        g.flip_add(j);g.flips[0].cycles_done=1;g.flips[0].ice_id=0;g.flips[0].backlog=2000;   // the sale went through, but its answer was lost in a restart
        g.flip_next=Instant::now()-Duration::from_secs(5);g.flip_tick();
        assert!(drain(&mut rx).iter().any(|m|m["cmd"]=="tradesellqty"));
        // meanwhile an iceberg is placing an order of its own
        g.inflight.push_back(0);g.trades.push(Trade{time:clock(),side:"BUY",id:"ENCHANTED_COAL".into(),qty:100,price:1.0,kind:Kind::Limit,status:Status::Working,sig:Sig::default()});
        g.on_agent(json!({"type":"error","cmd":"tradesellqty","msg":"no Enchanted Coal in the inventory or sacks to sell"}));
        assert_eq!(g.flips[0].backlog,0,"nothing left: the backlog was stale");assert_eq!(g.flips[0].fails,0);assert_eq!(g.flips[0].state,autoflip::FlipState::Running);
        assert!(g.trades[0].status==Status::Working&&g.inflight.len()==1,"the error was not taken for the iceberg's own order");
    }

    #[test]
    fn eta_columns_match_the_profit_they_belong_to(){
        let (mut g,_rx)=gui();
        let lv=|p:f64|vec![Level{price:p,amount:1_000_000,orders:1}];
        let prod=|vol:u64|Product{sell_summary:lv(100.0),buy_summary:lv(110.0),quick_status:Quick{sell_price:100.0,buy_price:110.0,sell_moving_week:vol,buy_moving_week:vol}};
        // ENCHANTED_COAL: 160 COAL each, the NPC pays 160: 3,125,000 items fill the 500M limit (= 500M raw coal)
        g.products.insert("ENCHANTED_COAL".into(),prod(168*400_000));g.products.insert("COAL".into(),prod(168*2_000_000));
        // crafting needs 500M coal at 60% of 2M/h = 417 h; buying the item itself needs 3.125M at 60% of 400k/h = 13 h
        let (h,what,need)=g.cnsmp_eta("ENCHANTED_COAL").expect("craft estimate");
        assert_eq!(what,"COAL");assert!((h-500_000_000.0/(2_000_000.0*screener::BUY_CAPTURE)).abs()<0.01&&need==500_000_000.0,"CNSMP-ETA is the raw-material route: {h}");
        let d=g.bnsmp_eta("ENCHANTED_COAL").expect("direct estimate");
        assert!((d-3_125_000.0/(400_000.0*screener::BUY_CAPTURE)).abs()<0.01,"BNSMP-ETA buys the item itself: {d}");
        assert!(d<h/10.0,"so a profit that needs the cheap-to-craft route is not given the fast direct time");
        assert!(g.bnsmp_eta("COAL").is_some()||npc_sell_of("COAL").is_none());
        // BNSMP is the bid-to-NPC profit at the same unit count
        let bnsmp=g.screener_value_for_tests("ENCHANTED_COAL",12).unwrap();
        assert!((bnsmp-(160.0-100.1)/160.0*NPC_SELL_CAP).abs()<1.0,"{bnsmp}");
    }

    #[test]
    fn the_npc_calculator_opens_from_the_header_state_and_counts_what_still_fits(){
        let (mut g,_rx)=gui();
        g.npc_sold=NPC_SELL_CAP-1_000_000.0;g.npc_sold_day=Gui::utc_day();
        // Enchanted Hard Stone pays 576: 1M of room fits 1,736 items
        let left=(NPC_SELL_CAP-g.npc_sold_today()).max(0.0);let npc=npc_sell_of("ENCHANTED_HARD_STONE").unwrap();
        assert_eq!((left/npc).floor() as u64,1736);
        g.npc_calc=Some(NpcCalc{item:"ENCHANTED_HARD_STONE".into(),have:"2000".into(),search_open:false});
        let ctx=egui::Context::default();
        let _=ctx.run(Default::default(),|ctx|g.npc_calc_dialog(ctx));   // renders without panicking
        assert!(g.npc_calc.is_some());
    }

    #[test]
    fn old_overlapping_cycle_rows_are_repaired_to_their_own_numbers() {
        let mk = |n: u32, units: u64, start: f64| autoflip::FlipCycle { t: 1000.0, id: 3, product: "X".into(), n, units, buy_avg: 10.0, list_avg: 12.0, profit: units as f64 * 2.0, proj_secs: 100.0, actual_secs: 1000.0 - start, proj_buy: 50.0, actual_buy: 40.0, v: 1 };
        let mut rows = vec![mk(1, 300, 0.0), mk(2, 300, 100.0), mk(3, 200, 300.0)];
        autoflip::repair_legacy_cycles(&mut rows);
        assert_eq!(rows.iter().map(|c| c.units).sum::<u64>(), 300, "units are counted once: the old rows summed to 800");
        assert_eq!(rows.len(), 2, "a cycle with no flips of its own folds into the next row");
        assert!(rows.iter().all(|c| c.v == 2));
        assert!((rows.iter().map(|c| c.profit).sum::<f64>() - 600.0).abs() < 1e-6);
    }

    #[test]
    fn the_apps_own_checks_do_not_keep_the_agent_awake(){
        let (mut g,mut rx)=gui();
        g.mock=false;g.auto_sleep=true;g.agent_up=true;
        // a poll of the order menu is pending (auto) and nothing was done by hand for half an hour
        let _=g.cmd_tx.send(json!({"cmd":"orders","auto":true}));drain(&mut rx);
        g.last_activity=Instant::now()-Duration::from_secs(30*60);
        bb::LAST_USER_CMD.store(0,std::sync::atomic::Ordering::Relaxed);
        g.sleep_tick();
        assert!(g.agent_sleeping,"auto checks alone no longer count as activity");
        // a command the user queued by hand does
        let (mut g,mut rx)=gui();
        g.mock=false;g.auto_sleep=true;g.agent_up=true;
        let _=g.cmd_tx.send(json!({"cmd":"claim"}));drain(&mut rx);
        g.last_activity=Instant::now()-Duration::from_secs(30*60);
        g.sleep_tick();
        assert!(!g.agent_sleeping,"a pending command of the user keeps it awake");
        // and so does a running auto flip
        let (mut g,_rx)=gui();
        g.mock=false;g.auto_sleep=true;g.agent_up=true;
        let mut j=autoflip::FlipJob::new("COAL".into());j.total=10;j.max_price=3.0;g.flips.push(j);
        g.last_activity=Instant::now()-Duration::from_secs(30*60);
        bb::LAST_USER_CMD.store(0,std::sync::atomic::Ordering::Relaxed);
        g.sleep_tick();
        assert!(!g.agent_sleeping,"a running flip needs the agent");
        g.flips[0].state=autoflip::FlipState::Done;g.last_activity=Instant::now()-Duration::from_secs(30*60);
        g.sleep_tick();
        assert!(g.agent_sleeping,"once the flip is done it can sleep");
    }

    #[test]
    fn inventory_figures_add_up() {
        let (mut g,_)=gui();
        g.on_invall(&json!({"free_slots":10,"items":[{"id":"SEEDS","count":640,"fits":1024}]}));
        g.orders=vec![OrderRow{side:"BUY".into(),product:"Seeds".into(),amount:Some(5000),price:Some(1.0),filled:1000,total:5000,claimable:true,to_claim:1000}];
        let (hold,fits,escrow,buys,_)=g.inv_figures("SEEDS");
        assert_eq!((hold,fits,escrow,buys),(640,1024,1000,4000));
    }
}

/// Where the account menu opens when it is asked for from the command bar (near the top right of a typical window).
fn ctx_right_hint() -> f32 { 1500.0 }

#[cfg(test)]
mod chart_option_tests {
    use super::*;
    /// Every toolbar option of the chart, in every combination: what the page is sent must match what was picked,
    /// and there must be something to draw.
    #[test]
    fn every_chart_option_combination() {
        let mut g = Gui::with_context(&egui::Context::default(), "mockup".into(), server::Mode::Standalone);
        let id = "ZZ_TEST_ITEM".to_string();
        let now = now_secs().floor();
        // 3 h of per-second samples and per-20 s history, prices moving
        let fine: VecDeque<(f64, f64, f64)> = (0..10_800).map(|i| { let t = now - 10_800.0 + i as f64; (t, 100.0 + (i / 5 % 7) as f64, 90.0 + (i / 5 % 5) as f64) }).collect();
        g.fine.insert(id.clone(), fine.clone());
        g.history.insert(id.clone(), fine.iter().step_by(20).copied().collect());
        let ranges = [1440.0, 4320.0, 43200.0, 259200.0, 525600.0, 525601.0, 2628000.0, 5256000.0];
        let tfs = [0i64, -1, -15, -30, 1, 5, 15, 60, 240, 1440];
        let types = ["candles", "hollow", "bars", "line"];
        let mut checked = 0;
        for ty in types { for tf in tfs { for series in 0..3u64 { for range in ranges {
            for (k, v) in [("type", json!(ty)), ("tf", json!(tf)), ("series", json!(series)), ("range", json!(range))] { g.web_msg(&json!({"k": k, "v": v})); }
            let p: Value = serde_json::from_str(&g.web_payload(&id)).unwrap();
            let what = format!("type={ty} tf={tf} series={series} range={range}");
            assert_eq!(p["type"], if ty == "line" { "line" } else { ty }, "{what}");
            assert_eq!(p["tfMin"], tf, "{what}");
            assert_eq!(p["seriesIdx"], series, "{what}");
            assert_eq!(p["rangeMin"], range, "{what}");
            let eff = g.effective_tf();
            if tf != 0 { assert_eq!(eff, tf, "{what}: the picked size must be used, not Auto"); }
            assert_eq!(p["tfLabel"], tf_name(eff), "{what}");
            if ty == "line" {
                let a = p["ask"].as_array().unwrap();
                assert!(!a.is_empty() && a.len() == p["bid"].as_array().unwrap().len(), "{what}: line has no points");
                if tf < 0 { let t: Vec<i64> = a.iter().map(|x| x["time"].as_i64().unwrap()).collect(); assert!(t.windows(2).all(|w| w[1] > w[0]), "{what}"); assert!(a.len() == 900, "{what}: seconds line: the newest 900 points expected"); }
            } else {
                let c = p["candles"].as_array().unwrap();
                assert!(!c.is_empty(), "{what}: no candles");
                let t: Vec<i64> = c.iter().map(|x| x["time"].as_i64().unwrap()).collect();
                let step = if eff < 0 { -eff } else { eff * 60 };
                assert!(t.iter().all(|x| x % step == 0), "{what}: candle times not on {step}s boundaries");
                assert!(t.windows(2).all(|w| w[1] > w[0]), "{what}: candle times not increasing");
                if tf == -1 { assert!(c.len() == 900, "{what}: 1 s candles: the newest 900 expected, got {}", c.len()); }
                if tf == -15 { assert!(c.len() >= 700, "{what}: 15 s candles: got {}", c.len()); }
                let s = &c[c.len() / 2]; assert!(s["high"].as_f64() >= s["low"].as_f64(), "{what}");
            }
            checked += 1;
        } } } }
        assert_eq!(checked, 4 * 10 * 3 * 8);
    }
}

/// Whether a user systemd unit is running, cached for a few seconds and refreshed in the background (safe to call every frame).
fn unit_active(unit: &str) -> bool {
    use std::sync::{Mutex, OnceLock};
    static C: OnceLock<Mutex<HashMap<String, (Instant, bool, bool)>>> = OnceLock::new();
    let m = C.get_or_init(|| Mutex::new(HashMap::new()));
    let mut g = m.lock().unwrap();
    let e = g.entry(unit.to_string()).or_insert((Instant::now() - Duration::from_secs(60), false, false));
    if e.0.elapsed() > Duration::from_secs(3) && !e.2 && !cfg!(test) {
        e.2 = true;
        let u = unit.to_string();
        std::thread::spawn(move || {
            let up = std::process::Command::new("systemctl").args(["--user", "is-active", "--quiet", &u]).status().map_or(false, |s| s.success());
            if let Some(m) = C.get() { if let Ok(mut g) = m.lock() { g.insert(u, (Instant::now(), up, false)); } }
        });
    }
    e.1
}
