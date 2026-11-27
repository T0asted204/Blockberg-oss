//! Auto Flip: buy with an iceberg (top bid + offset, requoted on a timer or when outbid), flip every filled slice straight from escrow into
//! a sell offer (best ask - offset, never below a floor / margin), and re-list the sell offer when someone undercuts it. Then repeat.
//!
//! The buy leg is an ordinary iceberg job (auto-claim off, so slices stay in escrow); this engine watches the order list, sends `fliporder`
//! for fully filled slices and `requote_sell` for undercut sell offers. One agent action at a time, like everything else.
use super::*;
use crate::iceberg::{parse_qty, same_product, MAX_ORDER_QTY};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum FlipState { Running, Paused, Done, Stopped }
impl FlipState {
    pub fn label(self) -> &'static str { match self { FlipState::Running => "Running", FlipState::Paused => "Paused", FlipState::Done => "Done", FlipState::Stopped => "Stopped" } }
    pub fn from_label(l: &str) -> FlipState { [FlipState::Running, FlipState::Paused, FlipState::Done, FlipState::Stopped].into_iter().find(|s| s.label() == l).unwrap_or(FlipState::Paused) }
}

#[derive(Clone)]
pub struct FlipJob {
    pub id: u32, pub product: String, pub state: FlipState,
    // buy leg (one iceberg per cycle)
    pub total: u64, pub slice: u64, pub min_price: f64, pub max_price: f64, pub offset: f64, pub requote_secs: u64, pub on_outbid: bool, pub outbid_gap: u64,
    // sell leg
    pub sell_offset: f64, pub floor: f64, pub margin_pct: f64, pub tax_pct: f64, pub undercut: bool, pub undercut_gap: u64, pub requote_max_qty: u64,
    /// where the goods are sold: 0 = bazaar sell offers, 1 = the /trades menu (NPC, fixed price, no tax, counts towards the daily NPC limit)
    pub exchange: u8, pub backlog: u64, pub claim_ctx: Option<(u64, f64)>, pub sell_all: bool,
    /// the NPC daily limit leaves no room for even one stack: buying waits too (`paused_ice` = this engine paused the buy iceberg and must resume it)
    pub cap_blocked: bool, pub paused_ice: bool, pub escrow_hold: bool,
    // loop (0 cycles = until stopped)
    pub cycles: u32, pub cycles_done: u32, pub ice_id: u32,
    /// "Auto requote" on a sell from the Ticket: no buying, just one sell offer (`total` items) kept at the top of the book until everything is sold
    pub sell_only: bool, pub sell_placed: bool, pub seen_order: bool,
    /// the price our sell-only offer is listed at, as we last set it (the order list can be half a minute behind)
    pub my_price: f64,
    // what it has done
    pub flipped_units: u64, pub buy_cost: f64, pub list_value: f64, pub real_units: u64, pub real_coins: f64, pub sell_requotes: u32, pub fails: u32, pub note: String,
    // runtime only
    pub sell_at: Instant, pub clean_at: Instant, pub inv_sell_at: Instant, pub partial_fail_at: Instant, pub placed_at: Instant, pub state_at: Instant, pub pending: Option<(Instant, &'static str)>, pub seen: HashMap<(i64, u64), Instant>,
    /// cycles whose buying has started and that are not yet completely sold (they become history rows when they are)
    pub pending_cycles: Vec<PendingCycle>,
}

/// A cycle in progress: when it started, what was projected, and the totals at its start so its own share can be worked out later.
#[derive(Clone, Default)]
pub struct PendingCycle { pub n: u32, pub start_t: f64, pub proj_secs: f64, pub proj_buy: f64, pub base_flipped: u64, pub base_cost: f64, pub base_list: f64,
    pub buy_done_t: Option<f64>, pub cum: Option<(u64, f64, f64)> }
impl PendingCycle {
    fn to_json(&self) -> Value { json!({"n": self.n, "start": self.start_t, "proj": self.proj_secs, "proj_buy": self.proj_buy, "bf": self.base_flipped, "bc": self.base_cost, "bl": self.base_list, "buy_done": self.buy_done_t, "cum": self.cum.map(|c| json!([c.0, c.1, c.2]))}) }
    fn from_json(v: &Value) -> PendingCycle {
        PendingCycle { n: v["n"].as_u64().unwrap_or(0) as u32, start_t: v["start"].as_f64().unwrap_or(0.0), proj_secs: v["proj"].as_f64().unwrap_or(0.0), proj_buy: v["proj_buy"].as_f64().unwrap_or(0.0),
            base_flipped: v["bf"].as_u64().unwrap_or(0), base_cost: v["bc"].as_f64().unwrap_or(0.0), base_list: v["bl"].as_f64().unwrap_or(0.0), buy_done_t: v["buy_done"].as_f64(),
            cum: v["cum"].as_array().and_then(|a| Some((a.first()?.as_u64()?, a.get(1)?.as_f64()?, a.get(2)?.as_f64()?))) }
    }
}

/// A finished cycle: what was done, and how long it was projected to take next to how long it really took.
#[derive(Clone)]
pub struct FlipCycle { pub t: f64, pub id: u32, pub product: String, pub n: u32, pub units: u64, pub buy_avg: f64, pub list_avg: f64, pub profit: f64, pub proj_secs: f64, pub actual_secs: f64, pub proj_buy: f64, pub actual_buy: f64, pub v: u8 }
impl FlipCycle {
    pub fn to_json(&self) -> Value { json!({"t": self.t, "id": self.id, "product": self.product, "n": self.n, "units": self.units, "buy_avg": self.buy_avg, "list_avg": self.list_avg, "profit": self.profit, "proj": self.proj_secs, "actual": self.actual_secs, "proj_buy": self.proj_buy, "actual_buy": self.actual_buy, "v": self.v}) }
    pub fn from_json(v: &Value) -> FlipCycle {
        let f = |k: &str| v[k].as_f64().unwrap_or(0.0);
        FlipCycle { t: f("t"), id: v["id"].as_u64().unwrap_or(0) as u32, product: v["product"].as_str().unwrap_or("").into(), n: v["n"].as_u64().unwrap_or(0) as u32, units: v["units"].as_u64().unwrap_or(0),
            buy_avg: f("buy_avg"), list_avg: f("list_avg"), profit: f("profit"), proj_secs: f("proj"), actual_secs: f("actual"), proj_buy: f("proj_buy"), actual_buy: f("actual_buy"), v: v["v"].as_u64().unwrap_or(1) as u8 }
    }
}

/// How long a setup is projected to take. Buying fills at the item's 7-day insta-sell rate, selling at its insta-buy rate (units per hour); every slice also costs the agent
/// about 25 s to place and 25 s to flip. `curve` is cumulative profit over time for one cycle.
#[derive(Clone, Default)]
pub struct FlipEta { pub trades: bool, pub place_secs: f64, pub buy_secs: f64, pub flip_secs: f64, pub sell_secs: f64, pub total_secs: f64, pub slices: u64, pub net_per_unit: f64, pub curve: Vec<(f64, f64)>, pub gap_secs: f64 }
pub const PLACE_SECS: f64 = 25.0;
/// Measured: 5,000 Enchanted Hard Stone went to the NPC in 43 s (including pulling them from the sacks); claiming a slice takes about 25 s.
pub const TRADES_UNITS_PER_SEC: f64 = 140.0;
pub const FLIP_SECS: f64 = 25.0;
/// A partly filled slice is claimed once this many items are waiting in escrow.
pub const PARTIAL_CLAIM: u64 = 1280;
/// Buying pauses while more than this many items wait in escrow (plus the backlog to sell) and carries on below the lower number.
pub const ESCROW_HOLD_ON: u64 = 25_000;
/// This many open sell offers of one item are merged into one.
pub const MERGE_AT: usize = 3;
/// A partly filled bazaar buy order is flipped into a sell offer once its filled part is worth this much (and is a quarter of the order).
pub const PARTIAL_FLIP_COINS: f64 = 1_000_000.0;
/// The shortest wait between two re-lists of an auto-requote sell offer (one re-list takes the agent about 7 s).
pub const SELL_REQUOTE_GAP: u64 = 5;
pub const ESCROW_HOLD_OFF: u64 = 10_000;

/// Older history rows of overlapping cycles were wrong: each row counted the units, profit and time of every cycle after it too (cycle 1 of a 30-cycle flip showed the whole flip).
/// Every flip's rows are subtracted from the next row to get the cycle's own numbers, and the time becomes the gap between this cycle's start and the next one's.
pub fn repair_legacy_cycles(rows: &mut Vec<FlipCycle>) {
    let ids: Vec<u32> = { let mut v: Vec<u32> = rows.iter().filter(|c| c.v < 2).map(|c| c.id).collect(); v.sort(); v.dedup(); v };
    for id in ids {
        let mut idx: Vec<usize> = (0..rows.len()).filter(|&k| rows[k].id == id && rows[k].v < 2).collect();
        idx.sort_by_key(|&k| rows[k].n);
        let old: Vec<FlipCycle> = idx.iter().map(|&k| rows[k].clone()).collect();
        let mono = old.windows(2).all(|w| w[0].units >= w[1].units);
        for (a, &k) in idx.iter().enumerate() {
            let c = &old[a];
            if a + 1 < old.len() && mono && old.len() > 1 {
                let nx = &old[a + 1];
                let (u, cost, list) = (c.units - nx.units, c.buy_avg * c.units as f64 - nx.buy_avg * nx.units as f64, c.list_avg * c.units as f64 - nx.list_avg * nx.units as f64);
                if u > 0 { rows[k].units = u; rows[k].buy_avg = cost / u as f64; rows[k].list_avg = list / u as f64; rows[k].profit = c.profit - nx.profit; } else { rows[k].units = 0; }
                let (s0, s1) = (c.t - c.actual_secs, nx.t - nx.actual_secs);
                if s1 > s0 { rows[k].actual_secs = s1 - s0; rows[k].t = s1; }   // finished: when the next cycle started (the best the old data allows)
            }
            rows[k].v = 2;
        }
    }
    rows.retain(|c| c.units > 0);   // a cycle whose flips all happened after the next one started has no numbers of its own: they sit in the next row
}

pub fn fmt_eta(secs: f64) -> String {
    if !secs.is_finite() { return "—".into(); }
    if secs < 90.0 { format!("{secs:.0} s") } else if secs < 5400.0 { format!("{:.0} min", secs / 60.0) } else if secs < 48.0 * 3600.0 { format!("{:.1} h", secs / 3600.0) } else { format!("{:.1} d", secs / 86400.0) }
}

impl FlipJob {
    pub fn new(product: String) -> Self {
        let now = Instant::now();
        FlipJob { id: 0, product, state: FlipState::Running, total: 0, slice: MAX_ORDER_QTY, min_price: 0.1, max_price: 0.0, offset: 0.1, requote_secs: 300, on_outbid: false, outbid_gap: 30,
            sell_offset: 0.1, floor: 0.0, margin_pct: 2.0, tax_pct: 1.25, undercut: true, undercut_gap: 60, requote_max_qty: 2000, exchange: 0, backlog: 0, claim_ctx: None, sell_all: false, cap_blocked: false, paused_ice: false, escrow_hold: false, cycles: 1, cycles_done: 0, ice_id: 0, sell_only: false, sell_placed: false, seen_order: false, my_price: 0.0,
            flipped_units: 0, buy_cost: 0.0, list_value: 0.0, real_units: 0, real_coins: 0.0, sell_requotes: 0, fails: 0, note: String::new(), sell_at: now, clean_at: now, inv_sell_at: now - Duration::from_secs(600), partial_fail_at: now - Duration::from_secs(600), placed_at: now, state_at: now, pending: None, seen: HashMap::new(), pending_cycles: vec![] }
    }
    pub fn est_profit(&self) -> f64 { self.list_value - self.buy_cost }
    /// The profit the whole flip is expected to make: the profit per unit so far times every unit planned (size x cycles). A finished or stopped flip is what it did.
    /// A flip with no end (0 cycles) has no total, so it shows what it has listed so far. None until the first claim has been listed.
    pub fn est_total(&self) -> Option<f64> {
        if self.flipped_units == 0 { return None; }
        let now = self.est_profit();
        match self.state {
            FlipState::Done | FlipState::Stopped => Some(now),
            _ if self.cycles == 0 => Some(now),
            _ => Some((now / self.flipped_units as f64) * (self.total as f64 * self.cycles as f64).max(self.flipped_units as f64)),
        }
    }
    /// What was really earned: the coins the sales actually paid, minus what the sold units cost on average. None until a sale of ours has been counted.
    /// (Only sales to the NPC are counted: the coins come from the sale itself. Bazaar sell offers are not tracked.)
    pub fn realized(&self) -> Option<f64> {
        if self.real_units == 0 || self.flipped_units == 0 { return None; }
        Some(self.real_coins - self.real_units as f64 * (self.buy_cost / self.flipped_units as f64))
    }
    pub fn to_json(&self) -> Value {
        json!({"id": self.id, "product": self.product, "state": self.state.label(), "total": self.total, "slice": self.slice, "min": self.min_price, "max": self.max_price, "offset": self.offset,
            "requote": self.requote_secs, "on_outbid": self.on_outbid, "outbid_gap": self.outbid_gap, "sell_offset": self.sell_offset, "floor": self.floor, "margin": self.margin_pct,
            "tax": self.tax_pct, "exchange": self.exchange, "backlog": self.backlog, "sell_all": self.sell_all, "paused_ice": self.paused_ice, "undercut": self.undercut, "undercut_gap": self.undercut_gap, "requote_max_qty": self.requote_max_qty, "cycles": self.cycles, "cycles_done": self.cycles_done,
            "ice_id": self.ice_id, "sell_only": self.sell_only, "sell_placed": self.sell_placed, "seen_order": self.seen_order, "my_price": self.my_price, "pc": self.pending_cycles.iter().map(|c| c.to_json()).collect::<Vec<_>>(), "flipped": self.flipped_units, "buy_cost": self.buy_cost, "list_value": self.list_value, "real_units": self.real_units, "real_coins": self.real_coins, "sell_requotes": self.sell_requotes, "fails": self.fails, "note": self.note})
    }
    pub fn from_json(v: &Value) -> FlipJob {
        let mut j = FlipJob::new(v["product"].as_str().unwrap_or("").into());
        let f = |k: &str, d: f64| v[k].as_f64().unwrap_or(d);
        let u = |k: &str, d: u64| v[k].as_u64().unwrap_or(d);
        j.id = u("id", 0) as u32; j.state = FlipState::from_label(v["state"].as_str().unwrap_or(""));
        j.total = u("total", 0); j.slice = u("slice", MAX_ORDER_QTY); j.min_price = f("min", 0.1); j.max_price = f("max", 0.0); j.offset = f("offset", 0.1); j.requote_secs = u("requote", 300);
        j.on_outbid = v["on_outbid"].as_bool().unwrap_or(false); j.outbid_gap = u("outbid_gap", 30);
        j.sell_offset = f("sell_offset", 0.1); j.floor = f("floor", 0.0); j.margin_pct = f("margin", 2.0); j.tax_pct = f("tax", 1.25);
        j.undercut = v["undercut"].as_bool().unwrap_or(true); j.undercut_gap = u("undercut_gap", 60); j.requote_max_qty = u("requote_max_qty", 2000);
        j.exchange = u("exchange", 0).min(1) as u8; j.backlog = u("backlog", 0); j.sell_all = v["sell_all"].as_bool().unwrap_or(false); j.paused_ice = v["paused_ice"].as_bool().unwrap_or(false);
        j.cycles = u("cycles", 1) as u32; j.cycles_done = u("cycles_done", 0) as u32; j.ice_id = u("ice_id", 0) as u32; j.sell_only = v["sell_only"].as_bool().unwrap_or(false); j.sell_placed = v["sell_placed"].as_bool().unwrap_or(false); j.seen_order = v["seen_order"].as_bool().unwrap_or(false); j.my_price = v["my_price"].as_f64().unwrap_or(0.0);
        j.flipped_units = u("flipped", 0); j.buy_cost = f("buy_cost", 0.0); j.list_value = f("list_value", 0.0); j.real_units = u("real_units", 0); j.real_coins = f("real_coins", 0.0); j.sell_requotes = u("sell_requotes", 0) as u32; j.fails = u("fails", 0) as u32;
        j.note = v["note"].as_str().unwrap_or("").into();
        j.pending_cycles = v["pc"].as_array().map(|a| a.iter().map(PendingCycle::from_json).collect()).unwrap_or_default();
        j
    }
}

#[derive(Clone)]
pub struct FlipForm {
    pub product: String, pub total: String, pub slice: String, pub min: String, pub max: String, pub offset: String, pub requote: String, pub on_outbid: bool, pub gap: String,
    pub sell_offset: String, pub floor: String, pub margin: String, pub tax: String, pub undercut: bool, pub undercut_gap: String, pub requote_max_qty: String, pub cycles: String,
    pub buy_mode: u8, pub floor_on: bool, pub exchange: u8, pub sell_all: bool,
    pub msg: Option<(bool, String)>,
}
impl FlipForm {
    pub fn new() -> Self {
        FlipForm { product: String::new(), total: String::new(), slice: MAX_ORDER_QTY.to_string(), min: "0.1".into(), max: String::new(), offset: "0.1".into(), requote: "5".into(), on_outbid: true, gap: "30".into(),
            sell_offset: "0.1".into(), floor: String::new(), margin: "2".into(), tax: "1.25".into(), undercut: true, undercut_gap: "60".into(), requote_max_qty: "2000".into(), cycles: "1".into(), buy_mode: 0, floor_on: false, exchange: 0, sell_all: false, msg: None }
    }
}

/// One line of the history: a buy that was flipped, or a sell offer listed / re-listed.
#[derive(Clone)]
pub struct FlipEvent { pub t: f64, pub id: u32, pub product: String, pub side: &'static str, pub units: u64, pub price: f64 }
impl FlipEvent {
    pub fn to_json(&self) -> Value { json!({"t": self.t, "id": self.id, "product": self.product, "side": self.side, "units": self.units, "price": self.price}) }
    pub fn from_json(v: &Value) -> FlipEvent {
        FlipEvent { t: v["t"].as_f64().unwrap_or(0.0), id: v["id"].as_u64().unwrap_or(0) as u32, product: v["product"].as_str().unwrap_or("").into(),
            side: if v["side"].as_str() == Some("SELL") { "SELL" } else { "BUY" }, units: v["units"].as_u64().unwrap_or(0), price: v["price"].as_f64().unwrap_or(0.0) }
    }
}

/// Cumulative projected profit at time `t` for `n_cyc` cycles that each start `gap` seconds after the last: cycle n contributes `curve` shifted by n * gap.
/// Cycles that are already complete add their full profit at once, so this stays cheap for any number of cycles.
pub fn projected_profit(curve: &[(f64, f64)], gap: f64, total: f64, n_cyc: usize, t: f64) -> f64 {
    if t <= 0.0 || n_cyc == 0 || curve.is_empty() { return 0.0; }
    let gap = gap.max(1.0);
    let at = |x: f64| -> f64 {
        if x <= 0.0 { return 0.0; }
        let k = curve.partition_point(|p| p.0 < x);
        if k == 0 { 0.0 } else if k >= curve.len() { curve.last().map_or(0.0, |p| p.1) } else { let (a, b) = (curve[k - 1], curve[k]); a.1 + (b.1 - a.1) * ((x - a.0) / (b.0 - a.0).max(1e-9)) }
    };
    let started = ((t / gap).floor() as usize).min(n_cyc - 1);                              // cycles 0..=started have begun
    let done = if t >= total { (((t - total) / gap).floor() as usize + 1).min(started + 1) } else { 0 };   // cycles 0..done are complete
    let full = curve.last().map_or(0.0, |p| p.1);
    done as f64 * full + (done..=started).map(|n| at(t - n as f64 * gap)).sum::<f64>()
}

/// Round up to the bazaar's 0.1 price step.
fn up1(x: f64) -> f64 { (x * 10.0 - 1e-9).ceil() / 10.0 }
fn near1(x: f64) -> f64 { (x * 10.0).round() / 10.0 }

impl Gui {
    // ---------------------------------------------------------------- pricing
    /// Where a slice bought at `buy_price` should be listed: best ask - offset, but never below the floor or the minimum margin (after tax).
    /// The lowest sell offer that is not ours: our own open offers for this item are subtracted level by level, so our offers never count as
    /// "someone undercutting" (which would make the bot chase its own prices down). 0.0 if nobody else is selling.
    pub fn flip_other_ask(&self, product: &str) -> f64 {
        let Some(p) = self.products.get(product) else { return 0.0 };
        // ours and our co-op partner's offers do not count: undercutting our own partner only lowers what we both get
        let mine: Vec<(f64, u64)> = self.orders.iter().chain(self.coop_orders.iter()).filter(|r| r.side == "SELL" && same_product(&r.product, product) && !(r.total > 0 && r.filled >= r.total))
            .map(|r| (r.price.unwrap_or(0.0), r.total.saturating_sub(r.filled))).collect();
        for lvl in &p.buy_summary {
            let ours: u64 = mine.iter().filter(|(pr, _)| (pr - lvl.price).abs() < 0.05).map(|m| m.1).sum();
            if lvl.amount > ours { return lvl.price; }
        }
        0.0
    }

    pub fn flip_sell_price(&self, j: &FlipJob, buy_price: f64) -> f64 {
        let other = self.flip_other_ask(&j.product);
        let ask = if other > 0.0 { other } else { self.products.get(&j.product).map_or(0.0, |p| p.ask()) };
        let min_margin = if buy_price > 0.0 { up1(buy_price * (1.0 + j.margin_pct / 100.0) / (1.0 - j.tax_pct / 100.0)) } else { 0.0 };
        let floor = up1(j.floor).max(min_margin);
        let target = if ask > 0.0 { near1(ask - j.sell_offset) } else { 0.0 };
        near1(target.max(floor)).max(0.1)
    }

    /// The projection for a setup (see FlipEta): None while prices are missing or the item has no trading volume.
    pub fn flip_eta(&self, j: &FlipJob) -> Option<FlipEta> {
        let p = self.products.get(&j.product)?;
        let (bid, ask) = (p.bid(), p.ask());
        if bid <= 0.0 || ask <= 0.0 || j.total == 0 { return None; }
        let r_buy = p.quick_status.sell_moving_week as f64 / 168.0 / 3600.0;    // units per second that fill buy orders
        let r_sell = p.quick_status.buy_moving_week as f64 / 168.0 / 3600.0;    // units per second that fill sell offers
        let trades = j.exchange == 1;
        if r_buy <= 0.0 || (!trades && r_sell <= 0.0) { return None; }
        let buy = (bid + j.offset).min(j.max_price);
        let sell = if trades { npc_sell_of(&j.product).filter(|v| *v > 0.0)? } else { self.flip_sell_price(j, buy) };
        let net = sell * (1.0 - j.tax_pct / 100.0) - buy;
        let slice = j.slice.max(1);
        let slices = j.total.div_ceil(slice);
        // when each slice is listed: it must fill, then the agent places / flips one thing at a time
        let mut listed: Vec<(f64, f64)> = vec![];
        let (mut agent_free, mut t_buy) = (0.0f64, 0.0f64);
        for k in 1..=slices {
            let units = (k * slice).min(j.total);
            let fill = units as f64 / r_buy + k as f64 * PLACE_SECS;
            t_buy = fill;
            let start = fill.max(agent_free);
            agent_free = start + FLIP_SECS;
            listed.push((agent_free, (units - (k - 1) * slice) as f64));
        }
        if trades {
            // each slice: claim it, then sell it to the NPC round by round; the agent does one thing at a time
            let (mut agent_free, mut curve, mut sell_total) = (0.0f64, vec![(0.0, 0.0)], 0.0f64);
            for k in 1..=slices {
                let units = (k * slice).min(j.total) - (k - 1) * slice;
                let fill = (k * slice).min(j.total) as f64 / r_buy + k as f64 * PLACE_SECS;
                let start = fill.max(agent_free);
                let dur = FLIP_SECS + units as f64 / TRADES_UNITS_PER_SEC;
                curve.push((start, curve.last().map_or(0.0, |c| c.1)));
                agent_free = start + dur; sell_total += units as f64 / TRADES_UNITS_PER_SEC;
                curve.push((agent_free, k as f64 * 0.0 + ((k * slice).min(j.total)) as f64 * net));
            }
            return Some(FlipEta { trades: true, place_secs: slices as f64 * PLACE_SECS, buy_secs: t_buy, flip_secs: slices as f64 * FLIP_SECS, sell_secs: sell_total, total_secs: agent_free, slices, net_per_unit: net, curve, gap_secs: t_buy });
        }
        // selling: offers fill at r_sell in total, oldest first
        let (mut t, mut sold, mut listed_units, mut idx) = (0.0f64, 0.0f64, 0.0f64, 0usize);
        let horizon = 14.0 * 86400.0;
        let mut step = ((listed.last().map_or(60.0, |l| l.0) + j.total as f64 / r_sell) / 500.0).clamp(10.0, 6.0 * 3600.0);
        let mut curve = vec![(0.0, 0.0)];
        let mut done_at = None;
        while t < horizon {
            t += step;
            while idx < listed.len() && listed[idx].0 <= t { listed_units += listed[idx].1; idx += 1; }
            sold = (sold + r_sell * step).min(listed_units);
            curve.push((t, sold * net));
            if sold >= j.total as f64 - 0.5 { done_at = Some(t); break; }
            if curve.len() > 2000 { step *= 2.0; }
        }
        let total = done_at?;
        let first_list = listed.first().map_or(0.0, |l| l.0);
        Some(FlipEta { trades: false, place_secs: slices as f64 * PLACE_SECS, buy_secs: t_buy, flip_secs: slices as f64 * FLIP_SECS, sell_secs: (total - first_list).max(0.0), total_secs: total, slices, net_per_unit: net, curve, gap_secs: t_buy })
    }

    // ---------------------------------------------------------------- persistence
    pub fn flip_json(&self) -> String { json!({"ver": 2, "next_id": self.next_flip_id, "flips": self.flips.iter().map(|j| j.to_json()).collect::<Vec<_>>(), "log": self.flip_log.iter().map(|e| e.to_json()).collect::<Vec<_>>(), "cycles": self.flip_cycles.iter().map(|c| c.to_json()).collect::<Vec<_>>()}).to_string() }
    pub fn flip_save(&mut self) {
        if self.mock || self.is_client() { return; }
        let text = self.flip_json();
        if text == self.flip_saved { return; }
        if let Some(p) = Some(bb::profile::data_dir().join("autoflip_jobs.json")) {
            if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
            let tmp = p.with_extension("tmp");
            if std::fs::write(&tmp, &text).is_ok() { let _ = std::fs::rename(&tmp, &p); }
        }
        self.flip_saved = text;
    }
    pub fn flip_load() -> (Vec<FlipJob>, u32, Vec<FlipEvent>, Vec<FlipCycle>) {
        let v = std::fs::read_to_string(bb::profile::data_dir().join("autoflip_jobs.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
        let Some(v) = v else { return (vec![], 1, vec![], vec![]) };
        (v["flips"].as_array().map(|a| a.iter().map(FlipJob::from_json).collect()).unwrap_or_default(), v["next_id"].as_u64().unwrap_or(1) as u32,
            v["log"].as_array().map(|a| a.iter().map(FlipEvent::from_json).collect()).unwrap_or_default(),
            v["cycles"].as_array().map(|a| a.iter().map(FlipCycle::from_json).collect()).unwrap_or_default())
    }

    // ---------------------------------------------------------------- user actions (a client forwards them to the server)
    pub fn flip_add(&mut self, mut job: FlipJob) {
        if self.is_client() {
            let mut v = job.to_json(); v["op"] = json!("flip_add");
            if self.remote_op(v) { let _ = self.toast_new(1, format!("Auto flip sent to the server: {}", pretty_s(&job.product))); }
            return;
        }
        if self.flips.iter().any(|f| f.product == job.product && matches!(f.state, FlipState::Running | FlipState::Paused)) {
            let _ = self.toast_new(2, format!("There is already an auto flip running for {}", pretty_s(&job.product)));
            return;
        }
        job.id = self.next_flip_id; self.next_flip_id += 1;
        self.say(ORANGE, format!("→ auto flip #{}: {} ({} per cycle, {})", job.id, pretty_s(&job.product), grouped(job.total), if job.cycles == 0 { "until stopped".to_string() } else { format!("{} cycle(s)", job.cycles) }));
        self.flips.push(job);
        self.flip_save();
    }

    pub fn flip_op(&mut self, id: u32, what: &str) {
        if self.is_client() { self.remote_op(json!({"op": "flip", "do": what, "id": id})); return; }
        let Some(i) = self.flips.iter().position(|j| j.id == id) else { return };
        match what {
            "pause" => if self.flips[i].state == FlipState::Running { self.flips[i].state = FlipState::Paused; self.flips[i].note = "paused: resting orders stay in game".into(); },
            "resume" => if self.flips[i].state == FlipState::Paused { let j = &mut self.flips[i]; j.state = FlipState::Running; j.note.clear(); j.fails = 0; j.state_at = Instant::now(); },
            "stop" => {
                let ice = self.flips[i].ice_id;
                if let Some(k) = self.jobs.iter().position(|x| x.id == ice && ice != 0) { self.ice_stop(k); }
                let j = &mut self.flips[i]; j.state = FlipState::Stopped; j.note = "stopped: sell offers already listed stay in game".into();
            }
            "remove" => if matches!(self.flips[i].state, FlipState::Done | FlipState::Stopped) { self.flips.remove(i); },
            _ => {}
        }
        self.flip_save();
    }

    // ---------------------------------------------------------------- the engine
    fn flip_start_cycle(&mut self, i: usize) {
        let j = self.flips[i].clone();
        let mut ice = IceJob::new(0, j.product.clone(), j.total, j.slice, j.min_price, j.max_price, j.offset, j.requote_secs);
        ice.auto_claim = false; ice.on_outbid = j.on_outbid; ice.outbid_gap = j.outbid_gap;
        self.ice_add(ice);
        let id = self.next_job_id - 1;
        let j = &mut self.flips[i];
        j.ice_id = id; j.state_at = Instant::now();
        j.note = format!("cycle {}: buying {} with iceberg #{id}", j.cycles_done + 1, grouped(j.total));
        // remember what this cycle was projected to take (for the history)
        let (proj_total, proj_buy) = self.flip_eta(&self.flips[i]).map_or((0.0, 0.0), |e| (e.total_secs, e.buy_secs));
        let j = &mut self.flips[i];
        let pc = PendingCycle { n: j.cycles_done + 1, start_t: now_secs(), proj_secs: proj_total, proj_buy, base_flipped: j.flipped_units, base_cost: j.buy_cost, base_list: j.list_value, buy_done_t: None, cum: None };
        j.pending_cycles.push(pc);
    }

    pub fn flip_tick(&mut self) {
        if self.is_client() { return; }
        if self.flip_cycles.iter().any(|c| c.v < 2) { repair_legacy_cycles(&mut self.flip_cycles); }
        if self.flip_next.elapsed() < Duration::from_secs(2) { return; }
        self.flip_next = Instant::now();
        let mut sent = false;
        for i in 0..self.flips.len() {
            if self.flips[i].state != FlipState::Running { continue; }
            if self.flips[i].sell_only { if !sent { sent = self.flip_sell_only(i); } continue; }
            self.flip_cap_sync(i);
            self.flip_buy_leg(i);
            if !sent && self.flips[i].state == FlipState::Running { sent = self.flip_act(i); }
            if !sent && self.flips[i].state == FlipState::Running { sent = self.flip_clean_orphans(i); }
            self.flip_check_done(i);
            self.flip_finish_cycles(i);
        }
        self.flip_save();
    }

    /// While the NPC daily limit leaves no room for even one stack, buying waits too (otherwise goods pile up in escrow that cannot be sold until the reset):
    /// the live buy iceberg is paused, and resumed when there is room again (the UTC day rolled over).
    fn flip_cap_sync(&mut self, i: usize) {
        let j = &self.flips[i];
        let blocked = j.exchange == 1 && npc_sell_of(&j.product).filter(|v| *v > 0.0).map_or(false, |npc| NPC_SELL_CAP - self.npc_sold_today() < npc * 64.0);
        if blocked != j.cap_blocked {
            self.flips[i].cap_blocked = blocked;
            self.say(if blocked { ORANGE } else { GREEN }, if blocked { format!("auto flip #{}: the NPC daily limit is used up: buying and selling wait for the next UTC day", self.flips[i].id) } else { format!("auto flip #{}: the NPC limit has room again: carrying on", self.flips[i].id) });
        }
        // Buying outruns claiming and selling when the inventory is small: once a lot of bought goods sit in escrow, no new slice is placed until it has gone down
        // (the order that is resting keeps filling). Only judged on an order list that is not old.
        if self.flips[i].exchange == 1 && self.orders_at.map_or(false, |t| t.elapsed() < Duration::from_secs(150)) {
            let (product, backlog) = (self.flips[i].product.clone(), self.flips[i].backlog);
            let esc: u64 = self.orders.iter().filter(|r| r.side == "BUY" && same_product(&r.product, &product) && r.claimable).map(|r| r.to_claim).sum::<u64>() + backlog;
            let hold = if self.flips[i].escrow_hold { esc > ESCROW_HOLD_OFF } else { esc > ESCROW_HOLD_ON };
            if hold != self.flips[i].escrow_hold {
                self.flips[i].escrow_hold = hold;
                self.say(if hold { ORANGE } else { GREEN }, if hold { format!("auto flip #{}: {} items are waiting to be claimed and sold: no new buy slice until that is below {}", self.flips[i].id, grouped(esc), grouped(ESCROW_HOLD_OFF)) } else { format!("auto flip #{}: escrow is down to {}: buying carries on", self.flips[i].id, grouped(esc)) });
            }
        }
        let hold = self.flips[i].escrow_hold;
        let ice = self.flips[i].ice_id;
        let k = self.jobs.iter().position(|x| x.id == ice && ice != 0);
        if blocked || hold {
            if let Some(k) = k { if matches!(self.jobs[k].state, iceberg::JobState::Idle | iceberg::JobState::Placing | iceberg::JobState::Resting) { self.ice_pause(k); self.flips[i].paused_ice = true; self.jobs[k].note = if blocked { "paused: the NPC daily limit is used up (resumes by itself)".into() } else { "paused: too much is waiting in escrow (resumes by itself)".into() }; } }
            if blocked && self.flips[i].paused_ice && self.flips[i].note.is_empty() { self.flips[i].note = "waiting for the NPC daily limit to reset".into(); }
        } else if self.flips[i].paused_ice {
            if let Some(k) = k { if self.jobs[k].state == iceberg::JobState::Paused { self.ice_resume(k); } }
            self.flips[i].paused_ice = false;
        }
    }

    /// The market bid of this item and the prices of the buy orders an iceberg of it is working on.
    fn flip_bid_and_live(&self, product: &str) -> (f64, Vec<f64>) {
        let bid = self.products.get(product).and_then(|p| p.sell_summary.first()).map_or(0.0, |l| l.price);
        let live = self.jobs.iter().filter(|x| same_product(&x.product, product) && x.cur_price > 0.0).map(|x| x.cur_price).collect();
        (bid, live)
    }

    /// Buy orders of this item that nothing manages any more and that cannot fill again (the market bid is well above them) lock their coins for good, and pile up
    /// in the order list. They are cancelled (a few per round, never the iceberg's own order, never one with goods waiting to be claimed).
    fn flip_clean_orphans(&mut self, i: usize) -> bool {
        let j = &self.flips[i];
        if j.clean_at.elapsed() < Duration::from_secs(90) || !self.agent_up || self.halted || self.draining || !self.inflight.is_empty() || j.pending.is_some()
            || self.busy(&["cancel", "claimorder", "fliporder", "requote_sell", "tradesellqty", "buyorder", "requote", "insert_sacks"]) { return false; }
        if !self.orders_at.map_or(false, |t| t.elapsed() < Duration::from_secs(150)) { return false; }
        let product = j.product.clone();
        let jobs: Vec<&iceberg::IceJob> = self.jobs.iter().filter(|x| same_product(&x.product, &product)).collect();
        if jobs.iter().any(|x| matches!(x.state, iceberg::JobState::Placing | iceberg::JobState::Requoting | iceberg::JobState::Adopting)) { return false; }
        let bid = self.products.get(&product).and_then(|p| p.sell_summary.first()).map_or(0.0, |l| l.price);
        if bid <= 0.0 { return false; }
        let live: Vec<f64> = jobs.iter().filter(|x| x.cur_price > 0.0).map(|x| x.cur_price).collect();
        let stale: Vec<OrderRow> = self.orders.iter().filter(|r| r.side == "BUY" && same_product(&r.product, &product) && r.total > 0 && r.filled < r.total && r.to_claim == 0
            && r.price.map_or(false, |p| p < bid - 1.0 && !live.iter().any(|l| (l - p).abs() < 0.15))).take(8).cloned().collect();
        self.flips[i].clean_at = Instant::now();
        if stale.is_empty() { return false; }
        let list: Vec<Value> = stale.iter().map(|r| json!({"product": r.product, "side": "BUY", "price": r.price})).collect();
        let _ = self.cmd_tx.send(json!({"cmd": "cancel", "auto": true, "orders": list}));
        let _ = self.cmd_tx.send(json!({"cmd": "orders", "auto": true})); self.orders_at = Some(Instant::now());
        self.say(ORANGE, format!("→ auto flip #{}: cancelling {} old buy order(s) of {} that the market has moved above ({})", self.flips[i].id, stale.len(), pretty_s(&product),
            stale.iter().map(|r| format!("{:.1}", r.price.unwrap_or(0.0))).collect::<Vec<_>>().join(", ")));
        true
    }

    /// The Ticket's "Auto requote" on a sell: place one sell offer for everything, and whenever someone lists below it, re-list at their price minus 0.1 (never below the floor)
    /// until the offer is gone because it all sold.
    fn flip_sell_only(&mut self, i: usize) -> bool {
        if !self.agent_up || self.halted || self.draining || !self.inflight.is_empty() || self.busy(&["selloffer", "requote_sell", "fliporder", "claimorder"]) { return false; }
        if let Some((t, what)) = self.flips[i].pending {
            if t.elapsed() < Duration::from_secs(180) { return false; }
            let j = &mut self.flips[i]; j.pending = None; j.note = format!("no answer to the {what}: looking again");
        }
        let (product, now) = (self.flips[i].product.clone(), Instant::now());
        let open: Vec<OrderRow> = self.orders.iter().filter(|r| r.side == "SELL" && same_product(&r.product, &product) && !(r.total > 0 && r.filled >= r.total)).cloned().collect();
        let fresh = self.orders_at.map_or(false, |t| t.elapsed() < Duration::from_secs(150));
        let r1 = |x: f64| (x * 10.0).round() / 10.0;
        if open.is_empty() && !self.flips[i].sell_placed {
            let (floor, qty) = (self.flips[i].floor, self.flips[i].total);
            let ask = self.flip_other_ask(&product);
            let ask = if ask > 0.0 { ask } else { self.products.get(&product).map_or(0.0, |p| p.ask()) };
            let price = r1((ask - self.flips[i].sell_offset).max(floor)).max(0.1);
            self.source_sacks = true;
            self.dispatch(Order { cmd: "selloffer", id: product.clone(), qty, unit: price, kind: Kind::Limit });
            let j = &mut self.flips[i];
            j.sell_placed = true; j.placed_at = now; j.seen_order = false; j.sell_at = now; j.my_price = price; j.note = format!("listing {} @ {:.1}", grouped(qty), price);
            self.flip_save();
            return true;
        }
        if !open.is_empty() {
            let j = &mut self.flips[i];
            j.seen_order = true; j.sell_placed = true;   // an offer of ours is up (also when it was listed by hand): never list a second one
            if j.my_price <= 0.0 { j.my_price = open.iter().filter_map(|r| r.price).fold(f64::INFINITY, f64::min); }
            let left: u64 = open.iter().map(|r| r.total.saturating_sub(r.filled)).sum();
            j.note = format!("{} left, listed @ {:.1}", grouped(left), j.my_price);
        } else if fresh && self.flips[i].seen_order && now.duration_since(self.flips[i].placed_at) > Duration::from_secs(40) && now.duration_since(self.flips[i].sell_at) > Duration::from_secs(40) {
            let j = &mut self.flips[i];
            j.state = FlipState::Done; j.note = "everything is sold".into();
            let (id, n) = (j.id, pretty_s(&product)); self.say(GREEN, format!("auto requote #{id} ({n}): everything is sold"));
            self.flip_save();
            return false;
        } else if fresh && !self.flips[i].seen_order && now.duration_since(self.flips[i].placed_at) > Duration::from_secs(150) {
            let j = &mut self.flips[i]; j.state = FlipState::Stopped; j.note = "the sell offer never showed up in the order list: check the log".into();
            self.flip_save();
            return false;
        }
        // Undercut? Judged from the live prices and the price we know we listed at, not from the order list (which can be half a minute old): as soon as anyone
        // else's offer is below ours, re-list just under it (never below the floor).
        let ours = self.flips[i].my_price;
        if ours > 0.0 && self.flips[i].sell_at.elapsed() >= Duration::from_secs(self.flips[i].undercut_gap) {
            let ask = self.flip_other_ask(&product);   // the lowest offer that is not ours
            let (floor, off) = (self.flips[i].floor, self.flips[i].sell_offset);
            if ask > 0.0 && ask < ours - 0.05 {
                let new_price = r1((ask - off).max(floor)).max(0.1);
                if new_price < ours - 0.05 {
                    let _ = self.cmd_tx.send(json!({"cmd": "requote_sell", "auto": true, "product": product, "price": ours, "new_price": new_price}));
                    self.say(ORANGE, format!("→ auto requote #{}: {} undercut (ask {:.1}): re-list {:.1} → {:.1}", self.flips[i].id, pretty_s(&product), ask, ours, new_price));
                    let j = &mut self.flips[i]; j.pending = Some((now, "sell requote")); j.sell_at = now;
                    return true;
                }
                let j = &mut self.flips[i]; j.sell_at = now; j.note = format!("undercut at {:.1}, but the floor is {:.1}: holding", ask, floor);
            }
        }
        false
    }

    /// The Ticket's "Auto requote" button. Buy: an iceberg of the whole quantity that re-quotes whenever someone outbids it, until everything is bought.
    /// Sell: a sell-only job (see `flip_sell_only`) with the top bid as its floor.
    pub fn ticket_auto_requote(&mut self, id: &str, buy: bool, qty: u64) {
        let Some(p) = self.products.get(id).cloned() else { return };
        if qty == 0 { let _ = self.toast_new(2, "Auto requote: nothing to do (quantity 0)"); return; }
        if buy {
            let mut f = IceForm::new();
            f.product = id.to_string(); f.total = qty.to_string(); f.slice = qty.min(MAX_ORDER_QTY).to_string();
            let cap = if p.ask() > 0.0 { p.ask() } else { p.bid() * 1.2 };
            f.max = fmt_step((cap * 10.0).ceil() / 10.0);
            f.requote = "5".into(); f.on_outbid = true; f.gap = "15".into(); f.auto_claim = true;
            self.ice_form = f;
            self.ice_submit();
        } else {
            let mut j = FlipJob::new(id.to_string());
            j.sell_only = true; j.total = qty; j.floor = (p.bid() * 10.0).round() / 10.0; j.undercut_gap = SELL_REQUOTE_GAP; j.exchange = 0;
            // an auto flip of the same item would fight over the same goods and offers (and blocks a second job): this button takes over
            let clash: Vec<u32> = self.flips.iter().filter(|f| f.product == id && matches!(f.state, FlipState::Running | FlipState::Paused)).map(|f| f.id).collect();
            for fid in clash { self.flip_op(fid, "stop"); let _ = self.toast_new(1, format!("Stopped auto flip #{fid} for {}: Auto requote manages it now", pretty_s(id))); }
            self.flip_add(j);
        }
    }

    fn flip_buy_leg(&mut self, i: usize) {
        let (ice, cycles, done) = (self.flips[i].ice_id, self.flips[i].cycles, self.flips[i].cycles_done);
        if ice != 0 {
            match self.jobs.iter().find(|x| x.id == ice).map(|x| x.state) {
                Some(iceberg::JobState::Done) | None => {   // finished (a finished iceberg removes itself a few seconds later)
                    let j = &mut self.flips[i]; j.cycles_done += 1; j.ice_id = 0;
                    if let Some(pc) = j.pending_cycles.iter_mut().find(|c| c.buy_done_t.is_none()) { pc.buy_done_t = Some(now_secs()); }
                    j.note = format!("cycle {} bought: flipping what is in escrow", j.cycles_done);
                }
                Some(iceberg::JobState::Stopped) => { let j = &mut self.flips[i]; j.ice_id = 0; j.state = FlipState::Paused; j.note = "its buy iceberg was stopped: Resume starts the next cycle".into(); }
                _ => {}
            }
        } else if (cycles == 0 || done < cycles) && !self.flips[i].cap_blocked { self.flip_start_cycle(i); }
    }

    /// A cycle is finished when everything it bought has been flipped and sold. Turns it into a history row with projected vs actual time.
    fn flip_finish_cycles(&mut self, i: usize) {
        if self.flips[i].pending_cycles.is_empty() || self.orders_at.map_or(true, |t| t.elapsed() > Duration::from_secs(150)) { return; }
        let product = self.flips[i].product.clone();
        let open_sell: u64 = self.orders.iter().filter(|r| r.side == "SELL" && same_product(&r.product, &product)).map(|r| r.total.saturating_sub(r.filled)).sum();
        let waiting_buy = self.orders.iter().any(|r| r.side == "BUY" && same_product(&r.product, &product) && r.claimable && r.to_claim > 0);
        let now = now_secs();
        let mut done: Vec<FlipCycle> = vec![];
        {
            let j = &mut self.flips[i];
            let (flipped, cost, list, free) = (j.flipped_units, j.buy_cost, j.list_value, j.pending.is_none() && !waiting_buy && j.backlog == 0);
            let (id, tax) = (j.id, j.tax_pct);
            let sold = flipped.saturating_sub(open_sell);
            let bases: Vec<(u32, u64, f64, f64)> = j.pending_cycles.iter().map(|c| (c.n, c.base_flipped, c.base_cost, c.base_list)).collect();
            j.pending_cycles.retain_mut(|pc| {
                let Some(bd) = pc.buy_done_t else { return true };
                if pc.cum.is_none() {
                    // Cycles overlap: the next cycle starts buying while this one is still being flipped and sold. This cycle's flips end where the next one's began
                    // (counting everything up to "now" for each of them made every cycle claim the units of all the cycles after it).
                    if let Some(nx) = bases.iter().find(|b| b.0 == pc.n + 1) { pc.cum = Some((nx.1, nx.2, nx.3)); }
                    else if free { pc.cum = Some((flipped, cost, list)); } else { return true; }
                }
                let (cum_u, cum_c, cum_l) = pc.cum.unwrap_or((flipped, cost, list));
                if !(open_sell == 0 || sold as f64 >= cum_u as f64 * 0.995) { return true; }
                let units = cum_u.saturating_sub(pc.base_flipped);
                if units == 0 { return false; }
                let (dc, dl) = (cum_c - pc.base_cost, cum_l - pc.base_list);
                done.push(FlipCycle { t: now, id, product: product.clone(), n: pc.n, units, buy_avg: dc / units as f64, list_avg: dl / units as f64 / (1.0 - tax / 100.0), profit: dl - dc,
                    proj_secs: pc.proj_secs, actual_secs: (now - pc.start_t).max(0.0), proj_buy: pc.proj_buy, actual_buy: (bd - pc.start_t).max(0.0), v: 2 });
                false
            });
        }
        for c in done {
            self.say(GREEN, format!("auto flip #{}: cycle {} of {} done ({} units, ≈{} profit) in {} (projected {})", c.id, c.n, pretty_s(&c.product), grouped(c.units), fmt_coins(c.profit), fmt_eta(c.actual_secs), fmt_eta(c.proj_secs)));
            self.flip_cycles.push(c);
            if self.flip_cycles.len() > 200 { self.flip_cycles.remove(0); }
        }
    }

    fn flip_check_done(&mut self, i: usize) {
        let j = &self.flips[i];
        if j.ice_id != 0 || j.cycles == 0 || j.cycles_done < j.cycles || j.pending.is_some() || j.backlog > 0 { return; }
        let open_buy = self.orders.iter().any(|r| r.side == "BUY" && same_product(&r.product, &j.product) && r.claimable && r.to_claim > 0);
        let open_sell = self.orders.iter().any(|r| r.side == "SELL" && same_product(&r.product, &j.product) && !(r.total > 0 && r.filled >= r.total));
        if !open_buy && !open_sell && self.orders_at.is_some() {
            let j = &mut self.flips[i]; j.state = FlipState::Done; j.note = format!("all {} cycle(s) done: {} flipped, ≈{} profit", j.cycles, grouped(j.flipped_units), fmt_coins(j.est_profit()));
            let id = j.id; let n = pretty_s(&j.product);
            self.say(GREEN, format!("auto flip #{id} ({n}) finished"));
        }
    }

    /// One agent action for this job: flip a filled slice, or re-list an undercut sell offer. Returns true if something was sent.
    fn flip_act(&mut self, i: usize) -> bool {
        if !self.agent_up || self.halted || self.draining || !self.inflight.is_empty() || self.busy(&["fliporder", "requote_sell", "claimorder", "tradesellqty"]) { return false; }
        if let Some((t, what)) = self.flips[i].pending {
            if t.elapsed() < Duration::from_secs(180) { return false; }
            let j = &mut self.flips[i]; j.pending = None; j.note = format!("no answer to the {what}: looking again");
        }
        if self.flips[i].exchange == 1 { return self.flip_act_trades(i); }
        let (product, now) = (self.flips[i].product.clone(), Instant::now());
        // a filled slice sits in escrow: it must have been there for 15 s, so the iceberg has booked the fill before the order disappears
        // A partly filled order is flipped too once enough of it is filled (a quarter of the order and about a million coins): the goods start selling while the rest is
        // still being bought, instead of everything at the end. Not while the product already has 4 sell offers up (the bazaar limits how many you can have).
        let open_sells = self.orders.iter().filter(|r| r.side == "SELL" && same_product(&r.product, &product) && !(r.total > 0 && r.filled >= r.total)).count();
        let partial_wait = self.flips[i].partial_fail_at.elapsed() < Duration::from_secs(300);   // a partial flip that the game refused: not again for 5 minutes
        let partial_ok = |r: &OrderRow| !partial_wait && open_sells < 4 && r.filled < r.total && r.to_claim as f64 * r.price.unwrap_or(0.0) >= PARTIAL_FLIP_COINS && r.to_claim * 4 >= r.total;
        let rows: Vec<OrderRow> = self.orders.iter().filter(|r| r.side == "BUY" && same_product(&r.product, &product) && r.claimable && r.to_claim > 0 && r.total > 0 && (r.filled >= r.total || partial_ok(r))).cloned().collect();
        {
            let seen = &mut self.flips[i].seen;
            seen.retain(|k, _| rows.iter().any(|r| (near1(r.price.unwrap_or(0.0)) * 10.0) as i64 == k.0 && r.to_claim == k.1));
            for r in &rows { seen.entry(((near1(r.price.unwrap_or(0.0)) * 10.0) as i64, r.to_claim)).or_insert(now); }
        }
        let mut rows = rows;
        rows.sort_by(|a, b| b.to_claim.cmp(&a.to_claim));
        let ready = rows.iter().find(|r| partial_ok(r) || self.flips[i].seen.get(&((near1(r.price.unwrap_or(0.0)) * 10.0) as i64, r.to_claim)).map_or(false, |t| t.elapsed() >= Duration::from_secs(15))).cloned();
        if let Some(r) = ready {
            let buy_price = r.price.unwrap_or(0.0);
            let sell = self.flip_sell_price(&self.flips[i], buy_price);
            let _ = self.cmd_tx.send(json!({"cmd": "fliporder", "auto": true, "product": product, "side": "BUY", "price": r.price, "sell_price": sell}));
            self.say(ORANGE, format!("→ auto flip #{}: {} x{} bought @ {:.1} → sell offer @ {:.1}", self.flips[i].id, pretty_s(&product), grouped(r.to_claim), buy_price, sell));
            let j = &mut self.flips[i]; j.pending = Some((now, "flip")); j.note = format!("flipping {} @ {:.1}", grouped(r.to_claim), sell);
            return true;
        }
        // Goods in the inventory: when the iceberg re-quotes it cancels its part-filled order, and the game hands over what had filled. Nothing else sells those, so they would
        // pile up in the inventory: list them for sale like any flipped slice.
        // Any amount, as soon as it is seen (goods should spend as little time in the inventory as possible), as long as an order slot is free.
        if self.flips[i].inv_sell_at.elapsed() >= Duration::from_secs(20) {
            let have = self.inv_all.iter().find(|x| x.id == product).map_or(0, |x| x.count);
            let slots_ok = if self.coop_orders.is_empty() { self.orders.len() < 14 } else { self.orders.len() < crate::iceberg::COOP_ORDER_SHARE };
            if have > 0 && slots_ok {
                let bid = self.products.get(&product).map_or(0.0, |p| p.bid());
                let j = &self.flips[i];
                let basis = if j.flipped_units > 0 { j.buy_cost / j.flipped_units as f64 } else { bid };
                let sell = self.flip_sell_price(j, basis);
                self.source_sacks = false;
                self.dispatch(Order { cmd: "selloffer", id: product.clone(), qty: have, unit: sell, kind: Kind::Limit });
                if let Some(x) = self.inv_all.iter_mut().find(|x| x.id == product) { x.count = 0; }
                let j = &mut self.flips[i];
                j.flipped_units += have; j.buy_cost += basis * have as f64; j.list_value += sell * have as f64 * (1.0 - j.tax_pct / 100.0);
                j.inv_sell_at = now; j.sell_at = now; j.note = format!("listed {} from the inventory @ {:.1}", grouped(have), sell);
                let (jid, t) = (j.id, now_secs());
                self.flip_log.push(FlipEvent { t, id: jid, product: product.clone(), side: "BUY", units: have, price: basis });
                self.flip_log.push(FlipEvent { t, id: jid, product: product.clone(), side: "SELL", units: have, price: sell });
                self.say(ORANGE, format!("→ auto flip #{}: {} x{} from the inventory → sell offer @ {:.1}", jid, pretty_s(&product), grouped(have), sell));
                self.flip_save();
                return true;
            }
        }
        // several small sell offers of this item: cancel and list them as one (frees order slots for buying; one offer is one re-list when undercut)
        let open_sells = self.orders.iter().filter(|r| r.side == "SELL" && same_product(&r.product, &product) && !(r.total > 0 && r.filled >= r.total)).count();
        if open_sells >= MERGE_AT && self.flips[i].sell_at.elapsed() >= Duration::from_secs(30) {
            let j = &self.flips[i];
            let basis = if j.flipped_units > 0 { j.buy_cost / j.flipped_units as f64 } else { 0.0 };
            let new_price = self.flip_sell_price(j, basis);
            let _ = self.cmd_tx.send(json!({"cmd": "requote_sell", "auto": true, "all": true, "product": product, "new_price": new_price}));
            self.say(ORANGE, format!("→ auto flip #{}: {} open sell offers of {}: merging them into one @ {:.1}", self.flips[i].id, open_sells, pretty_s(&product), new_price));
            let j = &mut self.flips[i]; j.pending = Some((now, "sell requote")); j.sell_at = now; j.note = format!("merging {open_sells} sell offers @ {new_price:.1}");
            return true;
        }
        // an undercut sell offer
        if self.flips[i].undercut && self.flips[i].sell_at.elapsed() >= Duration::from_secs(self.flips[i].undercut_gap) {
            let ask = self.flip_other_ask(&product);   // someone else's lowest offer
            let j = &self.flips[i];
            let cand = self.orders.iter().filter(|r| r.side == "SELL" && same_product(&r.product, &product) && !(r.total > 0 && r.filled >= r.total))
                .filter(|r| r.price.map_or(false, |p| ask > 0.0 && ask < p - 0.05) && (r.total - r.filled.min(r.total)) <= j.requote_max_qty)
                .max_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal)).cloned();
            if let Some(r) = cand {
                let old = r.price.unwrap_or(0.0);
                let basis = if j.flipped_units > 0 { j.buy_cost / j.flipped_units as f64 } else { 0.0 };
                let new_price = self.flip_sell_price(j, basis);
                if new_price < old - 0.05 {
                    let _ = self.cmd_tx.send(json!({"cmd": "requote_sell", "auto": true, "product": product, "price": r.price, "new_price": new_price}));
                    self.say(ORANGE, format!("→ auto flip #{}: {} sell offer undercut (ask {:.1}): re-list {:.1} → {:.1}", self.flips[i].id, pretty_s(&product), ask, old, new_price));
                    let j = &mut self.flips[i]; j.pending = Some((now, "sell requote")); j.sell_at = now; j.note = format!("re-listing @ {new_price:.1}");
                    return true;
                } else if let Some(j) = self.flips.get_mut(i) { j.sell_at = now; j.note = format!("undercut, but {new_price:.1} would be at or below my floor: keeping {old:.1}"); }
            }
        }
        false
    }

    /// Selling through the /trades menu: claim each filled slice (into the sacks if the inventory is full), then sell it to the NPC in rounds.
    fn flip_act_trades(&mut self, i: usize) -> bool {
        let (product, now) = (self.flips[i].product.clone(), Instant::now());
        let npc = npc_sell_of(&product).filter(|v| *v > 0.0).unwrap_or(0.0);
        // what is left of the NPC daily limit, in items at this item's price
        let fit = if npc > 0.0 { ((NPC_SELL_CAP - self.npc_sold_today()).max(0.0) / npc).floor() as u64 } else { 0 };
        // 1. goods already claimed and waiting to be sold: as many as still fit under the limit
        if self.flips[i].backlog > 0 {
            if fit < 64 {
                self.flips[i].note = format!("the NPC daily limit ({}) is used up: waiting for the next UTC day", fmt_coins(NPC_SELL_CAP));
                return false;
            }
            let sell_all = self.flips[i].sell_all;
            let want = if sell_all { self.flips[i].backlog } else { self.flips[i].backlog.min(self.flips[i].requote_max_qty.max(64)) };
            let all_fits = sell_all && want <= fit;
            let chunk = if want > fit { fit / 64 * 64 } else { want };   // whole stacks only: the shop rounds a sale up to stacks
            let _ = if all_fits { self.cmd_tx.send(json!({"cmd": "tradesellqty", "auto": true, "product": product, "qty": Value::Null, "mode": "all", "from_sacks": true})) }
                else { self.cmd_tx.send(json!({"cmd": "tradesellqty", "auto": true, "product": product, "qty": chunk, "mode": "qty", "from_sacks": true})) };
            self.say(ORANGE, format!("→ auto flip #{}: sell {} {} to the NPC{}", self.flips[i].id, if all_fits { "all".to_string() } else { grouped(chunk) }, pretty_s(&product), if want > fit { " (what still fits under the daily limit)" } else { "" }));
            let j = &mut self.flips[i]; j.pending = Some((now, "tradesell")); j.note = format!("selling {} to the NPC", if all_fits { "everything".to_string() } else { grouped(chunk) });
            return true;
        }
        // 2. a filled slice in escrow (it must have sat there for 15 s so the iceberg books the fill first)
        // a partly filled order the market has moved above will not fill again: whatever it holds is claimed however small, so it can be cancelled afterwards
        let (bid, live) = self.flip_bid_and_live(&product);
        let dead = |r: &OrderRow| bid > 0.0 && r.price.map_or(false, |p| p < bid - 1.0 && !live.iter().any(|l| (l - p).abs() < 0.15));
        let rows: Vec<OrderRow> = self.orders.iter().filter(|r| r.side == "BUY" && same_product(&r.product, &product) && r.claimable && r.to_claim > 0 && r.total > 0 && (r.filled >= r.total || r.to_claim >= PARTIAL_CLAIM || dead(r))).cloned().collect();
        {
            let seen = &mut self.flips[i].seen;
            seen.retain(|k, _| rows.iter().any(|r| (near1(r.price.unwrap_or(0.0)) * 10.0) as i64 == k.0 && r.to_claim == k.1));
            for r in &rows { seen.entry(((near1(r.price.unwrap_or(0.0)) * 10.0) as i64, r.to_claim)).or_insert(now); }
        }
        // A completely filled slice waits 15 s first (the iceberg books the fill). A partly filled one is claimed as soon as enough is in escrow: its amount keeps
        // changing while it fills, so waiting for it to hold still would leave it there until it is full, and the escrow backs up behind the sales.
        let mut rows = rows;
        rows.sort_by(|a, b| b.to_claim.cmp(&a.to_claim));
        let ready = rows.iter().find(|r| (r.filled < r.total && (r.to_claim >= PARTIAL_CLAIM || dead(r))) || self.flips[i].seen.get(&((near1(r.price.unwrap_or(0.0)) * 10.0) as i64, r.to_claim)).map_or(false, |t| t.elapsed() >= Duration::from_secs(15))).cloned();
        let Some(r) = ready else { return false };
        let buy_price = r.price.unwrap_or(0.0);
        let min_ok = buy_price * (1.0 + self.flips[i].margin_pct / 100.0);
        if npc <= 0.0 || npc < min_ok {
            let j = &mut self.flips[i]; j.state = FlipState::Paused;
            j.note = format!("paused: the NPC pays {npc:.1}, below your minimum margin ({min_ok:.1} for a buy at {buy_price:.1}). The slice stays in escrow.");
            return false;
        }
        if fit < 64 {   // claim only while at least a stack can still be sold today; what does not fit stays in the backlog until the reset
            self.flips[i].note = format!("the NPC daily limit ({}) is used up: waiting for the next UTC day", fmt_coins(NPC_SELL_CAP));
            return false;
        }
        let _ = self.cmd_tx.send(json!({"cmd": "claimorder", "auto": true, "product": product, "side": "BUY", "price": r.price, "sack_on_full": true, "npc_on_full": true}));
        self.say(ORANGE, format!("→ auto flip #{}: claim {} {} bought @ {:.1} to sell to the NPC @ {:.1}", self.flips[i].id, grouped(r.to_claim), pretty_s(&product), buy_price, npc));
        let j = &mut self.flips[i]; j.pending = Some((now, "claim")); j.claim_ctx = Some((r.to_claim, buy_price)); j.note = format!("claiming {}", grouped(r.to_claim));
        true
    }

    /// The claim for a /trades flip finished: the goods are ours, now they get sold.
    pub fn flip_on_claimed(&mut self, v: &Value) -> bool {
        let product = v["product"].as_str().unwrap_or("").to_string();
        let Some(i) = self.flips.iter().position(|j| j.product == product && matches!(j.pending, Some((_, "claim")))) else { return false };
        let npc = npc_sell_of(&product).unwrap_or(0.0);
        let j = &mut self.flips[i];
        let (mut qty, price) = j.claim_ctx.take().unwrap_or((0, 0.0));
        let row_qty = qty;
        // one claim can run through several orders at the same price: trust what the agent counted over the one row we saw
        if let Some(n) = v["info"]["claimed_units"].as_u64() { if n > 0 { qty = n; } }
        j.pending = None; j.fails = 0;
        // what the claim already sold to the NPC whenever the inventory filled up does not need selling again
        let (npc_sold, npc_coins) = (v["info"]["npc_sold"].as_u64().unwrap_or(0).min(qty), v["info"]["npc_coins"].as_f64().unwrap_or(0.0));
        j.backlog += qty - npc_sold; j.flipped_units += qty; j.buy_cost += price * qty as f64; j.list_value += npc * qty as f64 * (1.0 - j.tax_pct / 100.0);
        if npc_sold > 0 { j.real_units += npc_sold; j.real_coins += npc_coins; }
        j.note = if npc_sold > 0 { format!("claimed {}, {} of it already sold to the NPC on the way", grouped(qty), grouped(npc_sold)) } else { format!("claimed {}: selling to the NPC", grouped(qty)) };
        let (jid, now) = (j.id, now_secs());
        self.flip_log.push(FlipEvent { t: now, id: jid, product: product.clone(), side: "BUY", units: qty, price });
        self.orders.retain(|r| !(r.side == "BUY" && same_product(&r.product, &product) && r.claimable && r.price.map_or(false, |p| (p - price).abs() < 0.05) && r.to_claim == row_qty));
        if self.agent_up { let _ = self.cmd_tx.send(json!({"cmd": "orders", "auto": true})); self.orders_at = Some(Instant::now()); }
        self.flip_save();
        true
    }

    /// A /trades sale finished (true when it was this engine's own sale: then nothing else should treat the reply as an order of theirs).
    pub fn flip_on_trades_fill(&mut self, v: &Value) -> bool {
        let Some(i) = self.flips.iter().position(|j| matches!(j.pending, Some((_, "tradesell")))) else { return false };
        let sold = v["sold"].as_u64().unwrap_or(0);
        let product = self.flips[i].product.clone();
        let j = &mut self.flips[i];
        j.pending = None;
        if sold == 0 { j.fails += 1; j.note = "the NPC bought nothing (are the goods in the inventory or sacks?)".into(); if j.fails >= 3 { j.state = FlipState::Paused; j.note = "paused: three sales in a row sold nothing".into(); } return true; }
        j.fails = 0; j.backlog = if j.sell_all { 0 } else { j.backlog.saturating_sub(sold) };
        j.note = format!("sold {} to the NPC{}", grouped(sold), if j.backlog > 0 { format!(", {} to go", grouped(j.backlog)) } else { String::new() });
        let npc = v["gained"].as_f64().map(|g| g / sold as f64).unwrap_or_else(|| npc_sell_of(&product).unwrap_or(0.0));
        if let Some(g) = v["gained"].as_f64() { j.real_units += sold; j.real_coins += g; }   // the coins the sale really paid
        let (jid, now) = (j.id, now_secs());
        self.flip_log.push(FlipEvent { t: now, id: jid, product, side: "SELL", units: sold, price: npc });
        if self.flip_log.len() > 500 { let n = self.flip_log.len() - 500; self.flip_log.drain(..n); }
        self.flip_save();
        true
    }

    // ---------------------------------------------------------------- agent replies
    pub fn flip_on_flipped(&mut self, v: &Value) {
        let product = v["product"].as_str().unwrap_or("").to_string();
        let Some(i) = self.flips.iter().position(|j| j.product == product && matches!(j.pending, Some((_, "flip")))) else { return };
        let (qty, sell, buy) = (v["qty"].as_u64().unwrap_or(0), v["sell_price"].as_f64().unwrap_or(0.0), v["price"].as_f64().unwrap_or(0.0));
        let j = &mut self.flips[i];
        j.pending = None; j.fails = 0; j.sell_at = Instant::now();
        j.flipped_units += qty; j.buy_cost += buy * qty as f64; j.list_value += sell * qty as f64 * (1.0 - j.tax_pct / 100.0);
        j.note = format!("listed {} @ {:.1}", grouped(qty), sell);
        let (jid, now) = (j.id, now_secs());
        self.flip_log.push(FlipEvent { t: now, id: jid, product: product.clone(), side: "BUY", units: qty, price: buy });
        self.flip_log.push(FlipEvent { t: now, id: jid, product: product.clone(), side: "SELL", units: qty, price: sell });
        if self.flip_log.len() > 500 { let n = self.flip_log.len() - 500; self.flip_log.drain(..n); }
        // the slice is no longer claimable: drop it from the list now, the real re-read follows
        self.orders.retain(|r| !(r.side == "BUY" && same_product(&r.product, &product) && r.claimable && r.price.map_or(false, |p| (p - buy).abs() < 0.05) && r.to_claim == qty));
        if self.agent_up { let _ = self.cmd_tx.send(json!({"cmd": "orders", "auto": true})); self.orders_at = Some(Instant::now()); }
        self.flip_save();
    }

    pub fn flip_on_requoted_sell(&mut self, v: &Value) {
        let product = v["product"].as_str().unwrap_or("").to_string();
        let Some(i) = self.flips.iter().position(|j| j.product == product && matches!(j.pending, Some((_, "sell requote")))) else { return };
        let j = &mut self.flips[i];
        j.pending = None; j.sell_at = Instant::now();
        match (v["result"].as_str().unwrap_or(""), v["place_error"].as_str()) {
            ("filled", _) => j.note = "the sell offer had filled meanwhile".into(),
            (_, Some(e)) => { j.fails += 1; j.note = format!("cancelled the sell offer but could not re-list it: {e}. The goods are in your inventory."); j.state = FlipState::Paused; }
            _ => {
                if let Some(p) = v["price"].as_f64() { j.my_price = p; }
                j.sell_requotes += 1; j.fails = 0; j.note = format!("{}re-listed {} @ {:.1}", v["merged"].as_u64().map_or(String::new(), |m| format!("merged {m} offers, ")), grouped(v["returned"].as_u64().unwrap_or(0)), v["price"].as_f64().unwrap_or(0.0));
                let ev = FlipEvent { t: now_secs(), id: j.id, product: product.clone(), side: "SELL", units: v["returned"].as_u64().unwrap_or(0), price: v["price"].as_f64().unwrap_or(0.0) };
                self.flip_log.push(ev);
                if self.flip_log.len() > 500 { let n = self.flip_log.len() - 500; self.flip_log.drain(..n); }
            }
        }
        if self.agent_up { let _ = self.cmd_tx.send(json!({"cmd": "orders", "auto": true})); self.orders_at = Some(Instant::now()); }
        self.flip_save();
    }

    /// An agent error for something this engine sent. Returns true when it was ours (so nothing else treats it as the error of one of its own orders).
    pub fn flip_on_error(&mut self, msg: &str, cmd: Option<&str>) -> bool {
        let what = match cmd { Some("fliporder") => "flip", Some("requote_sell") => "sell requote", Some("claimorder") => "claim", Some("tradesellqty") => "tradesell", _ => return false };
        let mut ours = false;
        for j in self.flips.iter_mut().filter(|j| matches!(j.pending, Some((_, w)) if w == what)) {
            ours = true;
            j.pending = None; j.sell_at = Instant::now();
            if what == "flip" && msg.contains("nothing filled yet to flip") {
                // a part-filled order whose goods were not there to flip (the order list was out of date): not a failure of the flip, just not now
                j.partial_fail_at = Instant::now(); j.note = "a partly filled order had nothing to flip yet: trying again later".into();
                continue;
            }
            if what == "sell requote" && msg.contains("nothing to merge") {
                // the inventory has no room to take the offers back: not a failure, the offers stay as they are
                j.note = "too little inventory room to merge the sell offers: leaving them".into();
                continue;
            }
            if what == "tradesell" && msg.contains("to sell") && j.backlog > 0 {
                // nothing is left to sell: the sale we were waiting for did go through (its answer was lost, e.g. in a restart). The backlog was stale.
                j.backlog = 0; j.fails = 0;
                j.note = "nothing left to sell: the earlier sale had already gone through".into();
                continue;
            }
            j.fails += 1;
            j.note = format!("{what} failed: {msg}");
            if j.fails >= 3 { j.state = FlipState::Paused; j.note = format!("paused after repeated errors: {msg}"); }
        }
        self.flip_save();
        ours
    }
}

// ======================================================================== the tab
const PANEL_BG: Color32 = Color32::from_rgb(13, 13, 13);
const HEAD_BG: Color32 = Color32::from_rgb(36, 36, 36);

/// UTC calendar date of a unix time ("11-14").
fn utc_date(t: f64) -> String {
    let z = (t / 86400.0).floor() as i64 + 719468; let era = z.div_euclid(146097); let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1; let m = if mp < 10 { mp + 3 } else { mp - 9 }; let _ = (era, yoe);
    format!("{:02}-{:02}", m, d)
}

/// State of the History tab (kept between frames).
#[derive(Clone, Default)]
pub struct FlipHistUi { pub flip: Option<u32>, pub result: u8, pub view: u8, pub sort: u8, pub asc: bool, pub open: Option<(u32, u32)>, pub export: bool }

fn hist_axes(ui: &mut egui::Ui, h: f32) -> (egui::Painter, egui::Rect) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 0.0, Color32::from_gray(22));
    (p, egui::Rect::from_min_max(rect.min + egui::vec2(70.0, 14.0), rect.max - egui::vec2(12.0, 26.0)))
}

/// Cumulative profit over time.
fn hist_line_chart(ui: &mut egui::Ui, pts: &[(f64, f64)]) {
    let (p, plot) = hist_axes(ui, 210.0);
    if pts.len() < 1 { p.text(plot.center(), egui::Align2::CENTER_CENTER, "no cycles to show", FontId::proportional(16.0), DIM); return; }
    let (t0, t1) = (pts.first().unwrap().0, pts.last().unwrap().0);
    let span_t = (t1 - t0).max(1.0);
    let hi = pts.iter().map(|q| q.1).fold(0.0f64, f64::max); let lo = pts.iter().map(|q| q.1).fold(0.0f64, f64::min); let span = (hi - lo).max(1.0);
    let y = |v: f64| plot.bottom() - ((v - lo) / span) as f32 * plot.height();
    let x = |t: f64| if pts.len() == 1 { plot.center().x } else { plot.left() + plot.width() * ((t - t0) / span_t) as f32 };
    for k in 0..=4 { let v = lo + span * k as f64 / 4.0; p.line_segment([egui::pos2(plot.left(), y(v)), egui::pos2(plot.right(), y(v))], Stroke::new(1.0, Color32::from_gray(45)));
        p.text(egui::pos2(plot.left() - 6.0, y(v)), egui::Align2::RIGHT_CENTER, fmt_coins(v), FontId::proportional(14.0), Color32::from_gray(190)); }
    p.text(egui::pos2(plot.left(), plot.bottom() + 12.0), egui::Align2::LEFT_CENTER, format!("{} {}", utc_date(t0), utc_hms(t0)), FontId::proportional(14.0), Color32::from_gray(170));
    p.text(egui::pos2(plot.right(), plot.bottom() + 12.0), egui::Align2::RIGHT_CENTER, format!("{} {}", utc_date(t1), utc_hms(t1)), FontId::proportional(14.0), Color32::from_gray(170));
    let line: Vec<egui::Pos2> = pts.iter().map(|q| egui::pos2(x(q.0), y(q.1))).collect();
    let end = pts.last().unwrap().1; let col = if end < 0.0 { RED } else { GREEN };
    if line.len() > 1 { p.add(egui::Shape::line(line.clone(), Stroke::new(2.0, col))); }
    for q in &line { p.circle_filled(*q, 3.0, col); }
}

/// Projected against actual time per cycle (minutes), side by side.
fn hist_bar_chart(ui: &mut egui::Ui, bars: &[(String, f64, f64)]) {
    let (p, plot) = hist_axes(ui, 210.0);
    if bars.is_empty() { p.text(plot.center(), egui::Align2::CENTER_CENTER, "no cycles to show", FontId::proportional(16.0), DIM); return; }
    let hi = bars.iter().map(|b| b.1.max(b.2)).fold(1.0f64, f64::max);
    let y = |v: f64| plot.bottom() - (v / hi) as f32 * plot.height();
    for k in 0..=4 { let v = hi * k as f64 / 4.0; p.line_segment([egui::pos2(plot.left(), y(v)), egui::pos2(plot.right(), y(v))], Stroke::new(1.0, Color32::from_gray(45)));
        p.text(egui::pos2(plot.left() - 6.0, y(v)), egui::Align2::RIGHT_CENTER, fmt_eta(v), FontId::proportional(14.0), Color32::from_gray(190)); }
    let w = plot.width() / bars.len() as f32;
    for (k, (name, a, b)) in bars.iter().enumerate() {
        let x0 = plot.left() + k as f32 * w; let bw = (w * 0.38).clamp(1.0, 18.0);
        p.rect_filled(egui::Rect::from_min_max(egui::pos2(x0 + w * 0.1, y(*a)), egui::pos2(x0 + w * 0.1 + bw, plot.bottom())), 0.0, Color32::from_gray(110));
        p.rect_filled(egui::Rect::from_min_max(egui::pos2(x0 + w * 0.1 + bw, y(*b)), egui::pos2(x0 + w * 0.1 + 2.0 * bw, plot.bottom())), 0.0, if *a > 0.0 && b <= a { GREEN } else { ORANGE });
        if bars.len() <= 16 { p.text(egui::pos2(x0 + w * 0.5, plot.bottom() + 12.0), egui::Align2::CENTER_CENTER, name, FontId::proportional(12.0), Color32::from_gray(170)); }
    }
    p.text(egui::pos2(plot.right(), plot.top() - 2.0), egui::Align2::RIGHT_CENTER, "grey = plan · green/orange = actual", FontId::proportional(13.0), DIM);
}

fn utc_hms(t: f64) -> String { let s = t as u64 % 86400; format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) }

/// A framed section with a dark title bar (orange title), like the mock-up.
fn panel(ui: &mut egui::Ui, title: &str, right: Option<&str>, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::NONE.fill(PANEL_BG).stroke(Stroke::new(1.0, Color32::from_gray(70))).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 34.0), egui::Sense::hover());
        ui.painter().rect_filled(r, 0.0, HEAD_BG);
        ui.painter().text(r.left_center() + egui::vec2(10.0, 0.0), egui::Align2::LEFT_CENTER, title, FontId::new(23.0, egui::FontFamily::Name("terminal-bold".into())), ORANGE);
        if let Some(t) = right { ui.painter().text(r.right_center() - egui::vec2(10.0, 0.0), egui::Align2::RIGHT_CENTER, t, FontId::proportional(17.0), Color32::WHITE); }
        egui::Frame::NONE.inner_margin(egui::Margin::symmetric(10, 8)).show(ui, |ui| { ui.set_width(ui.available_width()); body(ui); });
    });
    ui.add_space(8.0);
}

/// How the +/- buttons next to a field change it: the centre button moves by `big` (right-click = down), the outer ones by `small`.
#[derive(Clone, Copy)]
struct Step { big: f64, small: f64, min: f64, max: f64, dec: u8 }
const fn st(big: f64, small: f64, min: f64, max: f64, dec: u8) -> Step { Step { big, small, min, max, dec } }

pub fn fmt_step(x: f64) -> String { let t = format!("{x:.2}"); t.trim_end_matches('0').trim_end_matches('.').to_string() }

/// `text` moved by `delta` (a number, or a quantity like 50k), kept within the step's range.
fn adjust(text: &str, delta: f64, s: Step) -> String {
    let cur = text.trim().parse::<f64>().ok().or_else(|| parse_qty(text).map(|q| q as f64)).unwrap_or(0.0);
    let v = (cur + delta).clamp(s.min, s.max);
    if s.dec == 0 { format!("{}", v.round() as i64) } else { let t = format!("{:.*}", s.dec as usize, v); t.trim_end_matches('0').trim_end_matches('.').to_string() }
}

/// Every +/- button has this width, so a row can reserve exactly the room its three buttons take.
const STEP_BTN_W: f32 = 62.0;
/// The room three step buttons take, with their gaps and the gap before them.
const STEP_ROW_W: f32 = STEP_BTN_W * 3.0 + 3.0 * 2.0 + 10.0;

/// [-small] [+big] [+small] at the end of a field
fn step_buttons(ui: &mut egui::Ui, text: &mut String, s: Step) {
    let btn = |ui: &mut egui::Ui, label: String, tip: &str| ui.add_sized([STEP_BTN_W, 28.0], egui::Button::new(RichText::new(label).size(16.0).color(Color32::WHITE)).fill(Color32::from_rgb(30, 30, 30)).stroke(Stroke::new(1.0, Color32::from_gray(80)))).on_hover_text(tip.to_string());
    ui.spacing_mut().item_spacing.x = 3.0;
    if btn(ui, format!("−{}", fmt_step(s.small)), &format!("Decrease by {}", fmt_step(s.small))).clicked() { *text = adjust(text, -s.small, s); }
    let mid = btn(ui, format!("+{}", fmt_step(s.big)), &format!("Increase by {} (right-click: decrease by {})", fmt_step(s.big), fmt_step(s.big)));
    if mid.clicked() { *text = adjust(text, s.big, s); }
    if mid.secondary_clicked() { *text = adjust(text, -s.big, s); }
    if btn(ui, format!("+{}", fmt_step(s.small)), &format!("Increase by {}", fmt_step(s.small))).clicked() { *text = adjust(text, s.small, s); }
}

/// label on the left, an orange input, then the +/- buttons
fn frow(ui: &mut egui::Ui, label: &str, text: &mut String, valid: Option<bool>, step: Option<Step>) -> egui::Response {
    ui.horizontal(|ui| {
        lbl(ui, label);
        let w = ui.available_width() - 18.0 - if step.is_some() { STEP_ROW_W } else { 0.0 };   // 18: the box's own padding
        let r = ui.add(egui::TextEdit::singleline(text).text_color(ORANGE).font(FontId::proportional(20.0)).desired_width(w.max(80.0)).margin(egui::vec2(8.0, 5.0)));
        fx::focus_ring(ui, &r, valid);
        if let Some(s) = step { step_buttons(ui, text, s); }
        r
    }).inner
}

/// label on the left, value on the right
fn srow(ui: &mut egui::Ui, label: &str, value: String, color: Color32) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(17.0).color(Color32::from_gray(200)));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { ui.label(RichText::new(value).size(17.0).color(color)); });
    });
}

/// a label that fills the left column of a form row, left-aligned
fn lbl(ui: &mut egui::Ui, text: &str) { ui.allocate_ui_with_layout(egui::vec2(180.0, 28.0), egui::Layout::left_to_right(egui::Align::Center), |ui| { ui.set_min_size(egui::vec2(180.0, 28.0)); ui.label(RichText::new(text).size(18.0)); }); }

fn tab_button(ui: &mut egui::Ui, label: &str, selected: bool) -> egui::Response {
    let (fill, col) = if selected { (ORANGE, Color32::BLACK) } else { (Color32::from_rgb(28, 28, 28), Color32::WHITE) };
    ui.add(egui::Button::new(RichText::new(label).size(19.0).color(col)).fill(fill).stroke(Stroke::new(1.0, Color32::from_gray(80))).min_size(egui::vec2(0.0, 34.0)).corner_radius(2.0))
}

impl Gui {
    pub fn autoflip_screen(&mut self, ctx: &egui::Context) {
        self.screen_frame(ctx, "Auto Flip  AUTOFLIP", |g, ui| {
            g.flip_tabs(ui);
            ui.add_space(6.0);
            match g.flip_tab { 1 => g.flip_active_tab(ui), 2 => g.flip_history_tab(ui), 3 => g.flip_analytics_tab(ui), _ => g.flip_config_tab(ui) }
        });
        if let Some(j) = self.flip_confirm.clone() {
            egui::Window::new("Start auto flip?").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.label(RichText::new(format!("{} {} per cycle, {}", grouped(j.total), pretty_s(&j.product), if j.cycles == 0 { "until you stop it".to_string() } else { format!("{} cycle(s)", j.cycles) })).size(18.0).strong().color(GREEN));
                ui.label(if j.exchange == 1 { format!("Buys at top bid +{:.1} (max {:.1}), claims each slice and sells it to the NPC in the /trades menu (min margin {}%).", j.offset, j.max_price, j.margin_pct) } else { format!("Buys at top bid +{:.1} (max {:.1}), lists at best ask −{:.1} (floor {}, margin {}%).", j.offset, j.max_price, j.sell_offset, if j.floor > 0.0 { format!("{:.1}", j.floor) } else { "none".into() }, j.margin_pct) });
                ui.label(format!("Worst case one cycle spends ~{} coins.", fmt_coins(j.total as f64 * j.max_price)));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.fx_button("  Start  ").clicked() { self.flip_confirm = None; self.flip_add(j.clone()); self.flip_form.msg = Some((true, "started".into())); }
                    if ui.fx_button("  Cancel  ").clicked() { self.flip_confirm = None; }
                });
            });
        }
        if self.flip_load_dlg { self.flip_load_dialog(ctx); }
    }

    fn flip_tabs(&mut self, ui: &mut egui::Ui) {
        let live = self.flips.iter().filter(|j| matches!(j.state, FlipState::Running | FlipState::Paused)).count();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            for (i, l) in [String::from("1) Config"), format!("2) Active Flips ({live})"), String::from("3) History"), String::from("4) Analytics")].into_iter().enumerate() {
                if tab_button(ui, &l, self.flip_tab as usize == i).clicked() { self.flip_tab = i as u8; }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if tab_button(ui, "7) Export", false).fx_tip("Write the history to a CSV file").clicked() { self.flip_export(); }
                if tab_button(ui, "6) Load", false).fx_tip("Load saved settings into the form").clicked() { self.flip_load_dlg = true; self.flip_tab = 0; }
                if tab_button(ui, "5) Save", false).fx_tip("Save the form's settings as a preset").clicked() { self.flip_save_preset(); }
            });
        });
    }

    // ---------------------------------------------------------------- 1) Config
    fn flip_config_tab(&mut self, ui: &mut egui::Ui) {
        if std::env::var("BLOCKBERG_FLIP_DEMO").is_ok() && self.flip_form.total.is_empty() {
            self.flip_form.product = "ENCHANTED_HARD_STONE".into(); self.flip_form.total = "50000".into(); self.flip_form.max = "500".into(); self.flip_form.buy_mode = 1;
            if std::env::var("BLOCKBERG_FLIP_DEMO").as_deref() == Ok("trades") { self.flip_form.exchange = 1; self.flip_form.product = "ENCHANTED_COAL".into(); self.flip_form.max = "1300".into(); self.flip_form.total = "5000".into(); }
            if std::env::var("BLOCKBERG_FLIP_DEMO").as_deref() == Ok("search") { self.flip_form.product = "ench hard".into(); self.flip_search_open = true; }
            if let Ok(c) = std::env::var("BLOCKBERG_FLIP_CYCLES") { self.flip_form.cycles = c; }
            let now = now_secs();
            for (k, side, price) in [(0.0, "BUY", 0.1), (30.0, "SELL", 0.2), (90.0, "BUY", 0.1), (120.0, "SELL", 0.2)] { self.flip_log.push(FlipEvent { t: now - 300.0 + k, id: 1, product: "ENCHANTED_HARD_STONE".into(), side, units: 50_000, price }); }
        }
        let total_w = ui.available_width();
        let left_w = (total_w * 0.61).max(520.0).min(total_w - 340.0);
        let snap = self.flip_form.clone();
        let pos = |s: &str| s.trim().is_empty() || s.trim().parse::<f64>().map_or(false, |v| v >= 0.0);
        // a newly chosen item fills the buy price range around its bid (bid x 0.8 .. x 1.2); typing over it afterwards sticks
        {
            let key = self.flip_form.product.trim().to_uppercase().replace(' ', "_");
            if self.products.contains_key(&key) && self.flip_filled_for != key {
                let bid = self.products.get(&key).map_or(0.0, |p| p.bid());
                if bid > 0.0 {
                    let lo = ((bid * 0.8) * 10.0).round() / 10.0;
                    self.flip_form.min = fmt_step(lo.max(0.1));
                    self.flip_form.max = fmt_step(((bid * 1.2) * 10.0).round() / 10.0);
                    self.flip_filled_for = key;
                }
            }
        }
        ui.label(RichText::new("Automatically buy low and sell high on the Bazaar. Configure buy/sell rules, timing, and size.").color(Color32::from_gray(210)).size(16.0));
        ui.add_space(4.0);
        egui::ScrollArea::vertical().id_salt("flip_cfg_scroll").auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(left_w); ui.set_max_width(left_w);   // a row that is too wide must not widen the column (it pushed the right side off screen)
                    panel(ui, "1) Item & Buy Side", None, |ui| {
                        // the item has its own search bar: type part of a name, pick from the list
                        let item_ok = self.products.contains_key(self.flip_form.product.trim().to_uppercase().replace(' ', "_").as_str());
                        ui.horizontal(|ui| {
                            lbl(ui, "Item");
                            let r = ui.add(egui::TextEdit::singleline(&mut self.flip_form.product).text_color(ORANGE).font(FontId::proportional(20.0)).desired_width(ui.available_width() - 8.0).margin(egui::vec2(8.0, 5.0)).hint_text("search any item, e.g. ench hard stone"));
                            fx::focus_ring(ui, &r, if self.flip_form.product.is_empty() { None } else { Some(item_ok) });
                            if r.changed() { self.flip_search_open = true; }
                        });
                        let typed = self.flip_form.product.clone();
                        if !item_ok && !typed.trim().is_empty() && self.flip_search_open {
                            let hits = self.flip_search_hits(&typed);
                            if hits.is_empty() { ui.label(RichText::new("no item matches").color(DIM).size(15.0)); }
                            for id in hits {
                                ui.horizontal(|ui| {
                                    ui.add_space(184.0);
                                    icon_cell(ui, &self.icons, &id, "");
                                    if ui.selectable_label(false, RichText::new(pretty_s(&id)).size(17.0)).clicked() { self.flip_form.product = id.clone(); self.flip_search_open = false; }
                                });
                            }
                        } else if item_ok {
                            let name = pretty_s(self.flip_form.product.trim().to_uppercase().replace(' ', "_").as_str());
                            ui.horizontal(|ui| { ui.add_space(184.0); ui.label(RichText::new(format!("✔ {name}")).color(GREEN).size(15.0)); });
                        }
                        frow(ui, "Buy per cycle", &mut self.flip_form.total, Some(parse_qty(&snap.total).is_some()).filter(|_| !snap.total.is_empty()), Some(st(1000.0, 100.0, 1.0, 1.0e9, 0)));
                        frow(ui, "Max per order", &mut self.flip_form.slice, None, Some(st(1000.0, 100.0, 1.0, MAX_ORDER_QTY as f64, 0)));
                        frow(ui, "Buy price min", &mut self.flip_form.min, Some(pos(&snap.min)), Some(st(1.0, 0.1, 0.1, 1.0e9, 1)));
                        frow(ui, "Buy price max", &mut self.flip_form.max, Some(snap.max.trim().parse::<f64>().map_or(false, |v| v > 0.0)).filter(|_| !snap.max.is_empty()), Some(st(10.0, 1.0, 0.1, 1.0e9, 1)));
                        frow(ui, "Over top bid by", &mut self.flip_form.offset, Some(pos(&snap.offset)), Some(st(1.0, 0.1, 0.0, 1000.0, 1)));
                        ui.horizontal_wrapped(|ui| {   // wraps onto a second line instead of widening the column
                            ui.fx_checkbox(&mut self.flip_form.on_outbid, "Requote buy when outbid").fx_tip("Requotes the buy order as soon as someone bids above it, instead of waiting for the timer. Prices lag the game by 10–20 s.");
                            if self.flip_form.on_outbid { ui.label("at most every"); ui.add(egui::TextEdit::singleline(&mut self.flip_form.gap).text_color(ORANGE).desired_width(46.0)); ui.label("s"); step_buttons(ui, &mut self.flip_form.gap, st(10.0, 5.0, 0.0, 3600.0, 0)); if ui.button("⚡ Instant").on_hover_text("0 s: requote the moment a new price shows you outbid").clicked() { self.flip_form.gap = "0".into(); } }
                        });
                    });
                    panel(ui, "2) Sell Side", None, |ui| {
                        let trades = self.flip_form.exchange == 1;
                        ui.horizontal(|ui| {
                            lbl(ui, "Sell on");
                            egui::ComboBox::from_id_salt("flip_exchange").width(ui.available_width() - 8.0).selected_text(RichText::new(if trades { "Trades menu (NPC)" } else { "Bazaar" }).color(ORANGE).size(19.0)).show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.flip_form.exchange, 0, "Bazaar");
                                ui.selectable_value(&mut self.flip_form.exchange, 1, "Trades menu (NPC)");
                            });
                        });
                        ui.add_enabled_ui(!(trades && self.flip_form.sell_all), |ui| {
                        frow(ui, "Max per sell order", &mut self.flip_form.requote_max_qty, Some(parse_qty(&snap.requote_max_qty).is_some()), Some(st(1000.0, 100.0, 1.0, 1.0e7, 0)))
                            .on_hover_text(if trades { "How many items are sold to the NPC in one go. Filled slices are claimed first (into the sacks if the inventory is full)." } else { "The largest sell offer it will cancel and re-list when undercut. The goods come back to your inventory for a moment, so keep it to what fits." });
                        });
                        if trades {
                            ui.horizontal(|ui| { ui.add_space(184.0); ui.fx_checkbox(&mut self.flip_form.sell_all, "Sell everything I have").fx_tip("No limit per sale: sells all of this item you hold, in the inventory and in your sacks, in one go. That includes items that did not come from this flip."); });
                        }
                        if trades {
                            let npc = npc_sell_of(&self.flip_form.product.trim().to_uppercase().replace(' ', "_"));
                            ui.label(RichText::new(match npc { Some(v) if v > 0.0 => format!("The NPC pays {} each (fixed), no bazaar tax. Sales count towards the {} daily NPC limit; {} used today.", fmt_coins(v), fmt_coins(NPC_SELL_CAP), fmt_coins(self.npc_sold_today())), _ => "Pick an item that an NPC buys to see its price.".into() }).color(DIM).size(15.0));
                        } else {
                            frow(ui, "List under best ask by", &mut self.flip_form.sell_offset, Some(pos(&snap.sell_offset)), Some(st(1.0, 0.1, 0.0, 1000.0, 1)));
                            ui.horizontal(|ui| {
                                lbl(ui, "Floor price");
                                egui::ComboBox::from_id_salt("flip_floor_mode").width(if self.flip_form.floor_on { 150.0 } else { ui.available_width() - 8.0 }).selected_text(RichText::new(if self.flip_form.floor_on { "Fixed price" } else { "None" }).color(ORANGE).size(19.0)).show_ui(ui, |ui| {
                                    ui.selectable_value(&mut self.flip_form.floor_on, false, "None");
                                    ui.selectable_value(&mut self.flip_form.floor_on, true, "Fixed price");
                                });
                                if self.flip_form.floor_on {
                                    let w = (ui.available_width() - 18.0 - STEP_ROW_W).max(80.0);
                                    ui.add(egui::TextEdit::singleline(&mut self.flip_form.floor).text_color(ORANGE).font(FontId::proportional(20.0)).desired_width(w).margin(egui::vec2(8.0, 5.0)).hint_text("lowest price to list at"));
                                    step_buttons(ui, &mut self.flip_form.floor, st(10.0, 1.0, 0.0, 1.0e9, 1));
                                }
                            });
                        }
                        frow(ui, "Min margin %", &mut self.flip_form.margin, Some(pos(&snap.margin)), Some(st(1.0, 0.1, 0.0, 500.0, 2)));
                        if !trades {
                            ui.horizontal_wrapped(|ui| {   // wraps onto a second line instead of widening the column
                                ui.fx_checkbox(&mut self.flip_form.undercut, "Re-list when undercut").fx_tip("When someone lists below your sell offer, cancels it and lists again under them (never below the floor / margin). The goods come back to your inventory for a moment, so only offers up to the size above are re-listed.");
                                if self.flip_form.undercut { ui.label("at most every"); ui.add(egui::TextEdit::singleline(&mut self.flip_form.undercut_gap).text_color(ORANGE).desired_width(46.0)); ui.label("s,"); step_buttons(ui, &mut self.flip_form.undercut_gap, st(30.0, 10.0, 0.0, 3600.0, 0)); if ui.button("⚡ Instant").on_hover_text("0 s: relist the moment a new price shows you undercut").clicked() { self.flip_form.undercut_gap = "0".into(); } }
                            });
                        }
                    });
                    panel(ui, "3) Repeat", None, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Cycles").size(18.0));
                            ui.add(egui::TextEdit::singleline(&mut self.flip_form.cycles).text_color(ORANGE).font(FontId::proportional(20.0)).desired_width(70.0));
                            step_buttons(ui, &mut self.flip_form.cycles, st(10.0, 1.0, 0.0, 1000.0, 0));
                            ui.vertical(|ui| {
                                ui.label(RichText::new("0 = keep going until you stop it.").color(DIM).size(15.0));
                                ui.label(RichText::new("One cycle = one buy of \"Buy per cycle\" units.").color(DIM).size(15.0));
                            });
                        });
                        ui.add_space(6.0);
                        ui.label(RichText::new("Pick an item").size(17.0));
                        let cur = self.flip_form.product.clone();
                        egui::ComboBox::from_id_salt("flip_pick_item").width(ui.available_width() - 8.0).selected_text(RichText::new(if cur.is_empty() { "—".to_string() } else { pretty_s(&cur) }).color(Color32::WHITE).size(19.0)).show_ui(ui, |ui| {
                            for w in self.watch.clone() { if ui.selectable_label(w == cur, pretty_s(&w)).clicked() { self.flip_form.product = w.clone(); self.selected = Some(w); } }
                        });
                        ui.add_space(8.0);
                        let parsed = self.flip_parse_form();
                        if let Err(e) = &parsed { ui.label(RichText::new(e).color(DIM).size(15.0)); }
                        ui.horizontal(|ui| {
                            let btn = egui::Button::new(RichText::new("▶  Start Auto Flip").size(22.0).color(GREEN)).fill(Color32::from_rgb(8, 36, 8)).stroke(Stroke::new(1.5, GREEN)).min_size(egui::vec2(260.0, 44.0));
                            if ui.add_enabled(parsed.is_ok(), btn).clicked() {
                                if let Ok(j) = parsed { if j.total as f64 * j.max_price > CONFIRM_ABOVE { self.flip_confirm = Some(j); } else { self.flip_add(j); self.flip_form.msg = Some((true, "started".into())); } }
                            }
                            if !self.agent_up { ui.label(RichText::new("agent offline: it starts once the agent connects").color(ORANGE)); }
                            if let Some((ok, m)) = &self.flip_form.msg { ui.label(RichText::new(m).color(if *ok { GREEN } else { RED })); }
                        });
                    });
                });
                ui.vertical(|ui| {
                    ui.set_width(ui.available_width());
                    self.flip_summary_panels(ui);
                });
            });
        });
    }

    /// Items matching what was typed in the Item field, best first.
    fn flip_search_hits(&self, q: &str) -> Vec<String> { let mut v = self.search_items(q); v.truncate(7); v }

    /// The numbers the right-hand column shows for what is in the form right now.
    fn flip_projection(&self) -> Option<(FlipJob, f64, f64, f64, f64)> {
        let j = self.flip_parse_form().ok()?;
        let (bid, ask) = self.products.get(&j.product).map_or((0.0, 0.0), |p| (p.bid(), p.ask()));
        if bid <= 0.0 || ask <= 0.0 { return None; }
        let buy = (bid + j.offset).min(j.max_price);
        let sell = if j.exchange == 1 { npc_sell_of(&j.product).unwrap_or(0.0) } else { self.flip_sell_price(&j, buy) };
        let u = j.total as f64;
        let (cost, revenue) = (buy * u, sell * u);
        Some((j.clone(), buy, sell, cost, revenue))
    }

    fn flip_summary_panels(&mut self, ui: &mut egui::Ui) {
        let proj = self.flip_projection();
        let eta = proj.as_ref().and_then(|(j, ..)| self.flip_eta(j));
        panel(ui, "Summary", None, |ui| {
            let f = &self.flip_form;
            match &proj {
                Some((j, buy, sell, cost, revenue)) => {
                    let tax = revenue * j.tax_pct / 100.0;
                    let profit = revenue - tax - cost;
                    srow(ui, "Buy size (per cycle)", grouped(j.total), Color32::WHITE);
                    srow(ui, "Max per order", grouped(j.slice), Color32::WHITE);
                    srow(ui, &format!("Est. buy cost ({:.1})", buy), fmt_coins(*cost), Color32::WHITE);
                    srow(ui, &format!("Est. sell price ({:.1})", sell), fmt_coins(*revenue), Color32::WHITE);
                    if j.exchange == 1 { srow(ui, "Tax (NPC)", "none".into(), Color32::WHITE); } else { srow(ui, &format!("Bazaar tax ({}%)", j.tax_pct), format!("−{}", fmt_coins(tax)), RED); }
                    ui.separator();
                    srow(ui, "Est. profit / cycle", format!("{}{}", if profit < 0.0 { "−" } else { "+" }, fmt_coins(profit.abs())), if profit < 0.0 { RED } else { GREEN });
                    srow(ui, "Profit margin", format!("{}{:.2}%", if profit < 0.0 { "−" } else { "+" }, (profit / cost * 100.0).abs()), if profit < 0.0 { RED } else { GREEN });
                    match &eta {
                        Some(e) => {
                            ui.separator();
                            srow(ui, &format!("Place buy orders ({} × ~{:.0} s)", e.slices, PLACE_SECS), fmt_eta(e.place_secs), Color32::WHITE);
                            srow(ui, "Buy fills (7-day volume)", fmt_eta((e.buy_secs - e.place_secs).max(0.0)), Color32::WHITE);
                            if e.trades {
                                srow(ui, &format!("Claim from escrow ({} × ~{:.0} s)", e.slices, FLIP_SECS), fmt_eta(e.flip_secs), Color32::WHITE);
                                srow(ui, &format!("Sell to the NPC (~{:.0} items/s)", TRADES_UNITS_PER_SEC), fmt_eta(e.sell_secs), Color32::WHITE);
                            } else {
                                srow(ui, &format!("Flip to sell offers ({} × ~{:.0} s)", e.slices, FLIP_SECS), fmt_eta(e.flip_secs), Color32::WHITE);
                                srow(ui, "Sell fills (7-day volume)", fmt_eta(e.sell_secs), Color32::WHITE);
                            }
                            ui.separator();
                            srow(ui, "Est. time per cycle", fmt_eta(e.total_secs), ORANGE);
                            srow(ui, "Est. profit / hour", { let ph = profit / (e.total_secs / 3600.0).max(1e-9); format!("{}{}", if ph < 0.0 { "−" } else { "+" }, fmt_coins(ph.abs())) }, if profit < 0.0 { RED } else { GREEN });
                        }
                        None => { ui.label(RichText::new("No time estimate: this item has no recent trading volume.").color(DIM).size(15.0)); }
                    }
                }
                None => {
                    srow(ui, "Buy size (per cycle)", if f.total.is_empty() { "—".into() } else { f.total.clone() }, Color32::WHITE);
                    ui.label(RichText::new("Fill in the item, size and max price to see the estimate (needs live prices).").color(DIM).size(15.0));
                }
            }
        });
        // projected profit over time: placing orders, buy fills, flipping and sell fills, for the cycles you set (0 = shows three)
        let n_cyc = self.flip_form.cycles.trim().parse::<u32>().ok().map(|c| if c == 0 { 3 } else { c.min(1_000_000) }).unwrap_or(1) as usize;
        let series: Option<(Vec<(f64, f64)>, f64, f64)> = eta.as_ref().map(|e| {
            let tmax = (n_cyc - 1) as f64 * e.gap_secs + e.total_secs;
            let pts: Vec<(f64, f64)> = (0..=160).map(|k| { let t = tmax * k as f64 / 160.0; (t, projected_profit(&e.curve, e.gap_secs, e.total_secs, n_cyc, t)) }).collect();
            let top = pts.last().map_or(0.0, |p| p.1);
            (pts, tmax, top)
        });
        panel(ui, "P&L (Projected)", Some(&match &eta { Some(e) => format!("{} cycle(s) · {}", n_cyc, fmt_eta((n_cyc - 1) as f64 * e.gap_secs + e.total_secs)), None => "—".into() }), |ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 200.0), egui::Sense::hover());
            let p = ui.painter_at(rect);
            let plot = egui::Rect::from_min_max(rect.min + egui::vec2(62.0, 26.0), rect.max - egui::vec2(10.0, 24.0));
            if let Some((pts, tmax, top)) = &series {
                let (lo, hi) = (top.min(0.0), top.max(0.0));
                let span = (hi - lo).max(1.0);
                let y = |v: f64| plot.bottom() - ((v - lo) / span) as f32 * plot.height();
                let x = |t: f64| plot.left() + plot.width() * (t / tmax.max(1.0)) as f32;
                for k in 0..=4 {
                    let v = lo + span * k as f64 / 4.0;
                    p.line_segment([egui::pos2(plot.left(), y(v)), egui::pos2(plot.right(), y(v))], Stroke::new(1.0, Color32::from_gray(45)));
                    p.text(egui::pos2(plot.left() - 6.0, y(v)), egui::Align2::RIGHT_CENTER, fmt_coins(v), FontId::proportional(14.0), Color32::from_gray(190));
                }
                for k in 0..=4 {
                    let t = tmax * k as f64 / 4.0;
                    p.text(egui::pos2(x(t), plot.bottom() + 11.0), if k == 0 { egui::Align2::LEFT_CENTER } else if k == 4 { egui::Align2::RIGHT_CENTER } else { egui::Align2::CENTER_CENTER }, fmt_eta(t), FontId::proportional(14.0), Color32::from_gray(170));
                }
                let col = if *top < 0.0 { RED } else { GREEN };
                let line: Vec<egui::Pos2> = pts.iter().map(|(t, v)| egui::pos2(x(*t), y(*v))).collect();
                for w in line.windows(2) { p.add(egui::Shape::convex_polygon(vec![w[0], w[1], egui::pos2(w[1].x, y(0.0)), egui::pos2(w[0].x, y(0.0))], col.gamma_multiply(0.14), Stroke::NONE)); }
                p.add(egui::Shape::line(line, Stroke::new(2.0, col)));
                p.text(egui::pos2(plot.right(), rect.top() + 10.0), egui::Align2::RIGHT_CENTER, format!("{}{}", if *top < 0.0 { "−" } else { "+" }, fmt_coins(top.abs())), FontId::proportional(18.0), col);
                // where the buying ends, so you can see what is waiting on what
                if let Some(e) = &eta { let bx = x(e.buy_secs); p.line_segment([egui::pos2(bx, plot.top()), egui::pos2(bx, plot.bottom())], Stroke::new(1.0, ORANGE.gamma_multiply(0.6))); p.text(egui::pos2(bx + 4.0, plot.top() + 8.0), egui::Align2::LEFT_CENTER, "buying done", FontId::proportional(13.0), ORANGE); }
            } else {
                p.text(plot.center(), egui::Align2::CENTER_CENTER, "needs a valid setup, live prices and trading volume", FontId::proportional(16.0), DIM);
            }
        });
        panel(ui, "Recent Flips", None, |ui| {
            egui::Grid::new("flip_recent").num_columns(4).spacing([22.0, 5.0]).show(ui, |ui| {
                for t in ["Time", "Side", "Units", "Price"] { ui.label(RichText::new(t).color(Color32::from_gray(190)).size(15.0)); }
                ui.end_row();
                for e in self.flip_log.iter().rev().take(6) {
                    ui.label(RichText::new(utc_hms(e.t)).size(16.0));
                    ui.label(RichText::new(e.side).color(if e.side == "SELL" { GREEN } else { RED }).strong().size(16.0));
                    ui.label(RichText::new(grouped(e.units)).size(16.0)); ui.label(RichText::new(format!("{:.3}", e.price)).size(16.0));
                    ui.end_row();
                }
            });
            if self.flip_log.is_empty() { ui.label(RichText::new("nothing flipped yet").color(DIM)); }
        });
        let live = self.flips.iter().find(|j| j.state == FlipState::Running);
        let running = live.is_some();
        panel(ui, "Status", None, |ui| {
            srow(ui, "State", if running { "Running".into() } else if self.flips.iter().any(|j| j.state == FlipState::Paused) { "Paused".into() } else { "Not running".into() }, if running { GREEN } else { Color32::WHITE });
            srow(ui, "Next action", live.map_or("—".to_string(), |j| if j.note.is_empty() { "—".into() } else { j.note.chars().take(38).collect() }), Color32::WHITE);
            srow(ui, "Cycles done", self.flips.iter().map(|j| j.cycles_done).sum::<u32>().to_string(), Color32::WHITE);
            let tp: f64 = self.flips.iter().map(|j| j.est_profit()).sum();
            srow(ui, "Total P&L (est.)", if self.flips.iter().all(|j| j.flipped_units == 0) { "0.0".into() } else { format!("{}{}", if tp < 0.0 { "−" } else { "+" }, fmt_coins(tp.abs())) }, if tp < 0.0 { RED } else { Color32::WHITE });
        });
    }

    // ---------------------------------------------------------------- 2) Active Flips
    fn flip_active_tab(&mut self, ui: &mut egui::Ui) {
        panel(ui, "Active Flips", None, |ui| self.flip_jobs_ui(ui));
    }

    fn flip_jobs_ui(&mut self, ui: &mut egui::Ui) {
        if self.flips.is_empty() { ui.label(RichText::new("none yet: set one up on the Config tab").color(DIM)); return; }
        let (mut act, mut which) = (None::<u32>, "");
        egui::Grid::new("flip_jobs").striped(true).spacing([18.0, 4.0]).show(ui, |ui| {
            for t in ["#", "Item", "State", "Cycle", "Buying now", "Flipped", "Sold", "Sell offers", "Est. total", "Realized so far", "Requotes", ""] { ui.label(RichText::new(t).color(Color32::WHITE).strong()); }
            ui.end_row();
            for j in &self.flips {
                ui.label(j.id.to_string());
                icon_cell(ui, &self.icons, &j.product, &pretty_s(&j.product));
                let col = match j.state { FlipState::Done => GREEN, FlipState::Stopped | FlipState::Paused => DIM, FlipState::Running => ORANGE };
                ui.label(RichText::new(j.state.label()).color(col)).fx_tip(&j.note);
                ui.label(if j.cycles == 0 { format!("{} / ∞", j.cycles_done) } else { format!("{} / {}", j.cycles_done.min(j.cycles), j.cycles) });
                ui.label(match self.jobs.iter().find(|x| x.id == j.ice_id && j.ice_id != 0) { Some(x) => format!("{} / {} (#{})", grouped((x.filled + x.cur_filled).min(x.total)), grouped(x.total), x.id), None => "—".into() });
                ui.label(grouped(j.flipped_units));
                ui.label(if j.real_units > 0 { grouped(j.real_units) } else { "—".into() }).fx_tip("Units the NPC really paid for (counted from the sales this flip made, since this was added)");
                let sells: Vec<&OrderRow> = self.orders.iter().filter(|r| r.side == "SELL" && same_product(&r.product, &j.product)).collect();
                if j.exchange == 1 { ui.label(if j.backlog > 0 { format!("NPC · {} to sell", grouped(j.backlog)) } else { "NPC".to_string() }); } else
                { ui.label(if sells.is_empty() { "—".to_string() } else { format!("{} @ {}", sells.len(), sells.iter().filter_map(|r| r.price).fold(f64::INFINITY, f64::min)).replace("inf", "?") }); }
                let p = j.est_total().unwrap_or(0.0);
                ui.label(RichText::new(if j.flipped_units == 0 { "—".to_string() } else { format!("{}{}", if p < 0.0 { "−" } else { "+" }, fmt_coins(p.abs())) }).color(if p < 0.0 { RED } else { Color32::WHITE })).fx_tip(format!("Projection for the whole flip: {:.1} coins per unit so far x all {} planned units. A finished or stopped flip shows what it did. What has really been earned so far is in Realized.", j.est_profit() / j.flipped_units.max(1) as f64, grouped((j.total * j.cycles.max(1) as u64).max(j.flipped_units))));
                match j.realized() {
                    Some(r) => { ui.label(RichText::new(format!("{}{}", if r < 0.0 { "−" } else { "+" }, fmt_coins(r.abs()))).color(if r < 0.0 { RED } else { GREEN }).strong()).fx_tip(format!("Earned: {} coins paid for {} units, minus their average cost of {:.1} each", fmt_coins(j.real_coins), grouped(j.real_units), j.buy_cost / j.flipped_units.max(1) as f64)); }
                    None => { ui.label(RichText::new("—").color(DIM)).fx_tip("Nothing sold yet, or sold before this was tracked, or sold on the bazaar (not tracked)"); }
                }
                ui.label(j.sell_requotes.to_string());
                ui.horizontal(|ui| {
                    match j.state {
                        FlipState::Running => { if ui.fx_small_button("Pause").clicked() { act = Some(j.id); which = "pause"; } }
                        FlipState::Paused => { if ui.fx_small_button("Resume").clicked() { act = Some(j.id); which = "resume"; } }
                        _ => {}
                    }
                    if matches!(j.state, FlipState::Running | FlipState::Paused) && ui.fx_small_button("Stop").fx_tip("Stops buying and flipping. Sell offers already listed stay in game.").clicked() { act = Some(j.id); which = "stop"; }
                    if matches!(j.state, FlipState::Done | FlipState::Stopped) && ui.fx_small_button("Remove").clicked() { act = Some(j.id); which = "remove"; }
                });
                ui.end_row();
            }
        });
        for j in &self.flips { if !j.note.is_empty() && j.state != FlipState::Done { ui.label(RichText::new(format!("#{}: {}", j.id, j.note)).color(DIM).size(15.0)); } }
        if let Some(id) = act { self.flip_op(id, which); }
    }

    // ---------------------------------------------------------------- 3) History, 4) Analytics
    fn flip_history_tab(&mut self, ui: &mut egui::Ui) {
        let mut cycles = self.flip_cycles.clone();
        if cycles.iter().any(|c| c.v < 2) { repair_legacy_cycles(&mut cycles); }   // rows an older server saved wrongly are shown corrected until the server repairs them itself
        let mut h = std::mem::take(&mut self.flip_hist);
        let mut flips: Vec<(u32, String)> = vec![]; for c in &cycles { if !flips.iter().any(|f| f.0 == c.id) { flips.push((c.id, c.product.clone())); } }
        if let Some(f) = h.flip { if !flips.iter().any(|x| x.0 == f) { h.flip = None; } }
        let mut rows: Vec<&FlipCycle> = cycles.iter().filter(|c| h.flip.map_or(true, |f| f == c.id) && match h.result { 1 => c.profit >= 0.0, 2 => c.profit < 0.0, _ => true }).collect();
        // ---- summary of what is shown
        let (units, profit): (u64, f64) = (rows.iter().map(|c| c.units).sum(), rows.iter().map(|c| c.profit).sum());
        let cost: f64 = rows.iter().map(|c| c.buy_avg * c.units as f64).sum();
        // cycles overlap, so their times cannot be added up: the wall time of a flip is its first start to its last finish
        let (mut wall, mut plan) = (0.0f64, 0.0f64);
        for (id, _) in &flips {
            let r: Vec<&&FlipCycle> = rows.iter().filter(|c| c.id == *id).collect();
            if r.is_empty() { continue; }
            let s0 = r.iter().map(|c| c.t - c.actual_secs).fold(f64::MAX, f64::min); let s1 = r.iter().map(|c| c.t).fold(0.0, f64::max);
            wall += (s1 - s0).max(0.0);
            // projected: one cycle's total plus the buy time of every further cycle (they overlap the sells of the one before)
            let first = r.iter().min_by_key(|c| c.n).unwrap();
            plan += first.proj_secs + (r.len() as f64 - 1.0).max(0.0) * first.proj_buy.max(0.0);
        }
        let wins = rows.iter().filter(|c| c.profit >= 0.0).count();
        panel(ui, "History", Some(&format!("{} of {} cycle(s)", rows.len(), cycles.len())), |ui| {
            if cycles.is_empty() { ui.label(RichText::new("No finished cycle yet. A cycle is finished once everything it bought has been flipped and sold.").color(DIM)); return; }
            ui.horizontal_wrapped(|ui| {
                let pc = |c: Color32| c;
                for (k, v, col) in [("Profit", format!("{}{}", if profit < 0.0 { "−" } else { "+" }, fmt_coins(profit.abs())), if profit < 0.0 { RED } else { GREEN }),
                    ("Cycles", format!("{} ({} won)", rows.len(), wins), Color32::WHITE), ("Units", grouped(units), Color32::WHITE),
                    ("Avg / cycle", if rows.is_empty() { "—".into() } else { fmt_coins(profit / rows.len() as f64) }, Color32::WHITE),
                    ("Margin", if cost > 0.0 { format!("{:.2}%", profit / cost * 100.0) } else { "—".into() }, Color32::WHITE),
                    ("Profit / hour", if wall > 60.0 { fmt_coins(profit / wall * 3600.0) } else { "—".into() }, Color32::WHITE),
                    ("Took", fmt_eta(wall), Color32::WHITE),
                    ("Plan said", if plan > 0.0 { fmt_eta(plan) } else { "—".into() }, DIM),
                    ("vs plan", if plan > 0.0 && wall > 0.0 { format!("{:+.0}%", (wall / plan - 1.0) * 100.0) } else { "—".into() }, if plan > 0.0 && wall <= plan { GREEN } else { ORANGE })] {
                    ui.vertical(|ui| { ui.label(RichText::new(k).color(DIM).size(14.0)); ui.label(RichText::new(v).color(pc(col)).size(20.0).strong()); });
                    ui.add_space(14.0);
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Flip").color(DIM));
                egui::ComboBox::from_id_salt("hist_flip").selected_text(match h.flip { None => "All".to_string(), Some(f) => format!("#{} {}", f, flips.iter().find(|x| x.0 == f).map_or(String::new(), |x| pretty_s(&x.1))) }).show_ui(ui, |ui| {
                    ui.selectable_value(&mut h.flip, None, "All");
                    for (id, pr) in &flips { ui.selectable_value(&mut h.flip, Some(*id), format!("#{} {}", id, pretty_s(pr))); }
                });
                ui.label(RichText::new("Result").color(DIM));
                for (k, t) in [(0u8, "All"), (1, "Wins"), (2, "Losses")] { if ui.selectable_label(h.result == k, t).clicked() { h.result = k; } }
                ui.add_space(12.0);
                for (k, t) in [(0u8, "Table"), (1, "Profit over time"), (2, "Plan vs actual")] { if ui.selectable_label(h.view == k, RichText::new(t).size(17.0)).clicked() { h.view = k; } }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { if ui.fx_small_button("Export CSV").clicked() { h.export = true; } });
            });
            ui.add_space(4.0);
            match h.view {
                1 => {
                    let mut by_t: Vec<&FlipCycle> = rows.clone(); by_t.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
                    let mut acc = 0.0; let pts: Vec<(f64, f64)> = by_t.iter().map(|c| { acc += c.profit; (c.t, acc) }).collect();
                    hist_line_chart(ui, &pts);
                }
                2 => {
                    let mut by_t: Vec<&FlipCycle> = rows.clone(); by_t.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
                    let bars: Vec<(String, f64, f64)> = by_t.iter().map(|c| (format!("#{}·{}", c.id, c.n), c.proj_secs, c.actual_secs)).collect();
                    hist_bar_chart(ui, &bars);
                }
                _ => {}
            }
            // ---- sorting
            let key = |c: &FlipCycle, col: u8| -> f64 { match col {
                0 => c.id as f64 * 1e4 + c.n as f64, 1 => c.t, 2 => c.units as f64, 3 => c.buy_avg, 4 => c.list_avg,
                5 => if c.buy_avg > 0.0 { c.profit / (c.buy_avg * c.units as f64) } else { 0.0 }, 6 => c.profit,
                7 => if c.actual_secs > 0.0 { c.profit / c.actual_secs } else { 0.0 }, 8 => c.proj_secs, 9 => c.actual_secs,
                10 => if c.proj_secs > 0.0 { c.actual_secs / c.proj_secs } else { 0.0 }, _ => c.actual_buy } };
            rows.sort_by(|a, b| key(a, h.sort).partial_cmp(&key(b, h.sort)).unwrap_or(std::cmp::Ordering::Equal));
            if !h.asc { rows.reverse(); }
            egui::ScrollArea::vertical().id_salt("flip_hist").max_height(ui.available_height().max(300.0)).show(ui, |ui| {
                egui::Grid::new("flip_hist_grid").striped(true).spacing([18.0, 7.0]).show(ui, |ui| {
                    for (col, t) in ["Flip · cycle", "Finished (UTC)", "Units", "Buy avg", "List avg", "Margin", "Profit", "Profit/h", "Plan", "Actual", "vs plan", "Buying"].iter().enumerate() {
                        let arrow = if h.sort == col as u8 { if h.asc { " ▲" } else { " ▼" } } else { "" };
                        if ui.add(egui::Label::new(RichText::new(format!("{t}{arrow}")).color(Color32::WHITE).strong()).sense(egui::Sense::click())).clicked() {
                            if h.sort == col as u8 { h.asc = !h.asc; } else { h.sort = col as u8; h.asc = false; }
                        }
                    }
                    ui.end_row();
                    for c in &rows {
                        let key_row = (c.id, c.n);
                        let open = h.open == Some(key_row);
                        let margin = if c.buy_avg > 0.0 { c.profit / (c.buy_avg * c.units as f64) * 100.0 } else { 0.0 };
                        let ratio = if c.proj_secs > 0.0 { Some(c.actual_secs / c.proj_secs) } else { None };
                        let r = ui.add(egui::Label::new(RichText::new(format!("{}#{} · {}", if open { "▾ " } else { "▸ " }, c.id, c.n)).size(17.0)).sense(egui::Sense::click()));
                        if r.clicked() { h.open = if open { None } else { Some(key_row) }; }
                        ui.label(RichText::new(format!("{} {}", utc_date(c.t), utc_hms(c.t))).size(16.0));
                        ui.label(grouped(c.units)); ui.label(format!("{:.1}", c.buy_avg)); ui.label(format!("{:.1}", c.list_avg));
                        ui.label(RichText::new(format!("{margin:.2}%")).color(if margin < 0.0 { RED } else { GREEN }));
                        ui.label(RichText::new(format!("{}{}", if c.profit < 0.0 { "−" } else { "+" }, fmt_coins(c.profit.abs()))).color(if c.profit < 0.0 { RED } else { GREEN }).strong());
                        ui.label(if c.actual_secs > 0.0 { fmt_coins(c.profit / c.actual_secs * 3600.0) } else { "—".into() });
                        ui.label(RichText::new(if c.proj_secs > 0.0 { fmt_eta(c.proj_secs) } else { "—".into() }).color(DIM));
                        ui.label(fmt_eta(c.actual_secs));
                        ui.label(RichText::new(match ratio { Some(r) => format!("{:+.0}%", (r - 1.0) * 100.0), None => "—".into() }).color(match ratio { Some(r) if r <= 1.0 => GREEN, Some(_) => ORANGE, None => DIM }));
                        ui.label(RichText::new(fmt_eta(c.actual_buy)).color(DIM));
                        ui.end_row();
                        if open {
                            ui.label(""); 
                            ui.vertical(|ui| {
                                let flip_part = (c.actual_secs - c.actual_buy).max(0.0); let plan_flip = (c.proj_secs - c.proj_buy).max(0.0);
                                ui.label(RichText::new(format!("{}: bought {} @ {:.1}, listed @ {:.1} ", pretty_s(&c.product), grouped(c.units), c.buy_avg, c.list_avg)).size(15.0));
                                ui.label(RichText::new(format!("Buying: plan {} · took {}.   Flipping and selling: plan {} · took {}.", fmt_eta(c.proj_buy), fmt_eta(c.actual_buy), fmt_eta(plan_flip), fmt_eta(flip_part))).color(DIM).size(15.0));
                                ui.label(RichText::new(format!("Speed: {:.0} units/min bought and sold; per unit profit {:.2}", if c.actual_secs > 0.0 { c.units as f64 / c.actual_secs * 60.0 } else { 0.0 }, if c.units > 0 { c.profit / c.units as f64 } else { 0.0 })).color(DIM).size(15.0));
                                if c.v < 2 { ui.label(RichText::new("older row").color(DIM).size(14.0)); }
                                // the orders this cycle placed, from the trade log (kept for the latest 100 events only)
                                let ev: Vec<&FlipEvent> = self.flip_log.iter().filter(|e| e.id == c.id && e.t >= c.t - c.actual_secs - 5.0 && e.t <= c.t + 5.0).collect();
                                if ev.is_empty() { ui.label(RichText::new("The detailed trades of this cycle are no longer in the log.").color(DIM).size(14.0)); }
                                for e in ev.iter().take(14) { ui.label(RichText::new(format!("{}  {}  {} × {:.1}", utc_hms(e.t), e.side, grouped(e.units), e.price)).size(14.0).color(if e.side == "BUY" { Color32::from_gray(200) } else { GREEN })); }
                                if ev.len() > 14 { ui.label(RichText::new(format!("… and {} more", ev.len() - 14)).color(DIM).size(14.0)); }
                            });
                            for _ in 0..11 { ui.label(""); }
                            ui.end_row();
                        }
                    }
                });
            });
        });
        if h.export {
            h.export = false;
            let mut csv = String::from("flip,cycle,finished_utc,item,units,buy_avg,list_avg,profit,plan_secs,actual_secs,buy_plan_secs,buy_actual_secs\n");
            for c in &cycles { csv.push_str(&format!("{},{},{} {},{},{},{:.2},{:.2},{:.0},{:.0},{:.0},{:.0},{:.0}\n", c.id, c.n, utc_date(c.t), utc_hms(c.t), c.product, c.units, c.buy_avg, c.list_avg, c.profit, c.proj_secs, c.actual_secs, c.proj_buy, c.actual_buy)); }
            let p = bb::profile::data_dir().join("autoflip_cycles.csv");
            if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
            match std::fs::write(&p, csv) { Ok(_) => { let _ = self.toast_new(1, format!("Cycles exported: {}", p.display())); } Err(e) => { let _ = self.toast_new(2, format!("Export failed: {e}")); } }
        }
        self.flip_hist = h;
    }

    fn flip_analytics_tab(&mut self, ui: &mut egui::Ui) {
        panel(ui, "Analytics", None, |ui| {
            if self.flips.is_empty() { ui.label(RichText::new("no auto flips yet").color(DIM)); return; }
            egui::Grid::new("flip_an").striped(true).spacing([26.0, 5.0]).show(ui, |ui| {
                for t in ["Flip", "Item", "Cycles", "Units flipped", "Units sold", "Avg buy", "Avg list", "Est. total", "Realized so far", "Margin", "Sell re-lists"] { ui.label(RichText::new(t).color(Color32::WHITE).strong()); }
                ui.end_row();
                let (mut tu, mut tc, mut tv, mut tt) = (0u64, 0.0f64, 0.0f64, 0.0f64);
                for j in &self.flips {
                    let u = j.flipped_units as f64;
                    ui.label(format!("#{}", j.id)); icon_cell(ui, &self.icons, &j.product, &pretty_s(&j.product));
                    ui.label(j.cycles_done.to_string()); ui.label(grouped(j.flipped_units));
                    ui.label(if j.real_units > 0 { grouped(j.real_units) } else { "—".into() });
                    ui.label(if u > 0.0 { format!("{:.1}", j.buy_cost / u) } else { "—".into() });
                    ui.label(if u > 0.0 { format!("{:.1}", j.list_value / u / (1.0 - j.tax_pct / 100.0)) } else { "—".into() });
                    let p = j.est_profit();
                    let pt = j.est_total().unwrap_or(0.0);
                    ui.label(RichText::new(if u > 0.0 { format!("{}{}", if pt < 0.0 { "−" } else { "+" }, fmt_coins(pt.abs())) } else { "—".into() }).color(if pt < 0.0 { RED } else { GREEN }));
                    match j.realized() { Some(r) => { ui.label(RichText::new(format!("{}{}", if r < 0.0 { "−" } else { "+" }, fmt_coins(r.abs()))).color(if r < 0.0 { RED } else { GREEN }).strong()); } None => { ui.label(RichText::new("—").color(DIM)); } }
                    ui.label(if j.buy_cost > 0.0 { format!("{:.2}%", p / j.buy_cost * 100.0) } else { "—".into() });
                    ui.label(j.sell_requotes.to_string());
                    ui.end_row();
                    tu += j.flipped_units; tc += j.buy_cost; tv += j.list_value; tt += j.est_total().unwrap_or(0.0);
                }
                ui.label(RichText::new("Total").strong()); ui.label(""); ui.label(""); ui.label(RichText::new(grouped(tu)).strong()); ui.label(RichText::new(grouped(self.flips.iter().map(|j| j.real_units).sum())).strong()); ui.label(""); ui.label("");
                let p = tv - tc;
                ui.label(RichText::new(format!("{}{}", if tt < 0.0 { "−" } else { "+" }, fmt_coins(tt.abs()))).strong().color(if tt < 0.0 { RED } else { GREEN }));
                let rl: f64 = self.flips.iter().filter_map(|j| j.realized()).sum();
                ui.label(RichText::new(if self.flips.iter().any(|j| j.realized().is_some()) { format!("{}{}", if rl < 0.0 { "−" } else { "+" }, fmt_coins(rl.abs())) } else { "—".into() }).strong().color(if rl < 0.0 { RED } else { GREEN }));
                ui.label(if tc > 0.0 { format!("{:.2}%", p / tc * 100.0) } else { "—".into() }); ui.label("");
                ui.end_row();
            });
            ui.add_space(4.0);
            ui.label(RichText::new("Est. total = what the whole flip should make: profit per unit so far times all units planned (a finished or stopped flip shows what it did). Realized so far = coins the NPC really paid for the units it bought, minus their average cost (tracked from the sales made since this was added).").color(DIM).size(15.0));
        });
    }

    // ---------------------------------------------------------------- 5) Save, 6) Load, 7) Export
    /// Saved auto flip settings are shared by every account: one file in the main profile's folder.
    fn flip_preset_path() -> std::path::PathBuf { bb::profile::shared_data_dir().join("autoflip_presets.json") }
    fn flip_read_presets(path: &std::path::Path) -> Vec<(String, Value)> {
        std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.as_array().map(|a| a.iter().filter_map(|p| Some((p["name"].as_str()?.to_string(), p["form"].clone()))).collect())).unwrap_or_default()
    }
    fn flip_presets() -> Vec<(String, Value)> {
        let mut list = Self::flip_read_presets(&Self::flip_preset_path());
        // presets an account saved in its own folder before they were shared move into the shared file (once)
        let own = bb::profile::data_dir().join("autoflip_presets.json");
        if own != Self::flip_preset_path() && own.exists() {
            for (n, f) in Self::flip_read_presets(&own) { if !list.iter().any(|(m, _)| *m == n) { list.push((n, f)); } }
            Self::flip_write_presets(&list);
            let _ = std::fs::rename(&own, own.with_extension("json.moved"));
        }
        list
    }
    fn flip_write_presets(list: &[(String, Value)]) {
        let p = Self::flip_preset_path();
        if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
        let v: Vec<Value> = list.iter().map(|(n, f)| json!({"name": n, "form": f})).collect();
        let _ = std::fs::write(p, Value::Array(v).to_string());
    }
    fn flip_form_json(f: &FlipForm) -> Value {
        json!({"product": f.product, "total": f.total, "slice": f.slice, "min": f.min, "max": f.max, "offset": f.offset, "requote": f.requote, "on_outbid": f.on_outbid, "gap": f.gap,
            "sell_offset": f.sell_offset, "floor": f.floor, "floor_on": f.floor_on, "margin": f.margin, "tax": f.tax, "undercut": f.undercut, "undercut_gap": f.undercut_gap,
            "requote_max_qty": f.requote_max_qty, "cycles": f.cycles, "exchange": f.exchange, "sell_all": f.sell_all})
    }
    fn flip_form_from(v: &Value) -> FlipForm {
        let mut f = FlipForm::new();
        let s = |k: &str, d: &str| v[k].as_str().unwrap_or(d).to_string();
        f.product = s("product", ""); f.total = s("total", ""); f.slice = s("slice", &f.slice.clone()); f.min = s("min", "0.1"); f.max = s("max", ""); f.offset = s("offset", "0.1"); f.requote = s("requote", "5");
        f.on_outbid = v["on_outbid"].as_bool().unwrap_or(true); f.gap = s("gap", "30"); f.sell_offset = s("sell_offset", "0.1"); f.floor = s("floor", ""); f.floor_on = v["floor_on"].as_bool().unwrap_or(false);
        f.margin = s("margin", "2"); f.tax = s("tax", "1.25"); f.undercut = v["undercut"].as_bool().unwrap_or(true); f.undercut_gap = s("undercut_gap", "60"); f.requote_max_qty = s("requote_max_qty", "2000"); f.cycles = s("cycles", "1");
        f.buy_mode = 1; f.exchange = v["exchange"].as_u64().unwrap_or(0).min(1) as u8; f.sell_all = v["sell_all"].as_bool().unwrap_or(false);
        f
    }
    fn flip_save_preset(&mut self) {
        let name = format!("{} {}", if self.flip_form.product.is_empty() { "flip".to_string() } else { pretty_s(&self.flip_form.product) }, utc_hms(now_secs()));
        let mut list = Self::flip_presets();
        list.push((name.clone(), Self::flip_form_json(&self.flip_form)));
        Self::flip_write_presets(&list);
        let _ = self.toast_new(1, format!("Saved preset “{name}”"));
    }
    fn flip_load_dialog(&mut self, ctx: &egui::Context) {
        let mut list = Self::flip_presets();
        let (mut pick, mut del, mut close) = (None::<usize>, None::<usize>, false);
        egui::Window::new("Load auto flip settings").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            if list.is_empty() { ui.label(RichText::new("No saved presets yet. Use 5) Save.").color(DIM)); }
            for (i, (n, _)) in list.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(n).size(17.0));
                    if ui.fx_small_button("Load").clicked() { pick = Some(i); }
                    if ui.fx_small_button("Delete").clicked() { del = Some(i); }
                });
            }
            ui.add_space(6.0);
            if ui.fx_button("  Close  ").clicked() { close = true; }
        });
        if let Some(i) = pick { self.flip_form = Self::flip_form_from(&list[i].1); self.flip_filled_for = self.flip_form.product.trim().to_uppercase().replace(' ', "_"); close = true; }
        if let Some(i) = del { list.remove(i); Self::flip_write_presets(&list); }
        if close { self.flip_load_dlg = false; }
    }
    fn flip_export(&mut self) {
        let mut csv = String::from("time_utc,flip,item,side,units,price\n");
        for e in &self.flip_log { csv.push_str(&format!("{},{},{},{},{},{}\n", utc_hms(e.t), e.id, e.product, e.side, e.units, e.price)); }
        let p = bb::profile::data_dir().join("autoflip_history.csv");
        if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
        match std::fs::write(&p, csv) { Ok(_) => { let _ = self.toast_new(1, format!("History exported: {}", p.display())); } Err(e) => { let _ = self.toast_new(2, format!("Export failed: {e}")); } }
    }

    /// Parses the form into a job (used by Start and the summary).
    pub fn flip_parse_form(&self) -> Result<FlipJob, String> {
        let f = &self.flip_form;
        let q = f.product.trim().to_uppercase().replace(' ', "_");
        if q.is_empty() { return Err("pick an item".into()); }
        let id = if self.products.contains_key(&q) { q.clone() } else {
            let mut ids: Vec<&String> = self.products.keys().filter(|k| k.contains(&q)).collect();
            ids.sort_by_key(|k| k.len());
            match ids.first() { Some(s) => s.to_string(), None => return Err(format!("no product matching {q}")) }
        };
        let num = |s: &str, what: &str| s.trim().parse::<f64>().ok().filter(|v| *v >= 0.0 && v.is_finite()).ok_or(format!("enter {what}"));
        let opt = |s: &str, d: f64, what: &str| if s.trim().is_empty() { Ok(d) } else { num(s, what) };
        let mut j = FlipJob::new(id);
        j.total = parse_qty(&f.total).ok_or("enter the quantity to buy per cycle, e.g. 50k")?;
        j.slice = if f.slice.trim().is_empty() { MAX_ORDER_QTY } else { parse_qty(&f.slice).ok_or("enter the max per order")? }.min(MAX_ORDER_QTY);
        j.min_price = opt(&f.min, 0.1, "a minimum buy price")?; j.max_price = num(&f.max, "a maximum buy price")?;
        if j.max_price <= 0.0 || j.max_price < j.min_price { return Err("max buy price must be above the min".into()); }
        j.offset = opt(&f.offset, 0.1, "the offset over the top bid")?;
        j.requote_secs = 3600;   // the timer is only a safety net: buy orders are requoted when outbid
        j.on_outbid = f.on_outbid;
        j.outbid_gap = if f.gap.trim().is_empty() { 30 } else { f.gap.trim().parse::<u64>().ok().ok_or("the outbid gap is a number of seconds (0 = instant)")? };
        j.exchange = f.exchange.min(1); j.sell_all = f.sell_all && j.exchange == 1;
        if j.exchange == 1 && npc_sell_of(&j.product).filter(|v| *v > 0.0).is_none() { return Err("no NPC buys this item: sell it on the Bazaar".into()); }
        j.sell_offset = opt(&f.sell_offset, 0.1, "how far under the best ask to list")?;
        j.floor = if f.floor_on { opt(&f.floor, 0.0, "a floor price")? } else { 0.0 };
        j.margin_pct = opt(&f.margin, 0.0, "the minimum margin in %")?;
        j.tax_pct = if j.exchange == 1 { 0.0 } else { 1.25 };   // the bazaar tax cannot be changed; the NPC takes none
        j.undercut = f.undercut;
        j.undercut_gap = if f.undercut_gap.trim().is_empty() { 60 } else { f.undercut_gap.trim().parse::<u64>().ok().ok_or("the undercut gap is a number of seconds (0 = instant)")? };
        j.requote_max_qty = if f.requote_max_qty.trim().is_empty() { 2000 } else { parse_qty(&f.requote_max_qty).ok_or("enter the largest sell offer to re-list")? };
        j.cycles = if f.cycles.trim().is_empty() { 1 } else { f.cycles.trim().parse::<u32>().map_err(|_| "cycles is a whole number (0 = until stopped)".to_string())? };
        Ok(j)
    }
}
