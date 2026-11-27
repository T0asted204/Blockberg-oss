//! Iceberg orders, the bot queue screen, the inventory screen and the saved watchlist.
//! Included from gui.rs (`mod iceberg`), so it can see the `Gui` fields directly.
//!
//! Iceberg job (buy side): keep one bazaar buy order of up to `slice` items resting at
//! `top bid + offset` (never above `max`). If it fills, place the next slice at once. If it has not
//! completely filled after `requote_secs`, read the order menu, cancel what is left (Hypixel hands the
//! filled part over on cancel) and place a fresh order at the new top bid. Stops when `total` is bought.
use super::*;

/// However long "requote if outbid" is set to wait, it never waits longer than this: a requote takes the agent about 15 s anyway, and waiting 30 s on top of that
/// lost the top of the book to anyone who bids faster.
pub const OUTBID_MAX_GAP: u64 = 5;
/// Orders one account of a co-op may have open (the bazaar allows 14 for the whole co-op).
pub const COOP_ORDER_SHARE: usize = 7;
pub const MAX_ORDER_QTY: u64 = 71_680;
pub const DEFAULT_REQUOTE_SECS: u64 = 300;
const RETRY_AFTER_REJECT: u64 = 30;
const STEP_TIMEOUT: u64 = 90;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Screen { Main, Queue, Inventory, Portfolio, Screener, AutoFlip }

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum JobState { Idle, Placing, Resting, Requoting, Adopting, Paused, Done, Stopped }

impl JobState {
    pub fn from_label(l: &str) -> JobState {
        [JobState::Idle, JobState::Placing, JobState::Resting, JobState::Requoting, JobState::Adopting, JobState::Paused, JobState::Done, JobState::Stopped].into_iter().find(|s| s.label() == l).unwrap_or(JobState::Idle)
    }
    pub fn live(self) -> bool { matches!(self, JobState::Idle | JobState::Placing | JobState::Resting | JobState::Requoting | JobState::Adopting) }
    pub fn label(self) -> &'static str {
        match self {
            JobState::Idle => "Ready", JobState::Placing => "Placing", JobState::Resting => "Resting", JobState::Requoting => "Requoting", JobState::Adopting => "Reconnecting", JobState::Paused => "Paused", JobState::Done => "Done", JobState::Stopped => "Stopped",
        }
    }
}

#[derive(Clone)]
pub struct IceJob {
    pub id: u32,
    pub product: String,
    pub total: u64,
    pub slice: u64,
    pub min_price: f64,
    pub max_price: f64,
    pub offset: f64,
    pub requote_secs: u64,
    pub filled: u64,
    pub state: JobState,
    pub cur_trade: Option<usize>,
    pub cur_qty: u64,
    pub cur_price: f64,
    pub cur_filled: u64,
    pub cancel_filled: u64,
    pub placed_at: Instant,
    pub state_at: Instant,
    pub retry_at: Option<Instant>,
    pub stopping: bool,
    pub fails: u32,
    pub requotes: u32,
    pub auto_resumes: u32,
    pub slices_done: u32,
    pub run_secs: f64,
    pub note: String,
    /// Claim each filled slice out of escrow automatically. Off by default: slices then stay in escrow for you to claim.
    pub auto_claim: bool,
    /// Requote as soon as someone bids above this job's order (at most once per `outbid_gap` seconds), not only on the timer.
    pub on_outbid: bool,
    pub outbid_gap: u64,
    /// From the last session: (qty, price, placed unix time, state it was in, paused). The order is looked up in game and taken over.
    pub adopt: Option<AdoptHint>,
}

#[derive(Clone)]
pub struct IceEdit { pub id: u32, pub total: u64, pub slice: u64, pub min: f64, pub max: f64, pub offset: f64, pub requote_secs: u64, pub auto_claim: bool, pub on_outbid: bool, pub outbid_gap: u64, pub requote_now: bool }

#[derive(Clone)]
pub struct AdoptHint { pub qty: u64, pub price: f64, pub placed_unix: f64, pub requoting: bool, pub paused: bool, pub tries: u32, pub sent_at: Option<Instant> }

impl IceJob {
    pub fn new(id: u32, product: String, total: u64, slice: u64, min_price: f64, max_price: f64, offset: f64, requote_secs: u64) -> Self {
        let now = Instant::now();
        IceJob { id, product, total, slice, min_price, max_price, offset, requote_secs, filled: 0, state: JobState::Idle, cur_trade: None, cur_qty: 0,
            cur_price: 0.0, cur_filled: 0, cancel_filled: 0, placed_at: now, state_at: now, retry_at: None, stopping: false, fails: 0, requotes: 0, auto_resumes: 0,
            slices_done: 0, run_secs: 0.0, note: String::new(), auto_claim: false, on_outbid: false, outbid_gap: 30, adopt: None }
    }
    pub fn remaining(&self) -> u64 { self.total.saturating_sub(self.filled) }
    pub fn has_live_order(&self) -> bool { self.cur_trade.is_some() }
}

#[derive(Default, Clone)]
pub struct IceForm {
    pub product: String,
    pub total: String,
    pub min: String,
    pub max: String,
    pub offset: String,
    pub requote: String,
    pub slice: String,
    pub auto_claim: bool,
    pub on_outbid: bool,
    pub gap: String,
    pub msg: Option<(bool, String)>,
}

impl IceForm {
    pub fn new() -> Self {
        IceForm { gap: "30".into(), offset: "0.1".into(), requote: "5".into(), slice: MAX_ORDER_QTY.to_string(), min: "0.1".into(), ..Default::default() }
    }
}

#[derive(Clone)]
pub struct SackInfo { pub name: String, pub copies: u64, pub cap_each: u64, pub items: Vec<(String, u64)> }

/// One item as the sacks see it: stored, total capacity (copies add up), and which sacks hold it.
pub struct SackRow { pub name: String, pub stored: u64, pub cap: u64, pub sacks: Vec<String> }

#[derive(Clone)]
pub struct InvItem { pub id: String, pub count: u64, pub fits: u64 }
/// One occupied inventory slot (0-8 hotbar, 9-35 main).
#[derive(Clone)]
pub struct InvSlot { pub slot: u8, pub id: String, pub count: u64 }

/// One line of the queue screen.
#[derive(Clone)]
pub struct QRow { pub depth: u8, pub tag: &'static str, pub text: String, pub detail: String, pub color: Color32, pub when: Option<f64>, pub est: Option<f64> }

/// "2m", "500k", "1,500,000" -> number.
pub fn parse_qty(s: &str) -> Option<u64> {
    let t: String = s.trim().to_lowercase().chars().filter(|c| !matches!(c, ',' | '_' | ' ')).collect();
    if t.is_empty() { return None; }
    let (num, mult) = match t.chars().last()? { 'k' => (&t[..t.len() - 1], 1e3), 'm' => (&t[..t.len() - 1], 1e6), 'b' => (&t[..t.len() - 1], 1e9), _ => (&t[..], 1.0) };
    let v: f64 = num.parse().ok()?;
    if v <= 0.0 || !v.is_finite() { return None; }
    Some((v * mult).round() as u64)
}

pub fn fmt_dur(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    if s >= 86_400 { format!("{}d {:02}h", s / 86_400, s / 3600 % 24) }
    else if s >= 3600 { format!("{}h {:02}m", s / 3600, s / 60 % 60) }
    else if s >= 60 { format!("{}m {:02}s", s / 60, s % 60) }
    else { format!("{s}s") }
}

/// An Instant `secs` seconds in the past (clamped, so it can never underflow right after boot).
fn ago(secs: u64) -> Instant { let now = Instant::now(); now.checked_sub(Duration::from_secs(secs)).unwrap_or(now) }

fn data_path(name: &str) -> Option<std::path::PathBuf> {
    Some(crate::bb::profile::data_dir().join(name))
}
fn write_data(name: &str, text: String) {
    if let Some(p) = data_path(name) {
        if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
        // write-then-rename so a crash never leaves a half-written file
        let tmp = p.with_extension("tmp");
        if std::fs::write(&tmp, text).is_ok() { let _ = std::fs::rename(&tmp, &p); }
    }
}

/// The saved watchlist: (symbols, last selected symbol).
pub fn load_watch() -> Option<(Vec<String>, Option<String>)> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(data_path("watchlist.json")?).ok()?).ok()?;
    let list: Vec<String> = v["watch"].as_array()?.iter().filter_map(|x| x.as_str().map(String::from)).collect();
    if list.is_empty() { return None; }
    Some((list, v["selected"].as_str().map(String::from)))
}

pub fn same_product(display: &str, id: &str) -> bool {
    display.to_uppercase().replace(' ', "_") == id || pretty(id).to_lowercase() == display.to_lowercase()
}

impl Gui {
    // ---------- saved watchlist ----------
    pub fn watch_autosave(&mut self) {
        if self.mock { return; }
        let snap = (self.watch.clone(), self.selected.clone());
        if snap == self.saved_watch { return; }
        self.save_watch();
        self.saved_watch = snap;
    }
    pub fn save_watch(&self) {
        if self.mock { return; }
        write_data("watchlist.json", json!({"watch": self.watch, "selected": self.selected}).to_string());
    }
    pub fn view_save_tick(&mut self) { if !self.mock { self.view_autosave(); } }

    // ---------- iceberg persistence ----------
    pub fn ice_json(&self, with_run: bool) -> String {
        let jobs: Vec<Value> = self.jobs.iter().map(|j| {
            let mut v = json!({
                "id": j.id, "product": j.product, "total": j.total, "slice": j.slice, "min": j.min_price, "max": j.max_price, "offset": j.offset,
                "requote": j.requote_secs, "filled": j.filled, "state": j.state.label(), "requotes": j.requotes, "slices_done": j.slices_done,
                "stopping": j.stopping, "auto_claim": j.auto_claim, "on_outbid": j.on_outbid, "outbid_gap": j.outbid_gap,
            });
            if with_run { v["run_secs"] = json!(j.run_secs); }
            // the live order, so a restart can take it over again
            let live = j.cur_trade.is_some() || j.adopt.is_some();
            if live && j.state != JobState::Done && j.state != JobState::Stopped {
                let h = j.adopt.as_ref();
                v["order"] = json!({
                    "qty": if j.cur_trade.is_some() { j.cur_qty } else { h.map_or(0, |h| h.qty) },
                    "price": if j.cur_trade.is_some() { j.cur_price } else { h.map_or(0.0, |h| h.price) },
                    "placed_unix": if j.cur_trade.is_some() { now_secs() - j.placed_at.elapsed().as_secs_f64() } else { h.map_or(now_secs(), |h| h.placed_unix) },
                    "requoting": j.state == JobState::Requoting || h.is_some_and(|h| h.requoting) || (j.state == JobState::Placing),
                    "paused": j.state == JobState::Paused || h.is_some_and(|h| h.paused),
                });
            }
            v
        }).collect();
        // fills whose claim has not gone through yet (held back, or interrupted by closing the app)
        let mut claims: Vec<Value> = vec![];
        for i in self.ice_deferred.iter().map(|d| d.0).chain(self.claiming.iter().copied()) {
            if let Some(t) = self.trades.get(i) { if self.jobs.iter().any(|j| j.product == t.id) { claims.push(json!({"product": t.id, "price": t.price})); } }
        }
        for c in &self.restore_claims { claims.push(json!({"product": c.0, "price": c.1})); }
        json!({"ver": 2, "next_id": self.next_job_id, "jobs": jobs, "claims": claims}).to_string()
    }
    pub fn ice_save(&mut self) {
        if self.mock || self.is_client() { return; }
        let text = self.ice_json(true);
        write_data("iceberg_jobs.json", text);
        self.ice_saved = (self.ice_json(false), Instant::now());
    }
    /// Called every tick: write the file whenever anything but the running clock changed (and every 30 s for the clock).
    pub fn ice_autosave(&mut self) {
        if self.mock || self.is_client() { return; }
        let now = self.ice_json(false);
        if now != self.ice_saved.0 || self.ice_saved.1.elapsed() > Duration::from_secs(30) { self.ice_save(); }
    }
    /// Jobs come back where they were: a live order is looked up in game and taken over (see ice_on_orders).
    pub fn ice_load() -> (Vec<IceJob>, u32, Vec<(String, f64)>) {
        let Some(v) = data_path("iceberg_jobs.json").and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { return (vec![], 1, vec![]) };
        Self::ice_parse(&v)
    }
    pub fn ice_parse(v: &Value) -> (Vec<IceJob>, u32, Vec<(String, f64)>) {
        let mut out = vec![];
        let legacy = v["ver"].as_u64().unwrap_or(1) < 2;   // written by a build that did not record the live order
        for j in v["jobs"].as_array().cloned().unwrap_or_default() {
            let (Some(id), Some(product), Some(total)) = (j["id"].as_u64(), j["product"].as_str(), j["total"].as_u64()) else { continue };
            let mut job = IceJob::new(id as u32, product.into(), total, j["slice"].as_u64().unwrap_or(MAX_ORDER_QTY), j["min"].as_f64().unwrap_or(0.1),
                j["max"].as_f64().unwrap_or(0.0), j["offset"].as_f64().unwrap_or(0.1), j["requote"].as_u64().unwrap_or(DEFAULT_REQUOTE_SECS));
            job.filled = j["filled"].as_u64().unwrap_or(0);
            job.requotes = j["requotes"].as_u64().unwrap_or(0) as u32;
            job.slices_done = j["slices_done"].as_u64().unwrap_or(0) as u32;
            job.run_secs = j["run_secs"].as_f64().unwrap_or(0.0);
            job.stopping = j["stopping"].as_bool().unwrap_or(false);
            job.auto_claim = j["auto_claim"].as_bool().unwrap_or(false);
            job.on_outbid = j["on_outbid"].as_bool().unwrap_or(false); job.outbid_gap = j["outbid_gap"].as_u64().unwrap_or(30);
            let was = j["state"].as_str().unwrap_or("");
            let paused = was == "Paused";
            job.state = match was { "Done" => JobState::Done, "Stopped" => JobState::Stopped, "Paused" => JobState::Paused, _ => JobState::Idle };
            if let Some(o) = j["order"].as_object() {
                if !matches!(job.state, JobState::Done | JobState::Stopped) {
                    job.adopt = Some(AdoptHint { qty: o.get("qty").and_then(|x| x.as_u64()).unwrap_or(0), price: o.get("price").and_then(|x| x.as_f64()).unwrap_or(0.0),
                        placed_unix: o.get("placed_unix").and_then(|x| x.as_f64()).unwrap_or_else(now_secs), requoting: o.get("requoting").and_then(|x| x.as_bool()).unwrap_or(false),
                        paused, tries: 0, sent_at: None });
                    job.state = JobState::Adopting;
                    job.note = "app reopened: finding this iceberg's order in game".into();
                }
            }
            if legacy && !matches!(job.state, JobState::Done | JobState::Stopped) {
                // nothing says whether an order is resting in game: do not risk a second one
                job.state = JobState::Paused;
                job.note = "restored from an older version: check Escrow for an order left resting in game, then Resume".into();
            }
            if job.remaining() == 0 { job.state = JobState::Done; job.adopt = None; }
            out.push(job);
        }
        let claims = v["claims"].as_array().map(|a| a.iter().filter_map(|c| Some((c["product"].as_str()?.to_string(), c["price"].as_f64()?))).collect()).unwrap_or_default();
        (out, v["next_id"].as_u64().unwrap_or(1).max(1) as u32, claims)
    }

    // ---------- pricing ----------
    /// The order this job would place right now, or why it has to wait.
    pub fn ice_quote(&self, j: &IceJob) -> Result<(f64, u64), String> {
        let p = self.products.get(&j.product).ok_or_else(|| "waiting for prices".to_string())?;
        // The bazaar allows 14 orders and a co-op shares them: each account keeps to its half, so two accounts never lock each other out.
        let mine = self.orders.len();
        let theirs = self.coop_orders.len();
        if theirs > 0 && mine >= COOP_ORDER_SHARE { return Err(format!("{mine} orders open: waiting for a free slot (the co-op shares the bazaar's 14-order limit, this account keeps to {COOP_ORDER_SHARE})")); }
        if mine + theirs >= 14 { return Err("the co-op is at the bazaar's 14-order limit: waiting for a free slot".into()); }
        let (bid, ask) = (p.bid(), p.ask());
        let mut price = ((bid + j.offset) * 10.0).round() / 10.0;
        if price > ask { price = ask; }
        if price < j.min_price { price = j.min_price; if price > ask + 1e-9 { return Err(format!("price floor {:.1} is above the ask {:.1}", j.min_price, ask)); } }
        if price > j.max_price + 1e-9 { return Err(format!("top bid {:.1} + {:.1} = {:.1} is above your max {:.1}: waiting", bid, j.offset, price, j.max_price)); }
        let mut q = j.remaining().min(j.slice).min(MAX_ORDER_QTY);
        if let Some(pu) = self.purse {
            let afford = (pu / price).floor().max(0.0) as u64;
            if afford == 0 { return Err("not enough coins in the purse".into()); }
            q = q.min(afford);
        }
        let by_cap = ((DAILY_SPEND_CAP - self.spent_today).max(0.0) / price).floor() as u64;
        if by_cap == 0 { return Err("daily spend cap reached".into()); }
        q = q.min(by_cap);
        if q == 0 { return Err("nothing left to buy".into()); }
        Ok((price, q))
    }

    fn act_secs_of(&self, k: &str) -> f64 { self.act_secs.get(k).copied().unwrap_or(match k { "place" => 5.0, "requote" => 10.0, "orders" => 4.0, "claim" => 30.0, _ => 5.0 }) }
    fn act_start(&mut self, k: &'static str) { self.act_t0.insert(k, Instant::now()); }
    fn act_done(&mut self, k: &'static str) {
        if let Some(t0) = self.act_t0.remove(k) {
            let dt = t0.elapsed().as_secs_f64();
            let e = self.act_secs.entry(k).or_insert(dt);
            *e = *e * 0.7 + dt * 0.3;
        }
    }

    // ---------- job control ----------
    pub fn ice_add(&mut self, mut job: IceJob) {
        if self.is_client() {
            let v = json!({"op": "ice_add", "product": job.product, "total": job.total, "slice": job.slice, "min": job.min_price, "max": job.max_price, "offset": job.offset, "requote": job.requote_secs, "auto_claim": job.auto_claim, "on_outbid": job.on_outbid, "outbid_gap": job.outbid_gap});
            if self.remote_op(v) { let n = pretty_s(&job.product); let _ = self.toast_new(1, format!("Iceberg sent to the server: {} {n}", grouped(job.total))); }
            return;
        }
        job.id = self.next_job_id; self.next_job_id += 1;
        self.say(ORANGE, format!("→ iceberg #{}: BUY {} {} (max {:.1}, requote every {})", job.id, grouped(job.total), pretty_s(&job.product), job.max_price, fmt_dur(job.requote_secs as f64)));
        self.jobs.push(job);
        self.ice_save();
    }
    pub fn ice_pause(&mut self, i: usize) {
        if self.is_client() { if let Some(id) = self.jobs.get(i).map(|j| j.id) { self.remote_op(json!({"op": "ice", "do": "pause", "id": id})); } return; }
        let Some(j) = self.jobs.get_mut(i) else { return };
        if matches!(j.state, JobState::Idle | JobState::Placing | JobState::Resting) { j.state = JobState::Paused; j.note = if j.has_live_order() { "paused: the order stays resting in game".into() } else { "paused".into() }; }
        self.ice_save();
    }
    /// A job that paused itself after repeated errors tries again by itself: after 2 minutes, at most 3 times in a row.
    pub fn ice_auto_resume_tick(&mut self) {
        if self.is_client() || !self.agent_up || self.halted { return; }
        for i in 0..self.jobs.len() {
            let j = &self.jobs[i];
            if j.state == JobState::Paused && j.note.starts_with("paused after repeated errors") && j.auto_resumes < 3 && j.state_at.elapsed() > Duration::from_secs(120) {
                let id = j.id;
                self.jobs[i].auto_resumes += 1; self.jobs[i].fails = 0;
                self.say(ORANGE, format!("iceberg #{id}: resuming by itself after the errors (try {} of 3)", self.jobs[i].auto_resumes));
                self.ice_resume(i);
            }
        }
    }
    pub fn ice_resume(&mut self, i: usize) {
        if self.is_client() { if let Some(id) = self.jobs.get(i).map(|j| j.id) { self.remote_op(json!({"op": "ice", "do": "resume", "id": id})); } return; }
        let Some(j) = self.jobs.get_mut(i) else { return };
        if j.state != JobState::Paused { return; }
        j.state = if j.cur_trade.is_some() { JobState::Placing } else { JobState::Idle };
        j.state_at = Instant::now(); j.note.clear(); j.retry_at = None;
        self.ice_save();
    }
    /// Changes the settings of a job that is still running (item stays). The resting order keeps its old price until the next requote;
    /// `requote_now` makes that happen at once. Runs on the server; a client forwards it.
    pub fn ice_edit(&mut self, e: &IceEdit) -> Result<(), String> {
        if self.is_client() {
            self.remote_op(json!({"op": "ice_edit", "id": e.id, "total": e.total, "slice": e.slice, "min": e.min, "max": e.max, "offset": e.offset, "requote": e.requote_secs,
                "auto_claim": e.auto_claim, "on_outbid": e.on_outbid, "outbid_gap": e.outbid_gap, "now": e.requote_now}));
            return Ok(());
        }
        let Some(j) = self.jobs.iter_mut().find(|j| j.id == e.id) else { return Err("that iceberg no longer exists".into()) };
        if matches!(j.state, JobState::Done | JobState::Stopped) { return Err("a finished iceberg cannot be edited".into()); }
        if e.total == 0 || e.slice == 0 || e.max <= 0.0 || e.max < e.min || e.requote_secs < 30 { return Err("invalid values".into()); }
        if e.total < j.filled + j.cur_filled { return Err(format!("total is below what is already bought ({})", grouped(j.filled + j.cur_filled))); }
        j.total = e.total; j.slice = e.slice.min(MAX_ORDER_QTY); j.min_price = e.min; j.max_price = e.max; j.offset = e.offset; j.requote_secs = e.requote_secs;
        j.auto_claim = e.auto_claim; j.on_outbid = e.on_outbid; j.outbid_gap = e.outbid_gap;
        j.note = "settings changed".into();
        if e.requote_now && j.cur_trade.is_some() && j.state == JobState::Resting { j.placed_at = ago(j.requote_secs); j.note = "settings changed: requoting now".into(); }
        let id = j.id;
        self.say(ORANGE, format!("iceberg #{id}: settings changed"));
        self.ice_save();
        Ok(())
    }
    /// Stop: with an order resting, read the menu and cancel it first (keeping any partial fill).
    pub fn ice_stop(&mut self, i: usize) {
        if self.is_client() { if let Some(id) = self.jobs.get(i).map(|j| j.id) { self.remote_op(json!({"op": "ice", "do": "stop", "id": id})); } return; }
        let Some(j) = self.jobs.get_mut(i) else { return };
        match j.state {
            JobState::Done | JobState::Stopped | JobState::Requoting => {}
            JobState::Adopting => { j.stopping = true; j.note = "will stop as soon as its order is found".into(); }
            _ if j.cur_trade.is_some() => {
                j.stopping = true; j.note = "stopping: cancelling the resting order".into();
                if j.state == JobState::Paused { j.state = JobState::Placing; }
                j.placed_at = ago(j.requote_secs);   // check it now
            }
            _ => { j.state = JobState::Stopped; j.note = "stopped".into(); }
        }
        self.ice_save();
    }
    pub fn ice_remove(&mut self, i: usize) {
        if self.is_client() { if let Some(id) = self.jobs.get(i).map(|j| j.id) { self.remote_op(json!({"op": "ice", "do": "remove", "id": id})); } return; }
        if let Some(j) = self.jobs.get(i) { if j.has_live_order() && j.state.live() { return; } }
        if i < self.jobs.len() { self.jobs.remove(i); self.ice_save(); }
    }

    // ---------- the engine ----------
    /// Write every change of an iceberg's state to the journal, with the reason (the job's note) when it is paused or stopped,
    /// so "why did it pause?" can always be answered afterwards (journal: ~/.local/share/blockberg/journal.jsonl, ev "ice").
    fn ice_journal_states(&mut self) {
        let live: std::collections::HashSet<u32> = self.jobs.iter().map(|j| j.id).collect();
        self.ice_logged.retain(|id, _| live.contains(id));
        let (agent_up, inflight) = (self.agent_up, self.inflight.len());
        for j in &self.jobs {
            let label = j.state.label().to_string();
            let key = (label.clone(), if matches!(j.state, JobState::Paused | JobState::Stopped) { j.note.clone() } else { String::new() });
            if self.ice_logged.get(&j.id) == Some(&key) { continue; }
            let first = !self.ice_logged.contains_key(&j.id);
            self.ice_logged.insert(j.id, key);
            server::journal_event(json!({"ev": "ice", "id": j.id, "product": j.product, "state": label, "first": first, "note": j.note, "filled": j.filled, "total": j.total,
                "fails": j.fails, "cur_qty": j.cur_qty, "cur_price": j.cur_price, "agent_up": agent_up, "inflight": inflight, "halted": self.halted, "draining": self.draining}));
        }
    }

    /// The top bid, skipping levels that are only our own other open buy orders of this item (a leftover slice must never make us
    /// "outbid" ourselves and walk the price up).
    pub fn bid_without_own(&self, product: &str) -> Option<f64> {
        let p = self.products.get(product)?;
        let own: Vec<(f64, u64)> = self.orders.iter().filter(|r| r.side == "BUY" && same_product(&r.product, product) && !(r.total > 0 && r.filled >= r.total))
            .filter_map(|r| Some((r.price?, r.total.saturating_sub(r.filled)))).collect();
        p.sell_summary.iter().find(|l| {
            let mine: u64 = own.iter().filter(|(pr, _)| (pr - l.price).abs() < 0.05).map(|(_, q)| *q).sum();
            mine == 0 || l.amount > mine
        }).map(|l| l.price)
    }

    /// The highest open buy order of a co-op partner for this item (the order list is shared, so we see theirs).
    pub fn coop_top_bid(&self, product: &str) -> Option<f64> {
        self.coop_orders.iter().filter(|r| r.side == "BUY" && same_product(&r.product, product) && !(r.total > 0 && r.filled >= r.total))
            .filter_map(|r| r.price).fold(None, |m: Option<f64>, p| Some(m.map_or(p, |x| x.max(p))))
    }
    /// When the top bid of the book is a co-op partner's, the price to join it at (never above `max`): bidding 0.1 over our own partner only raises what we both pay.
    pub fn coop_join(&self, product: &str, max: f64) -> Option<f64> {
        let top = self.products.get(product).map_or(0.0, |p| p.bid());
        self.coop_top_bid(product).filter(|c| top > 0.0 && *c >= top - 0.05 && *c <= max + 1e-9)
    }

    pub fn ice_tick(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.ice_last_tick).as_secs_f64().min(2.0);
        self.ice_last_tick = now;
        for i in 0..self.jobs.len() {
            if self.jobs[i].state.live() { self.jobs[i].run_secs += dt; }
            match self.jobs[i].state {
                JobState::Idle => self.ice_idle(i, now),
                JobState::Placing => self.ice_placing(i, now),
                JobState::Resting => self.ice_resting(i, now),
                JobState::Adopting => self.ice_adopting(i, now),
                JobState::Requoting => {
                    if now.duration_since(self.jobs[i].state_at) > Duration::from_secs(STEP_TIMEOUT) {
                        let j = &mut self.jobs[i];
                        j.state = JobState::Resting; j.note = "no answer from the agent: will look again shortly".into();
                        j.placed_at = ago(j.requote_secs.saturating_sub(20));
                    }
                }
                _ => {}
            }
        }
        // Watchdog: an action the agent never answered must not block every later action forever.
        if self.inflight.is_empty() { self.inflight_since = None; }
        else {
            let t0 = *self.inflight_since.get_or_insert_with(Instant::now);
            if t0.elapsed() > Duration::from_secs(300) {
                let n = self.inflight.len();
                while let Some(ti) = self.inflight.pop_front() {
                    if self.trades[ti].status == Status::Working { self.trades[ti].status = Status::Rejected("no answer from the agent".into()); }
                    // An iceberg slice whose reply never came may well be resting in the bazaar. Placing another one blindly
                    // could double the order, so pause that job and let the user look at Escrow first.
                    for j in self.jobs.iter_mut().filter(|j| j.cur_trade == Some(ti) && j.state == JobState::Placing) {
                        j.cur_trade = None; j.state = JobState::Paused;
                        j.note = "no answer from the agent: check Escrow for a resting order, then press Resume".into();
                    }
                }
                self.inflight_since = None;
                self.say(RED, format!("⚠ {n} action(s) got no answer from the agent for 5 min: dropped so the queue can move on"));
                server::journal_event(json!({"ev": "dropped", "why": "no answer from the agent for 5 min", "count": n, "agent_up": self.agent_up}));
            }
        }
        // finished icebergs leave the queue on their own after a few seconds (so "complete" can still be seen)
        if self.ice_auto_remove {
            let waiting: Vec<String> = self.ice_deferred.iter().map(|d| d.0).chain(self.claiming.iter().copied()).filter_map(|i| self.trades.get(i).map(|t| t.id.clone())).collect();
            let gone: Vec<usize> = (0..self.jobs.len()).rev().filter(|&k| {
                let j = &self.jobs[k];
                j.state == JobState::Done && j.cur_trade.is_none() && now.duration_since(j.state_at) > Duration::from_secs(8) && !waiting.contains(&j.product)
            }).collect();
            for k in gone { let id = self.jobs[k].id; self.jobs.remove(k); self.say(DIM, format!("iceberg #{id} finished: removed from the queue")); }
        }
        self.ice_journal_states();
        self.act_prune();
        self.ice_claim_fallback();
        self.ice_restore_claims();
        self.ice_autosave();
    }

    /// After a restart: ask the order menu where this job's order is. The answer is handled in ice_on_orders.
    fn ice_adopting(&mut self, i: usize, now: Instant) {
        if self.draining { return; }
        if !self.agent_up || self.halted || !self.inflight.is_empty() { return; }
        let Some(h) = self.jobs[i].adopt.as_mut() else { self.jobs[i].state = JobState::Idle; return };
        if h.sent_at.is_some_and(|t| now.duration_since(t) < Duration::from_secs(45)) { return; }
        if h.tries >= 3 {
            let j = &mut self.jobs[i];
            j.adopt = None; j.state = JobState::Paused; j.note = "could not read the order menu after reopening: check Escrow, then Resume".into();
            return;
        }
        h.tries += 1; h.sent_at = Some(now);
        let _ = self.cmd_tx.send(json!({"cmd": "orders", "auto": true}));
        self.last_orders_check = now;
    }

    /// Claims that were waiting when the app was closed go out as soon as the agent is back.
    fn ice_restore_claims(&mut self) {
        if self.draining { return; }
        if self.restore_claims.is_empty() || !self.agent_up || self.halted || !self.inflight.is_empty() { return; }
        if self.jobs.iter().any(|j| j.state == JobState::Adopting) { return; }   // the new order first
        for (product, price) in std::mem::take(&mut self.restore_claims) {
            let _ = self.cmd_tx.send(json!({"cmd": "claimorder", "auto": true, "product": product, "side": "BUY", "price": price, "sack_on_full": self.auto_sack}));
            self.say(ORANGE, format!("→ claim {} (left over from before the app closed)", pretty_s(&product)));
        }
    }

    fn ice_send_claim(&mut self, i: usize) {
        if !self.claiming.insert(i) { return; }
        let t = self.trades[i].clone();
        let _ = self.cmd_tx.send(json!({"cmd": "claimorder", "auto": true, "product": t.id, "side": t.side, "price": t.price, "sack_on_full": self.auto_sack}));
        self.say(ORANGE, format!("→ claim {} {}", t.side, pretty_s(&t.id)));
    }

    /// Send the claims that were held back so the next order could go first. `only`: just that product.
    pub fn ice_flush_claims(&mut self, only: Option<&str>) {
        if self.draining { return; }
        let ready: Vec<usize> = self.ice_deferred.iter().map(|d| d.0).filter(|&i| only.map_or(true, |p| self.trades[i].id == p)).collect();
        self.ice_deferred.retain(|d| !ready.contains(&d.0));
        for i in ready { self.ice_send_claim(i); }
    }

    /// A held-back claim must never wait forever: send it when no order can follow, or after 60 s.
    fn ice_claim_fallback(&mut self) {
        if self.draining { return; }
        let due: Vec<usize> = self.ice_deferred.iter().filter(|(i, at)| {
            let id = &self.trades[*i].id;
            at.elapsed() > Duration::from_secs(60) || !self.jobs.iter().any(|j| &j.product == id && j.remaining() > 0 && matches!(j.state, JobState::Idle | JobState::Placing | JobState::Requoting | JobState::Resting))
        }).map(|d| d.0).collect();
        self.ice_deferred.retain(|d| !due.contains(&d.0));
        for i in due { self.ice_send_claim(i); }
    }

    fn ice_finish_slot(&mut self, i: usize) {
        let j = &mut self.jobs[i];
        j.cur_trade = None; j.cur_filled = 0;
        j.state = if j.stopping { JobState::Stopped } else { JobState::Idle };
        j.state_at = Instant::now();
        if j.stopping { j.note = "stopped".into(); }
    }

    fn ice_idle(&mut self, i: usize, now: Instant) {
        if self.jobs[i].remaining() == 0 {
            let j = &mut self.jobs[i]; j.state = JobState::Done; j.note = "complete".into(); j.state_at = Instant::now();
            let msg = format!("✔ iceberg #{} complete: bought {} {}", j.id, grouped(j.filled), pretty_s(&j.product));
            self.say(GREEN, msg);
            self.ice_save();
            return;
        }
        if self.jobs[i].retry_at.is_some_and(|t| now < t) { return; }
        if self.draining { self.jobs[i].note = "the server is restarting: the next order is placed after the restart".into(); return; }
        if !self.agent_up { self.jobs[i].note = "waiting for the agent".into(); return; }
        if self.halted { self.jobs[i].note = "trading halted".into(); return; }
        if !self.inflight.is_empty() { self.jobs[i].note = "waiting for the agent to finish another action".into(); return; }
        let job = self.jobs[i].clone();
        match self.ice_quote(&job) {
            Err(m) => self.jobs[i].note = m,
            Ok((price, qty)) => {
                // Hypixel's own "Top Order +0.1" button sets the price (the agent refuses to go above max_price);
                // check:false skips the order-menu read after placing, fills are caught from chat and the 30 s poll.
                let join = self.coop_join(&job.product, job.max_price);
                let _ = if let Some(c) = join { self.cmd_tx.send(json!({"cmd": "buyorder", "product": job.product, "qty": qty, "price": c, "top_plus": false, "max_price": job.max_price, "check": false})) }
                    else { self.cmd_tx.send(json!({"cmd": "buyorder", "product": job.product, "qty": qty, "price": price, "top_plus": true, "max_price": job.max_price, "check": false})) };
                self.trades.push(Trade { time: clock(), side: "BUY", id: job.product.clone(), qty, price, kind: Kind::Limit, status: Status::Working, sig: Sig::default() });
                self.inflight.push_back(self.trades.len() - 1);
                self.say(ORANGE, format!("→ BUY {} x{} @ top bid +{:.1} (≈{})", pretty_s(&job.product), qty, job.offset, fmt_coins(price)));
                self.act_start("place");
                let ti = self.trades.len() - 1;
                let j = &mut self.jobs[i];
                j.cur_trade = Some(ti); j.cur_qty = qty; j.cur_price = price; j.cur_filled = 0;
                j.state = JobState::Placing; j.state_at = now; j.retry_at = None;
                j.note = format!("placing {} @ {:.1}", grouped(qty), price);
            }
        }
    }

    fn ice_placing(&mut self, i: usize, now: Instant) {
        let Some(ti) = self.jobs[i].cur_trade else { self.jobs[i].state = JobState::Idle; return };
        match self.trades[ti].status.clone() {
            Status::Working => if now.duration_since(self.jobs[i].state_at) > Duration::from_secs(180) { self.jobs[i].note = "the agent has not answered yet".into(); },
            Status::Resting => {
                self.act_done("place");
                let pid = self.trades[ti].id.clone();
                self.ice_flush_claims(Some(&pid));
                let real = self.trades[ti].price;
                let j = &mut self.jobs[i];
                j.cur_price = real;
                j.state = JobState::Resting; j.placed_at = now; j.state_at = now; j.fails = 0;
                j.note = format!("resting {} @ {:.1}", grouped(j.cur_qty), j.cur_price);
            }
            Status::Filled => self.ice_book_fill(i),
            Status::Rejected(m) => {
                let id = self.jobs[i].id;
                let j = &mut self.jobs[i];
                j.cur_trade = None;
                j.note = format!("order rejected: {m}");
                if m.contains("order limit") || m.contains("maximum of") {
                    // the bazaar's 14-order limit (shared by the whole co-op): wait for claims and cancels to free a slot, never give up over it
                    j.state = JobState::Idle; j.retry_at = Some(now + Duration::from_secs(45));
                    j.note = "waiting for a free order slot (the co-op is at the bazaar's 14-order limit)".into();
                    return;
                }
                if m.contains("above your max") {
                    // price is simply too high right now: wait and look again, never give up over it
                    j.state = JobState::Idle; j.retry_at = Some(now + Duration::from_secs(20));
                    j.note = format!("waiting: {m}");
                    return;
                }
                j.fails += 1;
                if j.fails >= 3 {
                    j.state = JobState::Stopped;
                    self.say(RED, format!("✗ iceberg #{id} stopped after 3 rejected orders: {m}"));
                } else {
                    j.state = JobState::Idle; j.retry_at = Some(now + Duration::from_secs(RETRY_AFTER_REJECT * j.fails as u64));
                }
                self.ice_save();
            }
        }
    }

    fn ice_resting(&mut self, i: usize, now: Instant) {
        let Some(ti) = self.jobs[i].cur_trade else { self.jobs[i].state = JobState::Idle; return };
        match self.trades[ti].status.clone() {
            Status::Filled => { self.ice_book_fill(i); return; }
            Status::Rejected(_) => { self.ice_finish_slot(i); return; }
            _ => {}
        }
        let j = &self.jobs[i];
        // "requote when outbid": someone's bid is above ours, we have rested at least the minimum gap, and the new price stays inside the band
        let outbid = j.on_outbid && j.state == JobState::Resting && j.cur_price > 0.0 && now.duration_since(j.placed_at) >= Duration::from_secs(j.outbid_gap.min(OUTBID_MAX_GAP))
            && self.bid_without_own(&j.product).map_or(false, |top| top > j.cur_price + 0.05 && top + j.offset <= j.max_price + 1e-9)
            // the order above ours is our co-op partner's and we already sit at its price: not outbid
            && !self.coop_top_bid(&j.product).map_or(false, |c| { let top = self.products.get(&j.product).map_or(0.0, |p| p.bid()); c >= top - 0.05 && j.cur_price >= c - 0.05 });
        let due = j.stopping || outbid || now.duration_since(j.placed_at) >= Duration::from_secs(j.requote_secs);
        if due && self.agent_up && !self.halted && self.inflight.is_empty() && !self.draining {
            // One agent action: open the order menu once, cancel what is left, place the next order at top bid +0.1.
            let remaining = if j.stopping { 0 } else { j.remaining() };
            let refund = j.cur_qty as f64 * j.cur_price;
            let mut max_qty = MAX_ORDER_QTY;
            if let Some(pu) = self.purse { max_qty = max_qty.min(((pu + refund) / j.max_price.max(0.1)).floor() as u64); }
            max_qty = max_qty.min(((DAILY_SPEND_CAP - self.spent_today).max(0.0) / j.max_price.max(0.1)).floor() as u64);
            // Looking for lower orders of ours to cancel takes the agent another 5 to 10 s after every requote: only ask for it when the order list shows one.
            let lower = self.orders.iter().any(|r| r.side == "BUY" && same_product(&r.product, &j.product) && r.price.map_or(false, |p| p < j.cur_price - 0.05) && !(r.total > 0 && r.filled >= r.total));
            let _ = self.cmd_tx.send(json!({"cmd": "requote", "product": j.product, "price": j.cur_price, "slice": j.slice, "remaining": remaining, "max_qty": max_qty, "max_price": j.max_price, "cancel_below": lower, "join": self.coop_join(&j.product, j.max_price)}));
            self.act_start("requote");
            let j = &mut self.jobs[i];
            j.state = JobState::Requoting; j.state_at = now; j.note = "requoting: cancel what is left and place at the new top bid".into();
        }
    }

    fn ice_book_fill(&mut self, i: usize) {
        let Some(ti) = self.jobs[i].cur_trade else { return };
        let q = self.trades[ti].qty;
        let j = &mut self.jobs[i];
        j.filled += q; j.slices_done += 1; j.fails = 0;
        let msg = format!("✔ iceberg #{}: slice of {} filled ({} / {})", j.id, grouped(q), grouped(j.filled.min(j.total)), grouped(j.total));
        self.say(GREEN, msg);
        self.ice_finish_slot(i);
        self.ice_save();
    }

    /// The order menu came back (periodic poll): keep each resting slice's fill progress up to date.
    pub fn ice_on_orders(&mut self) {
        self.ice_adopt_from_orders();
        for i in 0..self.jobs.len() {
            if self.jobs[i].state != JobState::Resting || self.jobs[i].cur_trade.is_none() { continue; }
            let (product, price) = (self.jobs[i].product.clone(), self.jobs[i].cur_price);
            // unclaimed, fully filled slices can sit next to the live order at the same price: follow the one still open
            let mut cands: Vec<&OrderRow> = self.orders.iter().filter(|r| r.side == "BUY" && same_product(&r.product, &product) && r.price.map_or(true, |p| (p - price).abs() < 0.05)).collect();
            cands.sort_by_key(|r| r.total > 0 && r.filled >= r.total);
            let row = cands.first().map(|r| (*r).clone());
            if let Some(r) = row { self.jobs[i].cur_filled = r.filled; }
        }
    }

    /// The agent connection dropped (a network blip, a Hypixel hop): whatever the agent was doing is gone and its answers will never come.
    /// An iceberg that was placing or re-quoting an order does not wait five minutes and then ask for a look at Escrow: it finds its order again in the game
    /// (the same way it does after the app was closed), and an auto flip drops the step it was waiting on so it looks at the fresh orders at once.
    pub fn ice_agent_lost(&mut self) {
        if self.is_client() { return; }
        while let Some(ti) = self.inflight.pop_front() {
            if self.trades[ti].status == Status::Working { self.trades[ti].status = Status::Rejected("agent connection lost".into()); }
        }
        self.inflight_since = None;
        let mut said = vec![];
        for j in self.jobs.iter_mut() {
            if matches!(j.state, JobState::Placing | JobState::Requoting) {
                j.adopt = Some(AdoptHint { qty: j.cur_qty, price: j.cur_price, placed_unix: now_secs(), requoting: true, paused: false, tries: 0, sent_at: None });
                j.cur_trade = None; j.state = JobState::Adopting;
                j.note = "agent connection lost: finding this iceberg's order in game".into();
                said.push(j.id);
            }
        }
        for f in self.flips.iter_mut() { if f.pending.is_some() { f.pending = None; f.note = "agent connection lost: looking at the orders again".into(); } }
        for id in said { self.say(ORANGE, format!("iceberg #{id}: the agent connection dropped mid-order: finding its order again once it is back")); }
        self.ice_autosave();
    }

    /// Match every reconnecting job with its order in the menu and carry on from there.
    fn ice_adopt_from_orders(&mut self) {
        let now = Instant::now();
        for i in 0..self.jobs.len() {
            if self.jobs[i].state != JobState::Adopting { continue; }
            let Some(h) = self.jobs[i].adopt.clone() else { continue };
            let product = self.jobs[i].product.clone();
            let rows: Vec<OrderRow> = self.orders.iter().filter(|r| r.side == "BUY" && same_product(&r.product, &product)).cloned().collect();
            let near = |r: &OrderRow| r.price.map_or(true, |p| (p - h.price).abs() < 0.05);
            let open = |r: &OrderRow| !(r.total > 0 && r.filled >= r.total);
            // an order whose price we know, or (when a requote was in flight) the only open one there is
            let pick = rows.iter().filter(|r| near(r)).min_by_key(|r| !open(r)).cloned()
                .or_else(|| if h.requoting { let o: Vec<&OrderRow> = rows.iter().filter(|r| open(r)).collect(); if o.len() == 1 { Some(o[0].clone()) } else { None } } else { None });
            self.jobs[i].adopt = None;
            match pick {
                Some(r) => {
                    let qty = if r.total > 0 { r.total } else { h.qty };
                    let price = r.price.unwrap_or(h.price);
                    self.trades.push(Trade { time: clock(), side: "BUY", id: product.clone(), qty, price, kind: Kind::Limit, status: Status::Resting, sig: Sig::default() });
                    let ti = self.trades.len() - 1;
                    let age = (now_secs() - h.placed_unix).max(0.0);
                    let paused = h.paused && !self.jobs[i].stopping;
                    let id = self.jobs[i].id;
                    {
                        let j = &mut self.jobs[i];
                        j.cur_trade = Some(ti); j.cur_qty = qty; j.cur_price = price; j.cur_filled = r.filled;
                        j.placed_at = now.checked_sub(Duration::from_secs_f64(age)).unwrap_or(now);
                        j.state = if paused { JobState::Paused } else { JobState::Resting };
                        j.note = format!("took over the order from before: {} / {} filled @ {:.1}", grouped(r.filled), grouped(qty), price);
                    }
                    self.say(GREEN, format!("↺ iceberg #{id}: found its order again ({} / {} filled @ {:.1})", grouped(r.filled), grouped(qty), price));
                    if !open(&r) {
                        // it completed while the app was closed: book it and claim it like any other fill
                        self.trades[ti].sig.orders = true; self.apply_fill(ti, None); self.claim_trade(ti);
                        if paused { self.jobs[i].state = JobState::Resting; }
                    }
                }
                None => {
                    let id = self.jobs[i].id;
                    let j = &mut self.jobs[i];
                    j.state = if j.stopping { j.stopping = false; JobState::Stopped } else if h.paused { JobState::Paused } else { JobState::Idle };
                    j.note = "the order from before was not in the menu (cancelled or claimed by hand). Fills while the app was closed are not counted.".into();
                    self.say(ORANGE, format!("iceberg #{id}: its previous order is gone; carrying on from {} bought", grouped(self.jobs[i].filled)));
                }
            }
        }
        self.ice_save();
    }

    /// The agent's answer to "requote": the old order was filled / cancelled, and maybe a fresh one is resting.
    pub fn ice_on_requoted(&mut self, v: &Value) {
        let now = Instant::now();
        let pid = v["product"].as_str().unwrap_or("");
        // a late answer (the app already gave up waiting and went back to Resting) still counts
        let Some(i) = self.jobs.iter().position(|j| j.state == JobState::Requoting && j.product == pid)
            .or_else(|| self.jobs.iter().position(|j| j.state == JobState::Resting && j.fails > 0 && j.product == pid && j.cur_trade.is_some())) else { return };
        self.act_done("requote");
        let f = v["filled"].as_u64().unwrap_or(0);
        let ti = self.jobs[i].cur_trade;
        if v["result"].as_str() == Some("filled") {
            // it filled completely before we cancelled: book it through the normal path (which also claims)
            if let Some(ti) = ti { self.trades[ti].sig.orders = true; self.apply_fill(ti, None); self.claim_trade(ti); }
            self.jobs[i].state = JobState::Resting;
            return;
        }
        if let Some(ti) = ti {
            if f > 0 { self.trades[ti].qty = f; self.apply_fill(ti, None); self.trades[ti].sig.chat = true; }
            else { self.trades[ti].status = Status::Rejected("cancelled for requote".into()); }
        }
        let placed = v["placed"].as_str().is_some() && v["qty"].as_u64().unwrap_or(0) > 0;
        let (qty, price) = (v["qty"].as_u64().unwrap_or(0), v["price"].as_f64().unwrap_or(self.jobs[i].cur_price));
        {
            let j = &mut self.jobs[i];
            j.filled += f; j.requotes += 1; j.auto_resumes = 0;
            if f > 0 { j.slices_done += 1; }
        }
        let cb = v["cancelled_below"].as_u64().unwrap_or(0);
        let line = format!("↻ iceberg #{}: cancelled, {} kept{}{}", self.jobs[i].id, grouped(f), if placed { format!("; new order {} @ {:.1}", grouped(qty), price) } else { String::new() }, if cb > 0 { format!("; also cancelled {cb} lower order(s)") } else { String::new() });
        self.say(GREEN, line);
        if placed {
            let id = self.jobs[i].product.clone();
            self.ice_flush_claims(Some(&id));
            self.trades.push(Trade { time: clock(), side: "BUY", id, qty, price, kind: Kind::Limit, status: Status::Resting, sig: Sig::default() });
            let nt = self.trades.len() - 1;
            let j = &mut self.jobs[i];
            j.cur_trade = Some(nt); j.cur_qty = qty; j.cur_price = price; j.cur_filled = 0;
            j.state = JobState::Resting; j.placed_at = now; j.state_at = now; j.fails = 0;
            j.note = format!("resting {} @ {:.1}", grouped(qty), price);
        } else {
            self.ice_finish_slot(i);
            if let Some(e) = v["place_error"].as_str() { self.jobs[i].note = format!("cancelled, but the new order failed: {e}"); self.say(RED, format!("✗ iceberg #{}: {e}", self.jobs[i].id)); }
        }
        self.ice_save();
    }

    pub fn ice_on_error(&mut self, msg: &str, cmd: Option<&str>) {
        let now = Instant::now();
        if msg.contains("unknown cmd") && msg.contains("invall") { self.inv_unsupported = true; }
        if self.sacks_scanning { self.sacks_scanning = false; }
        for j in self.jobs.iter_mut() {
            // only a failed requote (or an error that names no command, from an old agent) may fail a requote in progress
            if j.state == JobState::Requoting && cmd.map_or(true, |c| c == "requote") && msg.starts_with("no open BUY order") {
                // A job that is being stopped has nothing left to cancel: it is stopped. (Going back to Resting made it ask for another cancel at once, which failed the
                // same way, again and again: a stop that never finished.)
                if j.stopping { j.state = JobState::Stopped; j.cur_trade = None; j.note = "stopped (its order was no longer open)".into(); j.stopping = false; continue; }
                // the order is gone (filled, or the earlier requote already cancelled it): not a failure. The regular look at the
                // order menu finds out which and moves the job on.
                j.note = "its order was not in the menu: checking the escrow".into();
                j.state = JobState::Resting; j.state_at = now; j.placed_at = now;
                self.last_orders_check = Instant::now() - Duration::from_secs(3600);
                continue;
            }
            if j.state == JobState::Requoting && cmd.map_or(true, |c| c == "requote") && msg.contains("Cancel Order") {
                // the order kept handing over fills instead of opening its cancel menu: not broken, just try again in a moment (not counted towards pausing)
                j.note = "its order was still handing over fills: requoting again shortly".into();
                j.state = JobState::Resting; j.state_at = now; j.placed_at = ago(j.requote_secs.saturating_sub(10));
                continue;
            }
            if j.state == JobState::Requoting && cmd.map_or(true, |c| c == "requote") {
                j.fails += 1;
                j.note = format!("{msg}: looking again shortly");
                j.state = JobState::Resting; j.state_at = now;
                j.placed_at = ago(j.requote_secs.saturating_sub(20));
                if j.fails >= 5 { j.state = JobState::Paused; j.note = format!("paused after repeated errors: {msg}"); }
            }
        }
    }

    /// Seconds until this job's order has been resting long enough to be requoted.
    pub fn ice_requote_in(&self, j: &IceJob) -> Option<f64> {
        if j.state != JobState::Resting { return None; }
        Some((j.requote_secs as f64 - j.placed_at.elapsed().as_secs_f64()).max(0.0))
    }
    pub fn ice_eta(&self, j: &IceJob) -> Option<f64> {
        let done = (j.filled + j.cur_filled) as f64;
        if done <= 0.0 || j.run_secs < 60.0 { return None; }
        let left = j.total.saturating_sub(j.filled + j.cur_filled) as f64;
        Some(left / (done / j.run_secs))
    }

    // ---------- queue rows ----------
    pub fn queue_rows(&self) -> Vec<QRow> {
        if self.is_client() { return self.remote_queue.clone(); }
        let mut rows = vec![];
        if self.restart_req {
            rows.push(QRow { depth: 0, tag: "RSTRT", text: "Restart the server".into(), detail: format!("waits for {} running action(s) to finish; nothing new starts meanwhile, then it comes straight back and carries on", self.restart_waiting()), color: ORANGE, when: None, est: None });
        }
        let blue = Color32::from_rgb(90, 170, 255);
        for j in &self.jobs {
            let pct = (j.filled + j.cur_filled) as f64 / j.total.max(1) as f64 * 100.0;
            let (col, tag) = match j.state { JobState::Done => (GREEN, "DONE"), JobState::Stopped => (DIM, "STOP"), JobState::Paused => (DIM, "PAUSE"), _ => (ORANGE, "ICE") };
            let eta = if j.state.live() { self.ice_eta(j) } else { None };
            rows.push(QRow { depth: 0, tag, text: format!("#{} BUY {} {}   {} / {}  ({:.1}%)", j.id, grouped(j.total), pretty_s(&j.product), grouped((j.filled + j.cur_filled).min(j.total)), grouped(j.total), pct),
                detail: format!("{} · band {:.1}–{:.1} · top bid +{:.1} · requote {} · slice ≤ {}", j.state.label(), j.min_price, j.max_price, j.offset, fmt_dur(j.requote_secs as f64), grouped(j.slice)),
                color: col, when: None, est: eta });
            let place_est = self.act_secs_of("place");
            match j.state {
                JobState::Idle => {
                    let (text, wait) = match self.ice_quote(j) {
                        Ok((p, q)) => (format!("Place BUY {} @ {:.1}  (top bid +{:.1})", grouped(q), p, j.offset), j.retry_at.map(|t| t.saturating_duration_since(Instant::now()).as_secs_f64()).or(Some(0.0))),
                        Err(m) => (format!("Waiting: {m}"), None),
                    };
                    rows.push(QRow { depth: 1, tag: "NEXT", text, detail: j.note.clone(), color: ORANGE, when: wait, est: Some(place_est) });
                }
                JobState::Placing => rows.push(QRow { depth: 1, tag: "NOW", text: format!("Placing BUY {} @ {:.1} in game", grouped(j.cur_qty), j.cur_price), detail: j.note.clone(), color: ORANGE, when: Some(0.0), est: Some(place_est) }),
                JobState::Resting => {
                    rows.push(QRow { depth: 1, tag: "WAIT", text: format!("Order resting: {} / {} filled @ {:.1}", grouped(j.cur_filled), grouped(j.cur_qty), j.cur_price), detail: "if it fills completely the next slice goes out immediately".into(), color: blue, when: None, est: None });
                    rows.push(QRow { depth: 1, tag: "REQ", text: "Requote: cancel what is left and place again at top bid +0.1 (one action)".into(), detail: String::new(), color: ORANGE,
                        when: self.ice_requote_in(j), est: Some(self.act_secs_of("requote")) });
                }
                JobState::Requoting => rows.push(QRow { depth: 1, tag: "NOW", text: "Requoting: cancelling the unfilled part and placing at top bid +0.1".into(), detail: j.note.clone(), color: ORANGE, when: Some(0.0), est: Some(self.act_secs_of("requote")) }),
                JobState::Paused => rows.push(QRow { depth: 1, tag: "HOLD", text: "Paused: nothing will happen until you press Resume".into(), detail: j.note.clone(), color: DIM, when: None, est: None }),
                _ => {}
            }
            if j.state.live() {
                let live = if j.cur_trade.is_some() { j.cur_qty } else { 0 };
                let left_after = j.total.saturating_sub(j.filled + live);
                let more = if j.slice > 0 { left_after.div_ceil(j.slice) } else { 0 };
                if more > 0 { rows.push(QRow { depth: 1, tag: "THEN", text: format!("{} more slice(s) of up to {}, each placed as soon as the one before fills", more, grouped(j.slice.min(MAX_ORDER_QTY))), detail: String::new(), color: DIM, when: None, est: None }); }
            }
        }
        // everything else the app is doing / about to do
        let owned: Vec<usize> = self.jobs.iter().filter_map(|j| j.cur_trade).collect();
        for (i, t) in self.trades.iter().enumerate() {
            if owned.contains(&i) { continue; }
            match t.status {
                Status::Working => rows.push(QRow { depth: 0, tag: "SEND", text: format!("{} {} x{} @ {:.1} (sent, waiting for the agent)", t.side, pretty_s(&t.id), grouped(t.qty), t.price), detail: String::new(), color: ORANGE, when: Some(0.0), est: Some(self.act_secs_of(if t.kind == Kind::Limit { "place" } else { "other" })) }),
                Status::Resting => rows.push(QRow { depth: 0, tag: "WAIT", text: format!("{} {} x{} @ {:.1} resting, waiting for a fill", t.side, pretty_s(&t.id), grouped(t.qty), t.price), detail: "checked against the order menu every 30 s".into(), color: blue, when: None, est: None }),
                _ => {}
            }
        }
        for (i, _) in &self.ice_deferred {
            let t = &self.trades[*i];
            rows.push(QRow { depth: 0, tag: "CLAIM", text: format!("Claim {} {} x{} (right after the next order is placed)", t.side, pretty_s(&t.id), grouped(t.qty)), detail: "held back so the new order is not delayed".into(), color: ORANGE, when: Some(0.0), est: Some(self.act_secs_of("claim")) });
        }
        let mut claims: Vec<usize> = self.claiming.iter().copied().collect();
        claims.sort();
        for i in claims {
            if let Some(t) = self.trades.get(i) { rows.push(QRow { depth: 0, tag: "CLAIM", text: format!("Claiming {} {} x{} from escrow", t.side, pretty_s(&t.id), grouped(t.qty)), detail: "clicks until the escrow is empty".into(), color: ORANGE, when: Some(0.0), est: Some(self.act_secs_of("claim")) }); }
        }
        if self.wf_running {
            for (i, (name, ok, msg)) in self.wf_steps.iter().enumerate() {
                let (tag, col) = match ok { Some(true) => ("DONE", GREEN), Some(false) => ("FAIL", RED), None => ("FLOW", ORANGE) };
                rows.push(QRow { depth: 0, tag, text: format!("Workflow step {}: {}", i + 1, name), detail: msg.clone(), color: col, when: if ok.is_none() { Some(0.0) } else { None }, est: None });
            }
        }
        // manual and background actions: what was asked of the agent, whether it is done, how long it took
        if let Ok(log) = self.cmd_tx.log.lock() {
            let mut entries: Vec<&ActRec> = log.iter().collect();
            entries.sort_by_key(|a| (a.done.is_some(), std::cmp::Reverse(a.sent)));
            for a in entries.into_iter().take(14) {
                match &a.done {
                    None => rows.push(QRow { depth: 0, tag: "SENT", text: a.label.clone(), detail: format!("sent {} ago, waiting for the agent", fmt_dur(a.sent.elapsed().as_secs_f64())), color: ORANGE, when: Some(0.0), est: Some(self.act_secs_of(match a.cmd.as_str() { "orders" => "orders", "claimorder" | "claim" => "claim", _ => "place" })) }),
                    Some((t, None)) if t.elapsed() < Duration::from_secs(120) => rows.push(QRow { depth: 0, tag: "DONE", text: a.label.clone(), detail: format!("took {}", fmt_dur(t.duration_since(a.sent).as_secs_f64())), color: DIM, when: None, est: None }),
                    Some((t, Some(e))) if t.elapsed() < Duration::from_secs(300) => rows.push(QRow { depth: 0, tag: "FAIL", text: a.label.clone(), detail: e.clone(), color: RED, when: None, est: None }),
                    _ => {}
                }
            }
        }
        if self.agent_up && !self.halted {
            if self.trades.iter().any(|t| t.status == Status::Resting) {
                rows.push(QRow { depth: 0, tag: "POLL", text: "Read the order menu (fill check)".into(), detail: "while orders are resting".into(), color: DIM, when: Some((30.0 - self.last_orders_check.elapsed().as_secs_f64()).max(0.0)), est: Some(self.act_secs_of("orders")) });
            }
        }
        rows
    }

    // ---------- action log (what the user and the app asked the agent to do) ----------
    /// An agent reply finishes the oldest matching request in the log.
    pub fn act_complete(&mut self, v: &Value) {
        let ty = v["type"].as_str().unwrap_or("");
        let (cmds, err): (&[&str], Option<String>) = match ty {
            "orders" => (&["orders"], None), "claimed" => (&["claimorder"], None), "flipped" => (&["fliporder"], None), "sacks" => (&["sackscan"], None),
            "invall" => (&["invall"], None), "requoted_sell" => (&["requote_sell"], None), "rejoin" => (&["rejoin"], None), "balance" => (&["balance", "claim", "cancel", "sackget"], None), "pickup" => (&["pickup"], None), "insert_sacks" => (&["insert_sacks"], None), "sellinv" => (&["sellinv"], None), "bank" => (&["bank"], None),
            "error" => (&[], Some(v["msg"].as_str().unwrap_or("error").to_string())),
            _ => return,
        };
        let Ok(mut log) = self.cmd_tx.log.lock() else { return };
        let idx = if err.is_some() {
            match v["cmd"].as_str() { Some(c) => log.iter().position(|a| a.done.is_none() && a.cmd == c), None => log.iter().position(|a| a.done.is_none()) }
        } else { log.iter().position(|a| a.done.is_none() && cmds.contains(&a.cmd.as_str())) };
        if let Some(i) = idx { log[i].done = Some((Instant::now(), err)); }
    }

    /// Forget old entries and give up on ones the agent never answered.
    fn act_prune(&mut self) {
        let Ok(mut log) = self.cmd_tx.log.lock() else { return };
        for a in log.iter_mut() { if a.done.is_none() && a.sent.elapsed() > Duration::from_secs(300) { a.done = Some((Instant::now(), Some("no answer from the agent".into()))); } }
        log.retain(|a| a.done.as_ref().map_or(true, |(t, _)| t.elapsed() < Duration::from_secs(600)));
        if log.len() > 60 { let n = log.len() - 60; log.drain(..n); }
    }

    /// When the client opens: refresh the escrow (order menu), the inventory and the sacks, once per run.
    pub fn startup_sync(&mut self) {
        if self.startup_synced || (self.mock && !self.sync_in_mock) { return; }
        self.startup_synced = true;
        let adopting = self.jobs.iter().any(|j| j.state == JobState::Adopting);   // finding its order already reads the order menu
        if !adopting { let _ = self.cmd_tx.send(json!({"cmd": "orders", "auto": true})); self.orders_at = Some(Instant::now()); }
        let _ = self.cmd_tx.send(json!({"cmd": "invall"}));
        if self.jobs.iter().any(|j| j.state.live()) {
            self.say(DIM, "sack check at start skipped: an iceberg is running (it needs the game for its orders). Press Scan sacks when it is quiet.");
        } else if !self.sacks.is_empty() || self.flips.iter().any(|f| matches!(f.state, crate::autoflip::FlipState::Running | crate::autoflip::FlipState::Paused)) {
            // a sack scan walks every sack menu and keeps the agent busy for minutes: everything else (requotes, claims, cancels) would wait behind it.
            // The sacks we already know are good enough; Scan sacks on the Inventory screen refreshes them.
        } else if !self.sacks_scanning {
            let _ = self.cmd_tx.send(json!({"cmd": "sackscan"}));
            self.sacks_scanning = true;
        }
        self.say(DIM, "client opened: refreshing escrow, inventory and sacks");
    }

    // ---------- sacks + saved inventory view ----------
    pub fn on_sacks(&mut self, v: &Value) {
        self.set_sacks(v);
        self.sacks_scanning = false;
        self.sacks_at = Some(now_secs());
        self.save_sacks();
    }
    fn set_sacks(&mut self, v: &Value) {
        self.sacks = v["sacks"].as_array().map(|a| a.iter().filter_map(|x| Some(SackInfo {
            name: x["name"].as_str()?.to_string(), copies: x["copies"].as_u64().unwrap_or(1).max(1), cap_each: x["cap_each"].as_u64().unwrap_or(0),
            items: x["items"].as_array().map(|i| i.iter().filter_map(|y| Some((y["name"].as_str()?.to_string(), y["stored"].as_u64().unwrap_or(0)))).collect()).unwrap_or_default(),
        })).collect()).unwrap_or_default();
    }
    pub fn save_sacks(&self) {
        if self.mock || self.is_client() { return; }
        let sacks: Vec<Value> = self.sacks.iter().map(|k| json!({"name": k.name, "copies": k.copies, "cap_each": k.cap_each,
            "items": k.items.iter().map(|(n, s)| json!({"name": n, "stored": s})).collect::<Vec<_>>()})).collect();
        write_data("sacks.json", json!({"at": self.sacks_at, "sacks": sacks}).to_string());
    }
    pub fn load_sacks(&mut self) {
        let Some(v) = data_path("sacks.json").and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { return };
        self.set_sacks(&v);
        self.sacks_at = v["at"].as_f64();
    }
    pub fn load_inv_view(&mut self) {
        let Some(v) = data_path("inventory_view.json").and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { return };
        self.inv_show_all = v["show_all"].as_bool().unwrap_or(false);
        self.inv_show_sacks = v["show_sacks"].as_bool().unwrap_or(false);
        self.inv_search = v["search"].as_str().unwrap_or("").to_string();
        self.saved_view = (self.inv_show_all, self.inv_show_sacks, self.inv_search.clone());
    }
    fn view_autosave(&mut self) {
        let snap = (self.inv_show_all, self.inv_show_sacks, self.inv_search.clone());
        if snap == self.saved_view { return; }
        write_data("inventory_view.json", json!({"show_all": snap.0, "show_sacks": snap.1, "search": snap.2}).to_string());
        self.saved_view = snap;
    }
    /// Items the sacks can hold, aggregated over every sack that lists them.
    pub fn sack_rows(&self) -> Vec<SackRow> {
        let mut map: std::collections::BTreeMap<String, SackRow> = Default::default();
        for k in &self.sacks {
            for (name, stored) in &k.items {
                let r = map.entry(name.to_lowercase()).or_insert_with(|| SackRow { name: name.clone(), stored: 0, cap: 0, sacks: vec![] });
                r.stored += stored; r.cap += k.cap_each * k.copies;
                r.sacks.push(format!("{}{}: {} / {}", k.name, if k.copies > 1 { format!(" ×{}", k.copies) } else { String::new() }, grouped(*stored), grouped(k.cap_each * k.copies)));
            }
        }
        map.into_values().collect()
    }
    pub fn sack_row(&self, name: &str) -> Option<SackRow> { self.sack_rows().into_iter().find(|r| r.name.eq_ignore_ascii_case(name)) }

    // ---------- inventory ----------
    pub fn inv_tick(&mut self) {
        if !matches!(self.screen, Screen::Inventory | Screen::Portfolio) || !self.agent_up || self.inv_unsupported { return; }
        // never stack up reads while the agent is busy with something long (a big NPC sale): wait for the last one to be answered or dropped
        if self.inv_poll_at.map_or(true, |t| t.elapsed() > Duration::from_secs(5)) && !self.busy(&["invall"]) {
            let _ = self.cmd_tx.send(json!({"cmd": "invall"}));
            self.inv_poll_at = Some(Instant::now());
        }
    }
    pub fn on_invall(&mut self, v: &Value) {
        self.inv_all = v["items"].as_array().map(|a| a.iter().filter_map(|x| Some(InvItem { id: x["id"].as_str()?.to_string(), count: x["count"].as_u64()?, fits: x["fits"].as_u64().unwrap_or(0) })).collect()).unwrap_or_default();
        self.inv_free = v["free_slots"].as_u64();
        self.inv_slots = v["slots"].as_array().map(|a| a.iter().filter_map(|x| Some(InvSlot { slot: x["slot"].as_u64()? as u8, id: x["id"].as_str()?.to_string(), count: x["count"].as_u64().unwrap_or(1) })).collect()).unwrap_or_default();
        self.inv_unsupported = false;
        self.inv_all_at = Some(Instant::now());
    }
    pub fn open_screen(&mut self, s: Screen) {
        self.screen = if self.screen == s { Screen::Main } else { s };
        if self.screen == Screen::Inventory || self.screen == Screen::Portfolio {
            self.inv_poll_at = None;
            if self.agent_up && !self.halted && self.orders_at.map_or(true, |t| t.elapsed() > Duration::from_secs(120)) && self.inflight.is_empty() {
                let _ = self.cmd_tx.send(json!({"cmd": "orders", "auto": true}));
                self.orders_at = Some(Instant::now());
            }
        }
        if self.screen == Screen::Queue && self.ice_form.product.is_empty() { self.ice_form.product = self.selected.clone().unwrap_or_default(); }
    }

    /// Per product: (in inventory, room left, in escrow, unfilled buys, listed sells)
    pub fn inv_figures(&self, id: &str) -> (u64, u64, u64, u64, u64) {
        let item = self.inv_all.iter().find(|x| x.id == id || pretty(&x.id).eq_ignore_ascii_case(&pretty(id)));
        let holding = item.map_or(0, |x| x.count);
        let fits = item.map_or(self.inv_free.unwrap_or(0) * 64, |x| x.fits);
        let (mut escrow, mut buys, mut sells) = (0, 0, 0);
        for r in self.orders.iter().filter(|r| same_product(&r.product, id)) {
            let open = r.total.saturating_sub(r.filled);
            if r.side == "BUY" { escrow += r.to_claim; buys += open; } else { sells += open; }
        }
        (holding, fits, escrow, buys, sells)
    }
}

// ---------- crafting recipes ----------
/// upgraded item -> (ingredient, how many ingredients per one upgraded item); from the NEU item repo via the agent.
fn recipes() -> &'static HashMap<String, (String, u64)> {
    static R: std::sync::OnceLock<HashMap<String, (String, u64)>> = std::sync::OnceLock::new();
    R.get_or_init(|| serde_json::from_str(include_str!("../assets/recipes.json")).unwrap_or_default())
}

/// How an upgraded item is crafted: (ingredient id, how many of it).
pub fn recipe_of(id: &str) -> Option<(String, u64)> { recipes().get(id).cloned() }
/// What this item is an ingredient of: (upgraded id, how many of this item one needs).
pub fn used_in(id: &str) -> Vec<(String, u64)> {
    let mut v: Vec<(String, u64)> = recipes().iter().filter(|(_, (ing, _))| ing == id).map(|(out, (_, n))| (out.clone(), *n)).collect();
    v.sort();
    v
}

/// Walk down the recipe chain: (root item, raw units of the root per one `id`, number of crafting steps).
pub fn raw_chain(id: &str) -> (String, u64, u32) {
    let (mut cur, mut mult, mut steps) = (id.to_string(), 1u64, 0u32);
    while let Some((ing, n)) = recipes().get(&cur) {
        mult = mult.saturating_mul(*n); cur = ing.clone(); steps += 1;
        if steps > 12 { break; }
    }
    (cur, mult, steps)
}

impl Gui {
    /// Product id for a display name ("Enchanted Seeds" -> ENCHANTED_SEEDS), checked against recipes and the bazaar list.
    pub fn id_for_name(&self, name: &str) -> String {
        let guess = name.to_uppercase().replace(' ', "_");
        if recipes().contains_key(&guess) || self.products.contains_key(&guess) { return guess; }
        if let Some(k) = self.products.keys().find(|k| pretty(k).eq_ignore_ascii_case(name)) { return k.clone(); }
        if let Some(k) = recipes().keys().chain(recipes().values().map(|v| &v.0)).find(|k| pretty(k).eq_ignore_ascii_case(name)) { return k.clone(); }
        guess
    }

    /// The drop-down under an item: one row of raw-material numbers lined up under the table's columns
    /// (each figure times the raw units one of this item is worth), nothing else.
    fn raw_row(&self, ui: &mut egui::Ui, id: &str, first_w: f32, lead: f32, widths: &[f32], vals: &[(u64, Color32)]) {
        let (root, mult, _) = raw_chain(id);
        egui::Frame::NONE.fill(Color32::from_gray(14)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(lead);
                ui.allocate_ui_with_layout(egui::vec2(first_w, 22.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.set_min_width(first_w);
                    // name the raw material these numbers are counted in
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let (r, _) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::hover());
                    if let Some(ic) = &self.icons { ic.paint(ui.painter(), &root, r); }
                    ui.label(RichText::new(pretty_s(&root)).color(DIM));
                });
                for ((v, c), w) in vals.iter().zip(widths) {
                    ui.allocate_ui_with_layout(egui::vec2(*w, 22.0), egui::Layout::left_to_right(egui::Align::Center), |ui| { ui.set_min_width(*w); ui.label(RichText::new(short_num(v.saturating_mul(mult))).color(*c)); });
                }
            });
        });
    }
}

/// 1,234 -> "1.23k", 5,200,000 -> "5.20m", 3,225,600,000 -> "3.23b".
pub fn short_num(n: u64) -> String {
    let f = n as f64;
    if f >= 1e9 { format!("{:.2}b", f / 1e9) } else if f >= 1e6 { format!("{:.2}m", f / 1e6) } else if f >= 1e3 { format!("{:.2}k", f / 1e3) } else { n.to_string() }
}

/// An item icon + name that can be clicked. Returns true when clicked.
fn icon_toggle(ui: &mut egui::Ui, icons: &Option<Icons>, id: &str, text: &str, open: bool) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (r, resp) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::click());
        if let Some(ic) = icons { ic.paint(ui.painter(), id, r); }
        if resp.hovered() || open { ui.painter().rect_stroke(r.expand(1.0), 2.0, Stroke::new(1.0, if open { ORANGE } else { DIM }), egui::StrokeKind::Inside); }
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).fx_tip("Click for the raw-material numbers");
        let lab = ui.add(egui::Label::new(text).sense(egui::Sense::click()));
        clicked = resp.clicked() || lab.clicked();
    });
    clicked
}

// ---------- screens ----------
/// Is what was typed into an iceberg field acceptable? `None` = empty / not judged yet, so nothing is shown red until you type.
pub fn ice_field_valid(which: u8, text: &str) -> Option<bool> {
    let t = text.trim();
    if t.is_empty() { return None; }
    let f = t.parse::<f64>().ok().filter(|v| v.is_finite());
    Some(match which {
        0 | 5 => parse_qty(t).map_or(false, |v| v > 0),
        1 | 3 => f.map_or(false, |v| v >= 0.0),
        2 => f.map_or(false, |v| v > 0.0),
        4 => f.map_or(false, |v| v >= 0.5),
        _ => true,
    })
}

pub fn field(ui: &mut egui::Ui, label: &str, text: &mut String, width: f32, hint: &str, valid: Option<bool>) {
    let width = width.max(label.chars().count() as f32 * 8.6 + 4.0);
    ui.vertical(|ui| {
        ui.label(RichText::new(label).color(DIM).size(16.0));
        let resp = ui.add(egui::TextEdit::singleline(text).desired_width(width).hint_text(hint));
        fx::focus_ring(ui, &resp, valid);
        if valid == Some(false) { fx::tip(resp, "Not a valid value for this field"); }
    });
    ui.add_space(8.0);
}

fn grouped_u(n: u64) -> String { grouped(n) }

/// What a column heading means (shown as a tooltip).
fn head_tip(t: &str) -> &'static str {
    match t {
        "In inventory" => "How many you are carrying right now",
        "Room left" => "How many more would fit in your inventory (free slots plus room in part-full stacks)",
        "If all fill" => "Room left in the inventory once everything in escrow and every open buy order has arrived",
        "Open buys" => "Items still to come from your open buy orders",
        "In sacks" => "How many your sacks hold (from the last sack scan)",
        "Sack capacity" => "How many your sacks can hold in total; sacks with the same name add their capacity",
        "Sack room left" => "Sack capacity minus what is already stored",
        "IAOF" => "If All Orders Fill: the sack room that would be left after all escrow and open buys arrive (never below 0)",
        _ => "",
    }
}

impl Gui {
    pub fn screen_frame(&mut self, ctx: &egui::Context, title: &str, body: impl FnOnce(&mut Self, &mut egui::Ui)) {
        egui::CentralPanel::default().frame(egui::Frame::NONE.inner_margin(0)).show(ctx, |ui| {
            terminal_frame().show(ui, |ui| {
                let hb = panel_heading(ui, title, true);
                if close_x(ui, hb) { self.screen = Screen::Main; }
                ui.add_space(6.0);
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| body(self, ui));
            });
        });
    }

    pub fn queue_screen(&mut self, ctx: &egui::Context) {
        self.screen_frame(ctx, "Bot Queue  QUEUE", |g, ui| {
            g.iceberg_form(ui);
            ui.add_space(8.0);
            g.ice_jobs_ui(ui);
            ui.add_space(8.0);
            g.queue_table(ui);
        });
    }

    fn iceberg_form(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("New iceberg order (buy)").color(ORANGE).size(20.0));
        ui.label(RichText::new("Keeps one buy order resting at top bid + offset. If it fills, the next one goes out at once; if not, it is cancelled and re-placed every requote period.").color(DIM).size(15.0));
        ui.add_space(4.0);
        let mut use_selected = false;
        let vv: Vec<Option<bool>> = vec![ice_field_valid(0, &self.ice_form.total), ice_field_valid(1, &self.ice_form.min), ice_field_valid(2, &self.ice_form.max),
            ice_field_valid(3, &self.ice_form.offset), ice_field_valid(4, &self.ice_form.requote), ice_field_valid(5, &self.ice_form.slice)];
        ui.horizontal_wrapped(|ui| {
            field(ui, "Item", &mut self.ice_form.product, 200.0, "e.g. SEEDS", None);
            if ui.fx_button("Use selected").fx_tip("Fill in the item selected in the watchlist").clicked() { use_selected = true; }
            field(ui, "Total to buy", &mut self.ice_form.total, 110.0, "2m", vv[0]);
            field(ui, "Price min", &mut self.ice_form.min, 70.0, "0.1", vv[1]);
            field(ui, "Price max", &mut self.ice_form.max, 70.0, "1.5", vv[2]);
            field(ui, "Over top bid by", &mut self.ice_form.offset, 70.0, "0.1", vv[3]);
            field(ui, "Requote (min)", &mut self.ice_form.requote, 70.0, "5", vv[4]);
            field(ui, "Max per order", &mut self.ice_form.slice, 90.0, "71680", vv[5]);
        });
        ui.horizontal(|ui| {
            ui.fx_checkbox(&mut self.ice_form.on_outbid, "Requote when outbid").fx_tip("Requotes as soon as someone bids above your order, instead of waiting for the timer (the timer still applies). Prices lag the game by about 10–20 s.");
            if self.ice_form.on_outbid { ui.label("at most every"); ui.add(egui::TextEdit::singleline(&mut self.ice_form.gap).desired_width(40.0)); ui.label("s"); if ui.button("⚡ Instant").on_hover_text("0 s: requote the moment a new price shows you outbid").clicked() { self.ice_form.gap = "0".into(); } }
        });
        ui.fx_checkbox(&mut self.ice_form.auto_claim, "Auto-claim filled slices from escrow").fx_tip("Off: every filled slice stays in escrow until you claim it yourself. The bazaar limits how many orders you can have, filled-but-unclaimed ones included.");
        if use_selected { if let Some(s) = self.selected.clone() { self.ice_form.product = s; } }
        let parsed = self.ice_parse_form();
        match &parsed {
            Ok(j) => {
                let n = j.total.div_ceil(j.slice.max(1));
                ui.label(RichText::new(format!("Buys up to {} {} in {} slice(s) of ≤ {} at top bid + {:.1}, never above {:.1}. Worst-case cost {} coins.",
                    grouped(j.total), pretty_s(&j.product), n, grouped(j.slice), j.offset, j.max_price, fmt_coins(j.total as f64 * j.max_price))).color(Color32::WHITE).size(16.0));
            }
            Err(e) => { ui.label(RichText::new(e).color(DIM).size(16.0)); }
        }
        ui.horizontal(|ui| {
            if ui.add_enabled(parsed.is_ok(), egui::Button::new(RichText::new("  Start iceberg  ").size(18.0))).clicked() {
                if let Ok(j) = parsed {
                    if j.total as f64 * j.max_price > CONFIRM_ABOVE { self.ice_confirm = Some(j); }
                    else { self.ice_add(j); self.ice_form.msg = Some((true, "started".into())); }
                }
            }
            if !self.agent_up { ui.label(RichText::new("agent offline: it will start placing once the agent connects").color(ORANGE)); }
            if let Some((ok, m)) = &self.ice_form.msg { ui.label(RichText::new(m).color(if *ok { GREEN } else { RED })); }
        });
    }

    /// Start the iceberg described by the form (asks first when the worst-case cost is large). Used by the order ticket.
    pub fn ice_submit(&mut self) {
        match self.ice_parse_form() {
            Ok(j) => {
                if j.total as f64 * j.max_price > CONFIRM_ABOVE { self.ice_confirm = Some(j); }
                else { let (n, p) = (grouped(j.total), pretty_s(&j.product)); self.ice_add(j); self.ice_form.msg = Some((true, "started".into())); let _ = self.toast_new(1, format!("Iceberg started: {n} {p}")); }
            }
            Err(e) => { let _ = self.toast_new(2, format!("Iceberg not started: {e}")); self.ice_form.msg = Some((false, e)); }
        }
    }

    pub fn ice_parse_form(&self) -> Result<IceJob, String> {
        let f = &self.ice_form;
        let q = f.product.trim().to_uppercase().replace(' ', "_");
        if q.is_empty() { return Err("pick an item".into()); }
        let id = if self.products.contains_key(&q) { q.clone() } else {
            let mut ids: Vec<&String> = self.products.keys().filter(|k| k.contains(&q)).collect();
            ids.sort_by_key(|k| k.len());
            match ids.first() { Some(s) => s.to_string(), None => return Err(format!("no product matching {q}")) }
        };
        let total = parse_qty(&f.total).ok_or("enter a total quantity, e.g. 2m or 500k")?;
        let num = |s: &str, what: &str| s.trim().parse::<f64>().ok().filter(|v| *v >= 0.0 && v.is_finite()).ok_or(format!("enter {what}"));
        let min = if f.min.trim().is_empty() { 0.1 } else { num(&f.min, "a minimum price")? };
        let max = num(&f.max, "a maximum price")?;
        if max <= 0.0 || max < min { return Err("max price must be above min price".into()); }
        let offset = if f.offset.trim().is_empty() { 0.1 } else { num(&f.offset, "the offset over the top bid")? };
        let mins = if f.requote.trim().is_empty() { 5.0 } else { num(&f.requote, "the requote time in minutes")? };
        if mins < 0.5 { return Err("requote time must be at least 30 seconds".into()); }
        let slice = if f.slice.trim().is_empty() { MAX_ORDER_QTY } else { parse_qty(&f.slice).ok_or("enter the max per order")? };
        if slice == 0 { return Err("max per order must be at least 1".into()); }
        let mut job = IceJob::new(0, id, total, slice.min(MAX_ORDER_QTY), min, max, offset, (mins * 60.0).round() as u64);
        job.auto_claim = f.auto_claim;
        job.on_outbid = f.on_outbid;
        job.outbid_gap = if f.gap.trim().is_empty() { 30 } else { f.gap.trim().parse::<u64>().ok().ok_or("the outbid gap is a number of seconds (0 = instant)")? };
        Ok(job)
    }

    fn ice_jobs_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Iceberg orders").color(ORANGE).size(20.0));
            if ui.fx_checkbox(&mut self.ice_auto_remove, "Remove finished icebergs automatically").fx_tip("A completed iceberg leaves this list a few seconds after it finishes. Stopped and paused ones stay until you remove them.").changed() { self.save_layout(); let v = self.ice_auto_remove; self.push_setting("ice_auto_remove", v); }
        });
        if self.jobs.is_empty() { ui.label(RichText::new("none yet").color(DIM)); return; }
        let (mut pause, mut resume, mut stop, mut remove, mut edit) = (None, None, None, None, None::<usize>);
        let mut gr=fx::GridRows::new(ui.ctx(),egui::Id::new("tbl_ice_jobs"),1);
        let colors=gr.colors(ui.ctx(),self.jobs.len()+1);
        let grid_out=egui::Grid::new("ice_jobs").striped(true).spacing([18.0, 4.0]).with_row_color({let c=colors; move |i,_|c.get(i).copied().flatten()}).show(ui, |ui| {
 gr.row_start(ui);
            for t in ["#", "Item", "Progress", "State", "Order now", "Requotes", ""] { ui.label(RichText::new(t).color(Color32::WHITE).strong()); }
            ui.end_row(); gr.row_start(ui);
            for (i, j) in self.jobs.iter().enumerate() {
                ui.label(j.id.to_string());
                icon_cell(ui, &self.icons, &j.product, &pretty_s(&j.product));
                ui.label(format!("{} / {}", grouped((j.filled + j.cur_filled).min(j.total)), grouped(j.total)));
                let col = match j.state { JobState::Done => GREEN, JobState::Stopped | JobState::Paused => DIM, _ => ORANGE };
                ui.label(RichText::new(j.state.label()).color(col)).fx_tip(&j.note);
                ui.label(if j.cur_trade.is_some() { format!("{} @ {:.1} ({} filled)", grouped(j.cur_qty), j.cur_price, grouped(j.cur_filled)) } else { "—".into() });
                ui.label(j.requotes.to_string());
                ui.horizontal(|ui| {
                    if j.state == JobState::Paused { if ui.fx_small_button("Resume").clicked() { resume = Some(i); } }
                    else if matches!(j.state, JobState::Idle | JobState::Placing | JobState::Resting) && ui.fx_small_button("Pause").clicked() { pause = Some(i); }
                    if !matches!(j.state, JobState::Done | JobState::Stopped) && ui.fx_small_button("Edit").fx_tip("Change total, price band, offset, requote time and options of this running iceberg").clicked() { edit = Some(i); }
                    if !matches!(j.state, JobState::Done | JobState::Stopped) && ui.fx_small_button("Stop").fx_tip("Cancels the resting order (keeping anything already filled)").clicked() { stop = Some(i); }
                    if matches!(j.state, JobState::Done | JobState::Stopped) || (j.state == JobState::Paused && !j.has_live_order()) { if ui.fx_small_button("Remove").clicked() { remove = Some(i); } }
                });
                ui.end_row(); gr.row_start(ui);
            }
        });
        gr.finish(ui,grid_out.response.rect,4.0);
        for j in &self.jobs { if !j.note.is_empty() && j.state.live() { ui.label(RichText::new(format!("#{}: {}", j.id, j.note)).color(DIM).size(15.0)); } }
        if let Some(i) = edit { if let Some(j) = self.jobs.get(i) {
            let f = IceForm { product: j.product.clone(), total: j.total.to_string(), min: j.min_price.to_string(), max: j.max_price.to_string(), offset: j.offset.to_string(),
                requote: (j.requote_secs as f64 / 60.0).to_string(), slice: j.slice.to_string(), auto_claim: j.auto_claim, on_outbid: j.on_outbid, gap: j.outbid_gap.to_string(), ..Default::default() };
            self.ice_edit = Some((j.id, f, false, String::new()));
        } }
        if let Some(i) = pause { self.ice_pause(i); }
        if let Some(i) = resume { self.ice_resume(i); }
        if let Some(i) = stop { self.ice_stop(i); }
        if let Some(i) = remove { self.ice_remove(i); }
    }

    /// The "Edit iceberg" window (opened by the Edit button in the jobs list).
    pub fn ice_edit_dialog(&mut self, ctx: &egui::Context) {
        let Some((id, mut f, mut now, err)) = self.ice_edit.clone() else { return };
        let Some(job) = self.jobs.iter().find(|j| j.id == id) else { self.ice_edit = None; return };
        let title = format!("Edit iceberg #{id}: {}", pretty_s(&job.product));
        let mut open = true; let (mut save, mut cancel) = (false, false);
        egui::Window::new(title).id(egui::Id::new("ice_edit_win")).open(&mut open).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            ui.label(RichText::new("The order resting in game keeps its old price until the next requote (or tick \"requote now\").").color(DIM).size(15.0));
            let vv: Vec<Option<bool>> = vec![ice_field_valid(0, &f.total), ice_field_valid(1, &f.min), ice_field_valid(2, &f.max), ice_field_valid(3, &f.offset), ice_field_valid(4, &f.requote), ice_field_valid(5, &f.slice)];
            ui.horizontal_wrapped(|ui| {
                field(ui, "Total to buy", &mut f.total, 110.0, "2m", vv[0]);
                field(ui, "Max per order", &mut f.slice, 90.0, "71680", vv[5]);
                field(ui, "Price min", &mut f.min, 70.0, "0.1", vv[1]);
                field(ui, "Price max", &mut f.max, 70.0, "1.5", vv[2]);
                field(ui, "Over top bid by", &mut f.offset, 70.0, "0.1", vv[3]);
                field(ui, "Requote (min)", &mut f.requote, 70.0, "5", vv[4]);
            });
            ui.horizontal(|ui| {
                ui.fx_checkbox(&mut f.on_outbid, "Requote when outbid");
                if f.on_outbid { ui.label("at most every"); ui.add(egui::TextEdit::singleline(&mut f.gap).desired_width(40.0)); ui.label("s"); if ui.button("⚡ Instant").clicked() { f.gap = "0".into(); } }
            });
            ui.fx_checkbox(&mut f.auto_claim, "Auto-claim filled slices from escrow");
            ui.fx_checkbox(&mut now, "Requote now (apply the new price settings to the resting order right away)");
            if !err.is_empty() { ui.label(RichText::new(&err).color(RED)); }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.fx_button("  Save  ").clicked() { save = true; }
                if ui.fx_button("  Cancel  ").clicked() { cancel = true; }
            });
        });
        if save {
            let num = |s: &str| s.trim().parse::<f64>().ok().filter(|v| *v >= 0.0 && v.is_finite());
            let r = (|| -> Result<IceEdit, String> {
                let total = parse_qty(&f.total).ok_or("enter a total quantity, e.g. 2m")?;
                let slice = parse_qty(&f.slice).ok_or("enter the max per order")?;
                let min = num(&f.min).ok_or("enter a minimum price")?; let max = num(&f.max).ok_or("enter a maximum price")?;
                let offset = num(&f.offset).ok_or("enter the offset")?; let mins = num(&f.requote).ok_or("enter the requote minutes")?;
                if max <= 0.0 || max < min { return Err("max price must be above min price".into()); }
                if mins < 0.5 { return Err("requote time must be at least 30 seconds".into()); }
                if total == 0 || slice == 0 { return Err("total and max per order must be at least 1".into()); }
                let gap = if f.gap.trim().is_empty() { 30 } else { f.gap.trim().parse::<u64>().ok().ok_or("the outbid gap is a number of seconds (0 = instant)")? };
                Ok(IceEdit { id, total, slice, min, max, offset, requote_secs: (mins * 60.0).round() as u64, auto_claim: f.auto_claim, on_outbid: f.on_outbid, outbid_gap: gap, requote_now: now })
            })();
            match r.and_then(|e| self.ice_edit(&e)) {
                Ok(()) => { self.ice_edit = None; let _ = self.toast_new(1, format!("Iceberg #{id} updated")); }
                Err(e) => { self.ice_edit = Some((id, f, now, e)); }
            }
        } else if cancel || !open { self.ice_edit = None; }
        else { self.ice_edit = Some((id, f, now, err)); }
    }

    fn queue_table(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Everything queued").color(ORANGE).size(20.0));
        let rows = self.queue_rows();
        if rows.is_empty() { ui.label(RichText::new("The bot has nothing queued.").color(DIM)); return; }
        let mut gr=fx::GridRows::new(ui.ctx(),egui::Id::new("tbl_queue_rows"),1);
        let colors=gr.colors(ui.ctx(),rows.len()+1);
        let grid_out=egui::Grid::new("queue_rows").striped(true).spacing([18.0, 4.0]).min_col_width(60.0).with_row_color({let c=colors; move |i,_|c.get(i).copied().flatten()}).show(ui, |ui| {
 gr.row_start(ui);
            for t in ["", "Action", "Happens in", "Takes about", "Detail"] { ui.label(RichText::new(t).color(Color32::WHITE).strong()); }
            ui.end_row(); gr.row_start(ui);
            for r in &rows {
                ui.label(RichText::new(r.tag).color(r.color).strong());
                let indent = "      ".repeat(r.depth as usize);
                ui.label(RichText::new(format!("{indent}{}", r.text)).color(if r.depth == 0 { Color32::WHITE } else { Color32::from_gray(200) }));
                ui.label(match r.when { Some(s) if s < 1.0 => "now".to_string(), Some(s) => fmt_dur(s), None => "—".into() });
                ui.label(match r.est { Some(s) => format!("~{}", fmt_dur(s)), None => "—".into() });
                ui.label(RichText::new(&r.detail).color(DIM));
                ui.end_row(); gr.row_start(ui);
            }
        });
        gr.finish(ui,grid_out.response.rect,4.0);
        let active: Vec<&IceJob> = self.jobs.iter().filter(|j| j.state.live()).collect();
        if !active.is_empty() {
            ui.add_space(6.0);
            for j in active {
                let t = match self.ice_eta(j) { Some(s) => format!("about {} to finish (from the fill rate so far)", fmt_dur(s)), None => "finish time unknown until the first fills come in".into() };
                ui.label(RichText::new(format!("Iceberg #{}: {}", j.id, t)).color(DIM));
            }
        }
    }

    /// A picture of the real inventory (3 rows of storage above the hotbar), top right of the Inventory screen.
    fn inv_picture(&self, ui: &mut egui::Ui, area: egui::Rect) {
        let (slot, gap, pad, title_h) = (36.0f32, 2.0f32, 8.0f32, 24.0f32);
        let w = 9.0 * slot + 8.0 * gap + pad * 2.0;
        let h = title_h + 3.0 * (slot + gap) + 8.0 + slot + pad * 2.0;
        if area.width() < w + 760.0 { return; }   // no room beside the controls (narrow window)
        let rect = egui::Rect::from_min_size(egui::pos2(area.right() - w - 6.0, area.top() + 4.0), egui::vec2(w, h));
        let p = ui.painter().clone();
        p.rect_filled(rect, 4.0, Color32::from_rgb(10, 10, 10));
        p.rect_stroke(rect, 4.0, Stroke::new(1.0, Color32::from_gray(110)), egui::StrokeKind::Inside);
        let used = self.inv_slots.len();
        let title = if self.inv_all_at.is_none() { "In your inventory: reading…".to_string() } else { format!("In your inventory  ·  {used}/36 slots") };
        p.text(rect.left_top() + egui::vec2(pad, title_h / 2.0 + 2.0), egui::Align2::LEFT_CENTER, title, FontId::proportional(17.0), ORANGE);
        let cell = |i: u8| -> egui::Rect {   // 9..35 are the three storage rows (top to bottom), 0..8 the hotbar underneath
            let (row, col) = if i >= 9 { (((i - 9) / 9) as f32, ((i - 9) % 9) as f32) } else { (3.0, i as f32) };
            let extra = if i < 9 { 8.0 } else { 0.0 };
            egui::Rect::from_min_size(rect.left_top() + egui::vec2(pad + col * (slot + gap), title_h + pad + row * (slot + gap) + extra), egui::vec2(slot, slot))
        };
        for i in 0..36u8 {
            let c = cell(i);
            p.rect_filled(c, 2.0, Color32::from_rgb(26, 26, 26));
            p.rect_stroke(c, 2.0, Stroke::new(1.0, Color32::from_gray(58)), egui::StrokeKind::Inside);
            let Some(s) = self.inv_slots.iter().find(|s| s.slot == i) else { continue };
            let inner = c.shrink(5.0);
            match &self.icons {
                Some(ic) if ic.has(&s.id) => ic.paint(&p, &s.id, inner),
                _ => {   // an item with no icon (not on the bazaar): its first letters on a coloured tile
                    let hue = s.id.bytes().fold(7u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
                    let col = Color32::from_rgb(60 + (hue % 90) as u8, 60 + ((hue / 90) % 90) as u8, 60 + ((hue / 8100) % 90) as u8);
                    p.rect_filled(inner, 3.0, col);
                    let ab: String = pretty_s(&s.id).split_whitespace().filter_map(|w| w.chars().next()).take(2).collect::<String>().to_uppercase();
                    p.text(inner.center(), egui::Align2::CENTER_CENTER, ab, FontId::proportional(14.0), Color32::WHITE);
                }
            }
            if s.count > 1 {
                let t = if s.count >= 1_000_000 { format!("{:.1}m", s.count as f64 / 1e6) } else if s.count >= 1000 { format!("{:.1}k", s.count as f64 / 1e3).replace(".0k", "k") } else { s.count.to_string() };
                p.text(c.right_bottom() - egui::vec2(3.0, 2.0) + egui::vec2(1.0, 1.0), egui::Align2::RIGHT_BOTTOM, &t, FontId::proportional(14.0), Color32::BLACK);
                p.text(c.right_bottom() - egui::vec2(3.0, 2.0), egui::Align2::RIGHT_BOTTOM, &t, FontId::proportional(14.0), Color32::WHITE);
            }
            let r = ui.interact(c, ui.id().with(("invslot", i)), egui::Sense::hover());
            fx::tip(r, format!("{} ×{}  ·  {}", pretty_s(&s.id), grouped(s.count), if i < 9 { format!("hotbar slot {}", i + 1) } else { format!("slot {i}") }));
        }
        if self.inv_all_at.is_some() && self.inv_slots.is_empty() && !self.inv_all.is_empty() {
            p.text(rect.center_bottom() - egui::vec2(0.0, 4.0), egui::Align2::CENTER_BOTTOM, "slot layout needs the newest agent", FontId::proportional(14.0), DIM);
        }
    }

    pub fn inventory_screen(&mut self, ctx: &egui::Context) {
        self.screen_frame(ctx, "Inventory  INV", |g, ui| {
            let area = ui.max_rect();
            g.inv_picture(ui, area);
            ui.horizontal_wrapped(|ui| {
                if ui.fx_button("Refresh").fx_tip("Re-read the inventory and the open orders").clicked() {
                    g.inv_poll_at = None;
                    if g.agent_up { let _ = g.cmd_tx.send(json!({"cmd": "orders"})); g.orders_at = Some(Instant::now()); }
                }
                ui.fx_checkbox(&mut g.inv_show_all, "Show items that are not on the bazaar");
                ui.fx_checkbox(&mut g.inv_show_sacks, "Show items we can hold via sacks");
                if g.inv_show_sacks {
                    let label = if g.sacks_scanning { "Scanning sacks… (~45 s)" } else { "Scan sacks" };
                    if ui.add_enabled(g.agent_up && !g.sacks_scanning && !g.halted, egui::Button::new(label)).fx_tip("Opens every sack once in game to read what it holds").clicked() {
                        let _ = g.cmd_tx.send(json!({"cmd": "sackscan"}));
                        g.sacks_scanning = true;
                    }
                    if let Some(t) = g.sacks_at { ui.label(RichText::new(format!("sacks scanned {} ago", iceberg::fmt_dur(now_secs() - t))).color(DIM)); }
                }
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("Search").color(DIM));
                let sr_ = ui.add(egui::TextEdit::singleline(&mut g.inv_search).desired_width(260.0).hint_text("e.g. seed"));
                fx::focus_ring(ui, &sr_, None);
                if !g.inv_search.is_empty() && ui.fx_small_button("×").fx_tip("Clear the search").clicked() { g.inv_search.clear(); }
                match g.inv_all_at { Some(t) => ui.label(RichText::new(format!("inventory read {}s ago", t.elapsed().as_secs())).color(DIM)), None => ui.label(RichText::new("inventory: not read yet").color(DIM)) };
                if let Some(t) = g.orders_at { ui.label(RichText::new(format!("· orders read {}s ago", t.elapsed().as_secs())).color(DIM)); }
                if let Some(f) = g.inv_free { ui.label(RichText::new(format!("· {f} free slot(s)")).color(DIM)); }
            });
            if g.inv_unsupported { ui.label(RichText::new("The agent in Minecraft is too old to list the inventory. Use Settings → Restart agent after copying the newest blockberg_agent.py to the laptop.").color(RED)); }
            else if !g.agent_up { ui.label(RichText::new("Agent offline").color(RED)); }
            ui.add_space(6.0);
            let needle = g.inv_search.trim().to_lowercase();
            let matches = |name: &str| needle.is_empty() || name.to_lowercase().contains(&needle);
            // every cell is a fixed-width, left-aligned box, so the header and the rows line up
            fn cell(ui: &mut egui::Ui, width: f32, add: impl FnOnce(&mut egui::Ui)) {
                ui.allocate_ui_with_layout(egui::vec2(width, 22.0), egui::Layout::left_to_right(egui::Align::Center), |ui| { ui.set_min_width(width); add(ui); });
            }
            if g.inv_show_sacks { g.sack_table(ui, &needle); return; }

            // every item we hold, plus every item with an open order
            let mut ids: Vec<String> = g.inv_all.iter().filter(|x| g.inv_show_all || g.products.contains_key(&x.id)).map(|x| x.id.clone()).collect();
            for r in &g.orders { let id = r.product.to_uppercase().replace(' ', "_"); if !ids.contains(&id) { ids.push(id); } }
            let hidden = g.inv_all.len();
            ids.retain(|id| matches(&pretty(id)) || matches(id));
            if ids.is_empty() && g.inv_all_at.is_none() {
                // not read yet: rows shaped like the table that is about to appear
                for k in 0..8 {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width().min(760.0), 22.0), egui::Sense::hover());
                    let w = 0.45 + 0.1 * (k % 3) as f32;
                    fx::skeleton(ui, egui::Rect::from_min_size(r.min, egui::vec2(r.width() * w, 18.0)), 3.0);
                    ui.add_space(4.0);
                }
                return;
            }
            if ids.is_empty() {
                ui.label(RichText::new(if g.inv_all_at.is_none() { "Waiting for the agent to read the inventory…".to_string() }
                    else if !needle.is_empty() { format!("Nothing held or on order matches \"{}\". Try \"Show items we can hold via sacks\" to search everything you could carry.", g.inv_search.trim()) }
                    else if hidden > 0 { format!("No bazaar items held and no open orders ({hidden} other item(s) hidden: tick the box above to show them).") }
                    else { "Nothing held and no open orders.".to_string() }).color(DIM));
                return;
            }
            let w = [250.0, 120.0, 120.0, 140.0, 120.0];
            ui.horizontal(|ui| {
                ui.add_space(24.0);
                for (n, (t, wd)) in ["Item", "In inventory", "Room left", "If all fill", "Open buys"].iter().zip(w).enumerate() { let wd = if n == 0 { wd - 22.0 } else { wd }; cell(ui, wd, |ui| { let l = ui.label(RichText::new(*t).color(Color32::WHITE).strong()); if !head_tip(t).is_empty() { fx::tip(l, head_tip(t)); } }); }
            });
            ui.separator();
            for id in ids {
                let (holding, fits, escrow, buys, sells) = g.inv_figures(&id);
                let incoming = escrow + buys;                       // everything that will still land in the inventory
                let room_after = fits.saturating_sub(incoming);     // room left once all of it has arrived
                let overflow = incoming.saturating_sub(fits);
                let sack = g.sack_row(&pretty(&id));
                let cs_id = ui.make_persistent_id(("inv_row", &id));
                let mut toggle = false;
                let (_tg, hdr_resp, _bd) = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), cs_id, g.inv_open).show_header(ui, |ui| {
                    cell(ui, w[0] - 22.0, |ui| toggle = icon_toggle(ui, &g.icons, &id, &pretty_s(&id), g.recipe_open.contains(&id)));
                    cell(ui, w[1], |ui| { ui.label(grouped_u(holding)); });
                    cell(ui, w[2], |ui| { ui.label(RichText::new(grouped_u(fits)).color(DIM)); });
                    cell(ui, w[3], |ui| { ui.label(RichText::new(grouped_u(room_after)).color(if overflow > 0 { ORANGE } else { Color32::WHITE })); });
                    cell(ui, w[4], |ui| { ui.label(RichText::new(if buys > 0 { format!("+{}", grouped_u(buys)) } else { "—".into() }).color(if buys > 0 { GREEN } else { DIM })); });
                }).body(|ui| {
                    egui::Grid::new(("inv_detail", &id)).num_columns(2).spacing([24.0, 3.0]).show(ui, |ui| {
                        let line = |ui: &mut egui::Ui, k: &str, v: String, c: Color32| { ui.label(RichText::new(k).color(DIM)); ui.label(RichText::new(v).color(c)); ui.end_row(); };
                        line(ui, "Holding now (inventory)", grouped_u(holding), Color32::WHITE);
                        match (&sack, g.holdings.get(&id)) {
                            (Some(r), _) => line(ui, "In sacks", format!("{} of {} capacity ({} room)", grouped_u(r.stored), grouped_u(r.cap), grouped_u(r.cap.saturating_sub(r.stored))), Color32::WHITE),
                            (None, Some((_, Some(s)))) => line(ui, "In sacks", grouped_u(*s), Color32::WHITE),
                            _ => line(ui, "In sacks", "no sack holds this (or sacks not scanned yet)".into(), DIM),
                        }
                        line(ui, "Filled, waiting in escrow to claim", grouped_u(escrow), if escrow > 0 { ORANGE } else { Color32::WHITE });
                        line(ui, "Open buy orders, not yet filled", format!("+{}", grouped_u(buys)), Color32::WHITE);
                        line(ui, "If every order fills you would hold", grouped_u(holding + incoming), if overflow > 0 { ORANGE } else { Color32::WHITE });
                        line(ui, "Inventory room left now", grouped_u(fits), Color32::WHITE);
                        line(ui, "Inventory room left if every order fills", grouped_u(room_after), if overflow > 0 { ORANGE } else { GREEN });
                        if overflow > 0 { line(ui, "Would not fit", format!("{} (stays in escrow unless sacks / the compactor take it)", grouped_u(overflow)), ORANGE); }
                        if sells > 0 { line(ui, "Sell offers still open", format!("{} (already out of your inventory)", grouped_u(sells)), DIM); }
                    });
                });
                { let rr_ = hdr_resp.response.rect; fx::row_bg(ui, egui::Id::new(("invrow", &id)), rr_, ui.rect_contains_pointer(rr_), g.recipe_open.contains(&id)); }
                if toggle { if !g.recipe_open.remove(&id) { g.recipe_open.insert(id.clone()); } }
                if g.recipe_open.contains(&id) {
                    let vals = [(holding, Color32::WHITE), (fits, DIM), (room_after, if overflow > 0 { ORANGE } else { Color32::WHITE }), (buys, if buys > 0 { GREEN } else { DIM })];
                    g.raw_row(ui, &id, w[0] - 22.0, 24.0, &w[1..], &vals);
                }
            }
        });
    }

    /// Every item a sack could hold, with how much of it is in the sacks and how much more would fit.
    fn sack_table(&mut self, ui: &mut egui::Ui, needle: &str) {
        if self.sacks.is_empty() {
            ui.label(RichText::new(if self.sacks_scanning { "Scanning the sacks in game…" } else { "No sack data yet: press \"Scan sacks\" (opens every sack once, ~45 s)." }).color(DIM));
            return;
        }
        let rows: Vec<_> = self.sack_rows().into_iter().filter(|r| needle.is_empty() || r.name.to_lowercase().contains(needle)).collect();
        if rows.is_empty() { ui.label(RichText::new(format!("No sack item matches \"{needle}\".")).color(DIM)); return; }
        ui.label(RichText::new(format!("{} item(s). Same-name sacks add their capacity.", rows.len())).color(DIM));
        let w = [230.0, 110.0, 100.0, 110.0, 100.0, 100.0, 120.0, 130.0, 120.0];
        fn cell(ui: &mut egui::Ui, width: f32, add: impl FnOnce(&mut egui::Ui)) {
            ui.allocate_ui_with_layout(egui::vec2(width, 22.0), egui::Layout::left_to_right(egui::Align::Center), |ui| { ui.set_min_width(width); add(ui); });
        }
        let heads = ["Item", "In inventory", "Room left", "If all fill", "Open buys", "In sacks", "Sack capacity", "Sack room left", "IAOF"];
        ui.horizontal(|ui| { for (t, wd) in heads.iter().zip(w) { cell(ui, wd, |ui| { let l = ui.label(RichText::new(*t).color(Color32::WHITE).strong()); if !head_tip(t).is_empty() { fx::tip(l, head_tip(t)); } }); } });
        ui.label(RichText::new("IAOF = sack room left minus everything still coming in (escrow + open buys): the room left if all current orders fill").color(DIM).size(14.0));
        ui.separator();
        for r in rows {
            let id = self.id_for_name(&r.name);
            let (held, fits, escrow, buys, _) = self.inv_figures(&id);
            let incoming = escrow + buys;
            let room_after = fits.saturating_sub(incoming);
            let mut toggle = false;
            let row_resp = ui.horizontal(|ui| {
                cell(ui, w[0], |ui| toggle = icon_toggle(ui, &self.icons, &id, &short(&r.name), self.recipe_open.contains(&id)));
                cell(ui, w[1], |ui| { ui.label(grouped_u(held)); });
                cell(ui, w[2], |ui| { ui.label(RichText::new(grouped_u(fits)).color(DIM)); });
                cell(ui, w[3], |ui| { ui.label(RichText::new(grouped_u(room_after)).color(if incoming > fits { ORANGE } else { Color32::WHITE })); });
                cell(ui, w[4], |ui| { ui.label(RichText::new(if buys > 0 { format!("+{}", grouped_u(buys)) } else { "—".into() }).color(if buys > 0 { GREEN } else { DIM })); });
                cell(ui, w[5], |ui| { ui.label(RichText::new(grouped_u(r.stored)).color(if r.stored > 0 { Color32::WHITE } else { DIM })); });
                cell(ui, w[6], |ui| { ui.label(grouped_u(r.cap)).fx_tip(r.sacks.join("\n")); });
                cell(ui, w[7], |ui| { ui.label(RichText::new(grouped_u(r.cap.saturating_sub(r.stored))).color(GREEN)); });
                let sack_room = r.cap.saturating_sub(r.stored);
                cell(ui, w[8], |ui| { ui.label(RichText::new(grouped_u(sack_room.saturating_sub(incoming))).color(if incoming > sack_room { ORANGE } else { Color32::WHITE })).fx_tip(if incoming > sack_room { format!("{} more is coming than the sacks can take", grouped_u(incoming - sack_room)) } else { String::new() }); });
            });
            { let rr_ = row_resp.response.rect; fx::row_bg(ui, egui::Id::new(("sackrow", &id)), rr_, ui.rect_contains_pointer(rr_), self.recipe_open.contains(&id)); };
            if toggle { if !self.recipe_open.remove(&id) { self.recipe_open.insert(id.clone()); } }
            if self.recipe_open.contains(&id) {
                let sack_room = r.cap.saturating_sub(r.stored);
                let vals = [(held, Color32::WHITE), (fits, DIM), (room_after, if incoming > fits { ORANGE } else { Color32::WHITE }), (buys, if buys > 0 { GREEN } else { DIM }),
                    (r.stored, Color32::WHITE), (r.cap, Color32::WHITE), (sack_room, GREEN), (sack_room.saturating_sub(incoming), if incoming > sack_room { ORANGE } else { Color32::WHITE })];
                self.raw_row(ui, &id, w[0], 0.0, &w[1..], &vals);
            }
        }
    }
}
