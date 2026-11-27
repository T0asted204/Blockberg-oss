//! The Portfolio screen: net worth, where it sits, how it moved. Included from gui.rs (`mod portfolio`).
//!
//! Net worth = purse + bank + every item we hold (inventory and sacks, at the bazaar bid: what an instant sell pays)
//! + what is tied up in open orders (coins locked in buy orders, items listed in sell offers, anything waiting to be
//! claimed). The engine (server) writes one point every 5 minutes to `networth.jsonl`; this screen draws that file.
use super::*;
use std::io::Write;

const BLUE: Color32 = Color32::from_rgb(70, 170, 255);
const VIOLET: Color32 = Color32::from_rgb(176, 120, 255);
const CYAN: Color32 = Color32::from_rgb(60, 220, 200);
const RECORD_EVERY: f64 = 300.0;

/// One recorded moment.
#[derive(Clone, Copy, Default, Debug)]
pub struct WPoint { pub t: f64, pub purse: f64, pub bank: f64, pub items: f64, pub orders: f64 }
impl WPoint { pub fn total(&self) -> f64 { self.purse + self.bank + self.items + self.orders } }

/// One item we hold somewhere.
pub struct Holding { pub id: String, pub inv: u64, pub sack: u64, pub esc: u64, pub bid: f64 }
impl Holding {
    pub fn qty(&self) -> u64 { self.inv + self.sack + self.esc }
    pub fn value(&self) -> f64 { self.qty() as f64 * self.bid }
}

pub struct Worth { pub pt: WPoint, pub holdings: Vec<Holding>, pub sacks_value: f64 }

pub const RANGES: [(&str, f64); 5] = [("6H", 6.0 * 3600.0), ("1D", 86400.0), ("3D", 3.0 * 86400.0), ("7D", 7.0 * 86400.0), ("ALL", f64::MAX)];

fn signed(x: f64) -> String { format!("{}{}", if x >= 0.0 { "+" } else { "−" }, fmt_coins(x.abs())) }
fn ago_short(secs: f64) -> String {
    let s = secs.max(0.0);
    if s < 90.0 { "now".into() } else if s < 5400.0 { format!("{:.0}m ago", s / 60.0) } else if s < 172800.0 { format!("{:.0}h ago", s / 3600.0) } else { format!("{:.1}d ago", s / 86400.0) }
}

fn path() -> Option<std::path::PathBuf> {
    Some(crate::bb::profile::data_dir().join("networth.jsonl"))
}

pub fn load_points() -> Vec<WPoint> {
    parse_points(path().and_then(|p| std::fs::read_to_string(p).ok()))
}

fn parse_points(text: Option<String>) -> Vec<WPoint> {
    let Some(text) = text else { return vec![] };
    let mut v: Vec<WPoint> = text.lines().filter_map(|l| {
        let j: Value = serde_json::from_str(l).ok()?;
        Some(WPoint { t: j["t"].as_f64()?, purse: j["p"].as_f64()?, bank: j["b"].as_f64().unwrap_or(0.0), items: j["i"].as_f64().unwrap_or(0.0), orders: j["o"].as_f64().unwrap_or(0.0) })
    }).collect();
    v.sort_by(|a, b| a.t.total_cmp(&b.t));
    v
}

/// The other accounts of the co-op, as their own engines last recorded them (same machine: their data folders are read directly).
#[derive(Clone, Default)]
pub struct OtherWorth { pub name: String, pub pt: WPoint, pub holdings: Vec<(String, u64, u64, u64)>, pub sacks_value: f64, pub points: Vec<WPoint> }

pub fn load_others() -> Vec<OtherWorth> {
    let me = crate::bb::profile::name();
    crate::bb::profile::all().into_iter().filter(|e| e.name != me).filter_map(|e| {
        let dir = crate::bb::profile::data_dir_of(&e.name);
        let points = parse_points(std::fs::read_to_string(dir.join("networth.jsonl")).ok());
        let pt = *points.last()?;
        let hv: Option<Value> = std::fs::read_to_string(dir.join("worth_holdings.json")).ok().and_then(|t| serde_json::from_str(&t).ok());
        let holdings = hv.as_ref().and_then(|v| v["holdings"].as_array()).map(|a| a.iter().filter_map(|h| Some((h["id"].as_str()?.to_string(), h["inv"].as_u64().unwrap_or(0), h["sack"].as_u64().unwrap_or(0), h["esc"].as_u64().unwrap_or(0)))).collect()).unwrap_or_default();
        let sacks_value = hv.as_ref().and_then(|v| v["sacks_value"].as_f64()).unwrap_or(0.0);
        Some(OtherWorth { name: e.name, pt, holdings, sacks_value, points })
    }).collect()
}

/// The whole co-op over time: at every recorded moment, each account's latest point (carried forward: an account that is offline still owns what it had) added up. The bank is shared by the co-op,
/// so it is counted once: the one read most recently.
pub fn merge_coop(own: &[WPoint], others: &[OtherWorth]) -> Vec<WPoint> {
    if others.is_empty() { return own.to_vec(); }
    let mut series: Vec<&[WPoint]> = vec![own];
    for o in others { series.push(&o.points); }
    let mut times: Vec<f64> = series.iter().flat_map(|s| s.iter().map(|p| p.t)).collect();
    times.sort_by(|a, b| a.total_cmp(b)); times.dedup_by(|a, b| (*a - *b).abs() < 90.0);   // two accounts record a minute or so apart: one moment
    let mut idx = vec![0usize; series.len()];
    let mut out = Vec::with_capacity(times.len());
    for t in times {
        let (mut purse, mut items, mut orders, mut bank, mut bank_t) = (0.0, 0.0, 0.0, 0.0, f64::MIN);
        let mut any = false;
        for (k, s) in series.iter().enumerate() {
            while idx[k] < s.len() && s[idx[k]].t <= t + 90.0 { idx[k] += 1; }
            if idx[k] == 0 { continue; }
            let p = s[idx[k] - 1];
            any = true; purse += p.purse; items += p.items; orders += p.orders;
            if p.t > bank_t { bank_t = p.t; bank = p.bank; }
        }
        if any { out.push(WPoint { t, purse, bank, items, orders }); }
    }
    out
}

/// The holdings behind the last point, for the other accounts' Portfolio screens.
fn write_holdings(w: &Worth) {
    let Some(path) = path() else { return };
    let Some(dir) = path.parent() else { return };
    let hs: Vec<Value> = w.holdings.iter().map(|h| json!({"id": h.id, "inv": h.inv, "sack": h.sack, "esc": h.esc})).collect();
    let _ = std::fs::write(dir.join("worth_holdings.json"), json!({"t": w.pt.t.round(), "holdings": hs, "sacks_value": w.sacks_value}).to_string());
}

fn append_point(p: &WPoint) {
    let Some(path) = path() else { return };
    if let Some(d) = path.parent() { let _ = std::fs::create_dir_all(d); }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}", json!({"t": p.t.round(), "p": p.purse.round(), "b": p.bank.round(), "i": p.items.round(), "o": p.orders.round()}));
    }
}

/// Net worth at time `t`: the last point at or before it.
fn value_at(h: &[WPoint], t: f64) -> Option<f64> {
    let i = h.partition_point(|p| p.t <= t);
    if i == 0 { None } else { Some(h[i - 1].total()) }
}

impl Gui {
    /// The whole co-op now: this account live, the others as their engines last recorded them (an account whose agent is off keeps what it last had). The bank is one account for all of them.
    pub fn worth_coop(&self) -> Option<Worth> {
        let mut w = self.worth_now()?;
        let now = w.pt.t;
        let mut map: std::collections::BTreeMap<String, Holding> = std::mem::take(&mut w.holdings).into_iter().map(|h| (h.id.clone(), h)).collect();
        for o in &self.worth_others {
            w.pt.purse += o.pt.purse; w.pt.items += o.pt.items; w.pt.orders += o.pt.orders; w.sacks_value += o.sacks_value;
            if w.pt.bank <= 0.0 { w.pt.bank = o.pt.bank; }
            for (id, inv, sack, esc) in &o.holdings {
                let bid = self.products.get(id).map_or(0.0, |p| p.bid());
                let h = map.entry(id.clone()).or_insert_with(|| Holding { id: id.clone(), inv: 0, sack: 0, esc: 0, bid });
                h.inv += inv; h.sack += sack; h.esc += esc;
            }
        }
        w.holdings = map.into_values().collect();
        Some(w)
    }

    /// What we are worth right now, from the live data. `None` until prices and the purse are known.
    pub fn worth_now(&self) -> Option<Worth> {
        let purse = self.purse?;
        if self.products.is_empty() { return None; }
        let bid = |id: &str| self.products.get(id).map(|p| p.bid()).filter(|b| *b > 0.0);
        let mut map: std::collections::BTreeMap<String, Holding> = Default::default();
        let mut add = |id: &str, f: &dyn Fn(&mut Holding)| { if let Some(b) = bid(id) { f(map.entry(id.to_string()).or_insert_with(|| Holding { id: id.to_string(), inv: 0, sack: 0, esc: 0, bid: b })); } };
        for i in &self.inv_all { add(&i.id, &|h| h.inv += i.count); }
        let mut sacks_value = 0.0;
        // the Hypixel API's sack counts are the whole truth and need no walk through the sacks; the in-game scan is the fallback
        let api = bb::SACK_API.lock().ok().and_then(|g| g.as_ref().map(|(_, m)| m.clone()));
        let from_sacks: Vec<(String, u64)> = match api {
            Some(m) => m.into_iter().collect(),
            None => self.sack_rows().into_iter().map(|r| (r.name.to_uppercase().replace(' ', "_"), r.stored)).collect(),
        };
        for (id, n) in from_sacks {
            if n > 0 { add(&id, &|h| h.sack += n); if let Some(b) = bid(&id) { sacks_value += n as f64 * b; } }
        }
        let mut orders = 0.0;
        for o in &self.orders {
            let id = o.product.to_uppercase().replace(' ', "_");
            let price = o.price.unwrap_or(0.0);
            let open = o.total.saturating_sub(o.filled) as f64;
            if o.side == "BUY" {
                orders += open * price;                       // coins locked in the order
                if o.to_claim > 0 { add(&id, &|h| h.esc += o.to_claim); }
            } else {
                if let Some(b) = bid(&id) { orders += open * b; } // still listed
                if o.claimable { orders += o.filled as f64 * price; } // sold, coins waiting to be claimed
            }
        }
        let holdings: Vec<Holding> = map.into_values().collect();
        let items: f64 = holdings.iter().map(|h| h.value()).sum();
        let pt = WPoint { t: now_secs(), purse, bank: self.bank.unwrap_or(0.0), items, orders };
        Some(Worth { pt, holdings, sacks_value })
    }

    /// Engine: write a point every 5 minutes, but only from data that is recent enough to trust.
    pub fn worth_tick(&mut self) {
        if self.mock || !self.is_engine() || self.worth_rec_at.elapsed().as_secs_f64() < 20.0 { return; }
        if self.worth_last_t == 0.0 { self.worth_last_t = load_points().last().map_or(1.0, |p| p.t); } // after a restart, carry on from the file
        let due = now_secs() - self.worth_last_t >= RECORD_EVERY;
        if !due { self.worth_rec_at = Instant::now(); return; }
        self.worth_rec_at = Instant::now();
        // the API's sack counts arrive with the first bank read; wait for them (up to 10 minutes) so the sacks are not left out
        if bb::SACK_API.lock().map_or(true, |g| g.is_none()) && self.boot.elapsed().as_secs() < 600 { return; }
        let fresh = |t: Option<Instant>, max: u64| t.map_or(false, |t| t.elapsed().as_secs() < max);
        // The purse is live, but the inventory and the order list are separate reads: when they are minutes old, coins that just left the purse for an order (or an
        // item that just left the inventory) are missing from the other side and the total dips, then jumps back. So a point needs everything read within 2 minutes,
        // and nothing in motion (a claim, a sale or an order half way). After 20 minutes of that not working it is recorded anyway.
        let overdue = now_secs() - self.worth_last_t >= 20.0 * 60.0;
        let (win_i, win_o) = if overdue { (600, 1800) } else { (120, 120) };
        let moving = !self.inflight.is_empty() || self.flips.iter().any(|f| f.pending.is_some())
            || self.busy(&["claimorder", "claim", "cancel", "fliporder", "requote_sell", "tradesellqty", "tradesell", "buyorder", "selloffer", "requote", "instabuy", "instasell", "insert_sacks", "pickup", "sackget", "sellinv"]);
        if moving && !overdue { return; }
        let (inv_ok, ord_ok) = (fresh(self.inv_all_at, win_i), fresh(self.orders_at, win_o));
        if !inv_ok || !ord_ok {
            // ask the agent for what is stale, when it is idle; the point is written on a later tick
            if self.agent_up && !self.halted && self.inflight.is_empty() {
                if !inv_ok { let _ = self.cmd_tx.send(json!({"cmd": "invall", "auto": true})); }
                if !ord_ok { let _ = self.cmd_tx.send(json!({"cmd": "orders", "auto": true})); self.orders_at = Some(Instant::now()); }
            }
            return;
        }
        if let Some(w) = self.worth_now() {
            append_point(&w.pt);
            write_holdings(&w);
            self.worth_last_t = w.pt.t;
            self.worth_hist_at = None; // re-read when the screen is open
        }
    }

    fn worth_reload(&mut self) {
        if self.mock {
            // screenshots / mock: a made-up week, never written anywhere
            if self.worth_hist.is_empty() {
                let now = now_secs();
                self.worth_hist = (0..7 * 288).map(|i| { let t = now - (7 * 288 - i) as f64 * 300.0; let x = i as f64;
                    WPoint { t, purse: 700_000.0 + 90_000.0 * (x / 40.0).sin(), bank: 1_500_000.0 + x * 900.0, items: 2_000_000.0 + x * 2_500.0 + 400_000.0 * (x / 150.0).sin(), orders: 600_000.0 + 150_000.0 * (x / 70.0).cos() } }).collect();
            }
            return;
        }
        if self.worth_hist_at.map_or(true, |t| t.elapsed().as_secs() > 15) {
            let own = load_points();
            if let Some(l) = own.last() { self.worth_last_t = self.worth_last_t.max(l.t); }
            self.worth_others = load_others();
            self.worth_hist = merge_coop(&own, &self.worth_others);
            self.worth_hist_at = Some(Instant::now());
        }
    }

    pub fn portfolio_screen(&mut self, ctx: &egui::Context) {
        self.worth_reload();
        self.screen_frame(ctx, "Portfolio  PORT", |g, ui| g.portfolio_body(ui));
    }

    fn portfolio_body(&mut self, ui: &mut egui::Ui) {
        let Some(w) = self.worth_coop() else {
            ui.add_space(20.0);
            ui.label(RichText::new(if !self.agent_up { "Waiting for prices and your purse. The agent is offline, so the purse is unknown." } else { "Waiting for prices and your purse…" }).color(DIM));
            return;
        };
        let total = w.pt.total();
        if self.mock && self.worth_hist.last().map_or(false, |l| l.total() > 0.0) {
            // mock only: scale the made-up week so it ends where the live value is
            let f = total / self.worth_hist.last().unwrap().total();
            for p in self.worth_hist.iter_mut() { p.purse *= f; p.bank *= f; p.items *= f; p.orders *= f; }
        }
        // history plus the live value as the last point
        let mut hist = self.worth_hist.clone();
        hist.push(w.pt);
        let now = w.pt.t;

        // ---- hero ----
        let shown = fx::count_to(ui.ctx(), egui::Id::new("worth_total"), total);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(if self.worth_others.is_empty() { "NET WORTH".to_string() } else { format!("NET WORTH · WHOLE CO-OP ({} + {}{})", crate::bb::profile::name(), self.worth_others.iter().map(|o| o.name.clone()).collect::<Vec<_>>().join(" + "),
                    self.worth_others.iter().filter(|o| w.pt.t - o.pt.t > 900.0).map(|o| format!("; {} last read {} ago", o.name, ago_short(w.pt.t - o.pt.t))).collect::<String>()) }).color(DIM).size(18.0));
                ui.label(RichText::new(format!("{} coins", grouped(shown.max(0.0).round() as u64))).size(46.0).color(Color32::WHITE).family(egui::FontFamily::Name("terminal-bold".into())));
                ui.horizontal_wrapped(|ui| {
                    for (label, secs) in [("1h", 3600.0), ("24h", 86400.0), ("7d", 7.0 * 86400.0)] {
                        let base = if hist.first().map_or(false, |f| f.t > now - secs) { None } else { value_at(&hist, now - secs) };
                        match base {
                            Some(b) => {
                                let d = total - b;
                                let c = if d >= 0.0 { GREEN } else { RED };
                                chip(ui, label, &format!("{}  {:+.1}%", signed(d), if b.abs() > 1.0 { d / b * 100.0 } else { 0.0 }), c);
                            }
                            None => chip(ui, label, "collecting…", DIM),
                        }
                    }
                });
            });
        });
        ui.add_space(10.0);

        // ---- where it sits ----
        let parts = [("PURSE", w.pt.purse, ORANGE, "Coins in hand"), ("BANK", w.pt.bank, BLUE, "Coins in the bank"),
            ("ITEMS", w.pt.items, GREEN, "Inventory + sacks + goods waiting in escrow, at the bazaar bid"), ("IN ORDERS", w.pt.orders, VIOLET, "Coins locked in buy orders, items listed in sell offers, coins waiting to be claimed")];
        let gap = 8.0;
        let tw = ((ui.available_width() - gap * 3.0 - 14.0) / 4.0).max(120.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for (name, v, c, tip) in parts {
                let pct = if total > 0.0 { v / total * 100.0 } else { 0.0 };
                let r = egui::Frame::NONE.fill(Color32::from_rgb(12, 12, 12)).stroke(Stroke::new(1.0, Color32::from_gray(60))).inner_margin(egui::Margin::symmetric(10, 8)).corner_radius(3).show(ui, |ui| {
                    ui.set_width(tw - 22.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new(name).color(c).size(17.0));
                        ui.label(RichText::new(fmt_coins(v)).size(30.0).color(Color32::WHITE).family(egui::FontFamily::Name("terminal-bold".into())));
                        ui.label(RichText::new(format!("{pct:.1}% of net worth")).color(DIM).size(16.0));
                    });
                });
                let rect = r.response.rect;
                ui.painter().rect_filled(egui::Rect::from_min_size(rect.left_top(), egui::vec2(3.0, rect.height())), 1.0, c);
                fx::tip_rect(ui, rect, ("worth_tile", name), tip);
            }
        });
        ui.add_space(6.0);
        // allocation bar
        {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 10.0), egui::Sense::hover());
            let mut x = rect.left();
            for (_, v, c, _) in parts {
                let wd = if total > 0.0 { (v / total) as f32 * rect.width() } else { 0.0 };
                if wd > 0.5 { ui.painter().rect_filled(egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2((wd - 1.0).max(0.5), rect.height())), 2.0, c); }
                x += wd;
            }
        }
        ui.add_space(14.0);

        // ---- chart ----
        ui.horizontal(|ui| {
            ui.label(RichText::new("Net worth over time").color(ORANGE).size(20.0));
            ui.add_space(12.0);
            for (i, (name, _)) in RANGES.iter().enumerate() {
                if ui.selectable_label(self.worth_range == i, *name).clicked() { self.worth_range = i; }
            }
            if let Some(f) = self.worth_hist.first() { ui.label(RichText::new(format!("recording since {}", ago_short(now - f.t))).color(DIM).size(16.0)); }
        });
        let span = RANGES[self.worth_range].1;
        let from = if span == f64::MAX { 0.0 } else { now - span };
        let pts: Vec<WPoint> = hist.iter().filter(|p| p.t >= from).cloned().collect();
        self.worth_chart(ui, &pts, now);
        ui.add_space(14.0);

        // ---- profit by day + periods ----
        let half = (ui.available_width() - 16.0) / 2.0;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 16.0;
            ui.vertical(|ui| {
                ui.set_width(half);
                ui.label(RichText::new("Change per day").color(ORANGE).size(20.0));
                days_chart(ui, &hist, now, half);
            });
            ui.vertical(|ui| {
                ui.set_width(half);
                ui.label(RichText::new("Summary").color(ORANGE).size(20.0));
                self.summary(ui, &hist, now, total);
            });
        });
        ui.add_space(14.0);

        // ---- holdings ----
        ui.label(RichText::new("What you hold").color(ORANGE).size(20.0));
        let mut hs: Vec<&Holding> = w.holdings.iter().filter(|h| h.value() >= 1.0).collect();
        hs.sort_by(|a, b| b.value().total_cmp(&a.value()));
        if hs.is_empty() { ui.label(RichText::new("No items held, or the inventory has not been read yet.").color(DIM)); }
        let top = hs.first().map(|h| h.value()).unwrap_or(1.0).max(1.0);
        {
            let (hr, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 22.0), egui::Sense::hover());
            let mut x = hr.left();
            for (wd, t) in [(260.0, "Item"), (110.0, "Inventory"), (110.0, "Sacks"), (110.0, "Escrow"), (110.0, "Bid"), (120.0, "Value")] {
                ui.painter().text(egui::pos2(x + 2.0, hr.center().y), egui::Align2::LEFT_CENTER, t, FontId::proportional(16.0), DIM);
                x += wd;
            }
        }
        for h in hs.iter().take(25) {
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 26.0), egui::Sense::hover());
            if resp.hovered() { ui.painter().rect_filled(rect, 2.0, Color32::from_rgb(20, 20, 20)); }
            let frac = (h.value() / top) as f32;
            ui.painter().rect_filled(egui::Rect::from_min_size(egui::pos2(rect.left(), rect.bottom() - 3.0), egui::vec2(rect.width() * frac, 2.0)), 1.0, GREEN.gamma_multiply(0.45));
            let mut x = rect.left();
            let mut cell = |ui: &mut egui::Ui, wd: f32, f: &dyn Fn(&mut egui::Ui)| {
                let r = egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(wd, 24.0));
                ui.scope_builder(egui::UiBuilder::new().max_rect(r).layout(egui::Layout::left_to_right(egui::Align::Center)), |ui| f(ui));
                x += wd;
            };
            let num = |n: u64| if n == 0 { RichText::new("–").color(Color32::from_gray(90)) } else { RichText::new(grouped(n)) };
            cell(ui, 260.0, &|ui| icon_cell(ui, &self.icons, &h.id, &pretty(&h.id)));
            cell(ui, 110.0, &|ui| { ui.label(num(h.inv)); });
            cell(ui, 110.0, &|ui| { ui.label(num(h.sack)); });
            cell(ui, 110.0, &|ui| { ui.label(num(h.esc)); });
            cell(ui, 110.0, &|ui| { ui.label(RichText::new(format!("{:.1}", h.bid)).color(DIM)); });
            cell(ui, 120.0, &|ui| { ui.label(RichText::new(fmt_coins(h.value())).color(Color32::WHITE)); });
        }
        ui.add_space(10.0);
        ui.label(RichText::new("Items are valued at the bazaar bid (what an instant sell pays), so this is what you could cash out now. History is recorded by the server every 5 minutes, including while the app is closed. Money moved to or from the bank counts in the totals, not as profit.").color(DIM).size(15.0));
    }

    fn summary(&self, ui: &mut egui::Ui, hist: &[WPoint], now: f64, total: f64) {
        let rows = [("Last hour", 3600.0), ("Last 6 hours", 6.0 * 3600.0), ("Last 24 hours", 86400.0), ("Last 3 days", 3.0 * 86400.0), ("Last 7 days", 7.0 * 86400.0)];
        for (name, secs) in rows {
            ui.horizontal(|ui| {
                ui.allocate_ui(egui::vec2(160.0, 22.0), |ui| ui.label(name));
                if hist.first().map_or(true, |f| f.t > now - secs) {
                    ui.label(RichText::new("not enough history yet").color(Color32::from_gray(90)));
                } else if let Some(b) = value_at(hist, now - secs) {
                    let d = total - b;
                    ui.label(RichText::new(format!("{}   {:+.2}%", signed(d), if b.abs() > 1.0 { d / b * 100.0 } else { 0.0 })).color(if d >= 0.0 { GREEN } else { RED }));
                }
            });
        }
        if let (Some(f), true) = (hist.first(), hist.len() > 1) {
            let d = total - f.total();
            ui.horizontal(|ui| {
                ui.allocate_ui(egui::vec2(160.0, 22.0), |ui| ui.label(format!("Since recording ({})", ago_short(now - f.t).replace(" ago", ""))));
                ui.label(RichText::new(format!("{}   {:+.2}%", signed(d), if f.total().abs() > 1.0 { d / f.total() * 100.0 } else { 0.0 })).color(if d >= 0.0 { GREEN } else { RED }));
            });
            let (hi, lo) = hist.iter().fold((f64::MIN, f64::MAX), |(h, l), p| (h.max(p.total()), l.min(p.total())));
            ui.horizontal(|ui| { ui.allocate_ui(egui::vec2(160.0, 22.0), |ui| ui.label("Peak")); ui.label(RichText::new(fmt_coins(hi)).color(Color32::WHITE)); });
            ui.horizontal(|ui| { ui.allocate_ui(egui::vec2(160.0, 22.0), |ui| ui.label("Lowest")); ui.label(RichText::new(fmt_coins(lo)).color(Color32::WHITE)); });
        }
        ui.add_space(4.0);
        let (buys, sells) = (self.trades.iter().filter(|t| t.side == "BUY" && matches!(t.status, Status::Filled)).count(), self.trades.iter().filter(|t| t.side == "SELL" && matches!(t.status, Status::Filled)).count());
        ui.horizontal(|ui| { ui.allocate_ui(egui::vec2(160.0, 22.0), |ui| ui.label("This session")); ui.label(RichText::new(format!("{buys} buys, {sells} sells filled · {} spent today", fmt_coins(self.spent_today))).color(DIM)); });
    }

    fn worth_chart(&self, ui: &mut egui::Ui, pts: &[WPoint], now: f64) {
        let h = 300.0;
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 3.0, Color32::from_rgb(8, 8, 8));
        if pts.len() < 2 {
            painter.text(rect.center(), egui::Align2::CENTER_CENTER, "Collecting history: a point is recorded every 5 minutes.\nCome back in a little while and this fills in.", FontId::proportional(20.0), DIM);
            return;
        }
        // down-sample for drawing
        let stride = pts.len() / 700 + 1;
        let mut dp: Vec<&WPoint> = pts.iter().step_by(stride).collect();
        if let Some(l) = pts.last() { if dp.last().map_or(true, |d| d.t != l.t) { dp.push(l); } }
        let (t0, t1) = (dp[0].t, dp[dp.len() - 1].t.max(dp[0].t + 1.0));
        let (mut lo, mut hi) = dp.iter().fold((f64::MAX, f64::MIN), |(l, h), p| (l.min(p.total()), h.max(p.total())));
        if hi - lo < 1.0 { lo -= 1.0; hi += 1.0; }
        let pad = (hi - lo) * 0.1; lo -= pad; hi += pad;
        let inner = egui::Rect::from_min_max(rect.min + egui::vec2(74.0, 12.0), rect.max - egui::vec2(12.0, 26.0));
        let px = |t: f64| inner.left() + ((t - t0) / (t1 - t0)) as f32 * inner.width();
        let py = |v: f64| inner.bottom() - ((v - lo) / (hi - lo)) as f32 * inner.height();
        // grid + axis labels
        for k in 0..=4 {
            let v = lo + (hi - lo) * k as f64 / 4.0;
            let y = py(v);
            painter.line_segment([egui::pos2(inner.left(), y), egui::pos2(inner.right(), y)], Stroke::new(1.0, Color32::from_gray(32)));
            painter.text(egui::pos2(inner.left() - 8.0, y), egui::Align2::RIGHT_CENTER, fmt_coins(v), FontId::proportional(15.0), DIM);
        }
        for k in 0..=3 {
            let t = t0 + (t1 - t0) * k as f64 / 3.0;
            let al = if k == 0 { egui::Align2::LEFT_TOP } else if k == 3 { egui::Align2::RIGHT_TOP } else { egui::Align2::CENTER_TOP };
            painter.text(egui::pos2(px(t), inner.bottom() + 6.0), al, ago_short(now - t), FontId::proportional(15.0), DIM);
        }
        let up = dp[dp.len() - 1].total() >= dp[0].total();
        let col = if up { GREEN } else { RED };
        // draw-in animation the first time the screen opens
        let reveal = fx::tween_from(ui.ctx(), egui::Id::new(("worth_reveal", self.worth_range)), 0.0, 1.0, 0.7);
        let n_show = ((dp.len() as f32 * reveal).ceil() as usize).clamp(2, dp.len());
        let line: Vec<egui::Pos2> = dp.iter().take(n_show).map(|p| egui::pos2(px(p.t), py(p.total()))).collect();
        let mut mesh = egui::Mesh::default();
        let (top, clear) = (Color32::from_rgba_unmultiplied(col.r(), col.g(), col.b(), 80), Color32::from_rgba_unmultiplied(col.r(), col.g(), col.b(), 0));
        for (i, p) in line.iter().enumerate() {
            mesh.colored_vertex(*p, top);
            mesh.colored_vertex(egui::pos2(p.x, inner.bottom()), clear);
            if i > 0 { let b = (i * 2) as u32; mesh.add_triangle(b - 2, b - 1, b); mesh.add_triangle(b - 1, b + 1, b); }
        }
        painter.add(egui::Shape::mesh(mesh));
        painter.add(egui::Shape::line(line.clone(), Stroke::new(2.0, col)));
        if let Some(last) = line.last() { painter.circle_filled(*last, 4.0, col); }
        // hover read-out
        if let Some(pos) = resp.hover_pos() {
            if inner.x_range().contains(pos.x) {
                let t = t0 + ((pos.x - inner.left()) / inner.width()) as f64 * (t1 - t0);
                let i = dp.partition_point(|p| p.t < t).min(dp.len() - 1);
                let p = dp[i];
                let (x, y) = (px(p.t), py(p.total()));
                painter.line_segment([egui::pos2(x, inner.top()), egui::pos2(x, inner.bottom())], Stroke::new(1.0, Color32::from_gray(90)));
                painter.circle_filled(egui::pos2(x, y), 5.0, Color32::WHITE);
                let d = p.total() - dp[0].total();
                let text = format!("{}\n{}\n{} from the start of this range", ago_short(now - p.t), grouped(p.total().max(0.0).round() as u64), signed(d));
                let g = painter.layout(text, FontId::proportional(17.0), Color32::WHITE, 300.0);
                let mut at = egui::pos2(x + 12.0, inner.top() + 6.0);
                if at.x + g.size().x + 12.0 > rect.right() { at.x = x - 12.0 - g.size().x - 10.0; }
                painter.rect_filled(egui::Rect::from_min_size(at - egui::vec2(6.0, 4.0), g.size() + egui::vec2(12.0, 8.0)), 3.0, Color32::from_rgb(25, 25, 25));
                painter.galley(at, g, Color32::WHITE);
            }
        }
    }
}

fn chip(ui: &mut egui::Ui, label: &str, text: &str, c: Color32) {
    egui::Frame::NONE.fill(Color32::from_rgb(14, 14, 14)).stroke(Stroke::new(1.0, c.gamma_multiply(0.5))).inner_margin(egui::Margin::symmetric(8, 3)).corner_radius(10).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(label).color(DIM).size(16.0));
            ui.label(RichText::new(text).color(c).size(18.0));
        });
    });
}

/// Seven 24-hour blocks ending now: how much net worth moved in each.
fn days_chart(ui: &mut egui::Ui, hist: &[WPoint], now: f64, width: f32) {
    let h = 190.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, h), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, Color32::from_rgb(8, 8, 8));
    let first_t = hist.first().map_or(now, |f| f.t);
    // oldest block first
    let mut bars: Vec<(String, Option<f64>)> = vec![];
    for k in (0..7).rev() {
        let (a, b) = (now - (k as f64 + 1.0) * 86400.0, now - k as f64 * 86400.0);
        let label = if k == 0 { "last 24h".to_string() } else { format!("{}d ago", k) };
        let end = value_at(hist, b);
        let start = value_at(hist, a).or_else(|| if first_t < b { hist.first().map(|f| f.total()) } else { None });
        bars.push((label, match (start, end) { (Some(s), Some(e)) if first_t < b - 600.0 => Some(e - s), _ => None }));
    }
    let vals: Vec<f64> = bars.iter().filter_map(|b| b.1).collect();
    if vals.is_empty() { painter.text(rect.center(), egui::Align2::CENTER_CENTER, "No full day recorded yet", FontId::proportional(18.0), DIM); return; }
    let mx = vals.iter().fold(1.0f64, |m, v| m.max(v.abs()));
    let inner = egui::Rect::from_min_max(rect.min + egui::vec2(10.0, 24.0), rect.max - egui::vec2(10.0, 24.0));
    let mid = inner.center().y;
    painter.line_segment([egui::pos2(inner.left(), mid), egui::pos2(inner.right(), mid)], Stroke::new(1.0, Color32::from_gray(50)));
    let bw = inner.width() / bars.len() as f32;
    let grow = fx::tween_from(ui.ctx(), egui::Id::new("days_grow"), 0.0, 1.0, 0.6);
    for (i, (label, v)) in bars.iter().enumerate() {
        let cx = inner.left() + bw * (i as f32 + 0.5);
        painter.text(egui::pos2(cx, rect.bottom() - 6.0), egui::Align2::CENTER_BOTTOM, label, FontId::proportional(14.0), DIM);
        let Some(v) = v else { continue };
        let hh = (v.abs() / mx) as f32 * (inner.height() / 2.0 - 4.0) * grow;
        let c = if *v >= 0.0 { GREEN } else { RED };
        let r = if *v >= 0.0 { egui::Rect::from_min_max(egui::pos2(cx - bw * 0.3, mid - hh), egui::pos2(cx + bw * 0.3, mid)) } else { egui::Rect::from_min_max(egui::pos2(cx - bw * 0.3, mid), egui::pos2(cx + bw * 0.3, mid + hh)) };
        painter.rect_filled(r, 2.0, c.gamma_multiply(0.85));
        let ty = if *v >= 0.0 { r.top() - 2.0 } else { r.bottom() + 2.0 };
        painter.text(egui::pos2(cx, ty), if *v >= 0.0 { egui::Align2::CENTER_BOTTOM } else { egui::Align2::CENTER_TOP }, signed(*v), FontId::proportional(14.0), c);
    }
    let _ = (CYAN, rect);
}
