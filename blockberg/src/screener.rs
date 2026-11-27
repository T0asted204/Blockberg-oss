//! The Screener: every bazaar item in a Category > Family > Item tree (like the in-game bazaar), for quick browsing.
use super::*;
use std::sync::OnceLock;
use crate::autoflip::fmt_eta;
use crate::iceberg::parse_qty;

const TREE_JSON: &str = include_str!("../assets/bazaar_tree.json");
/// Table columns in sort order; the toggle buttons above the table show / hide all but the first.
const COL_NAMES: [&str; 17] = ["Item", "Bid", "Ask", "Spread", "Buy vol 7d", "Sell vol 7d", "NPC sell", "Craft cost", "CNSΔ", "BNSΔ", "CNSMP", "CNSMP-ETA", "BNSMP", "BNSMP-ETA", "PPH", "Score ★", "Life"];
/// The share of the other players' insta-sell flow that a buy order of yours realistically gets (about 60%: your Hard Stone flips take 5–6 h where the full flow would take 3.4 h).
pub const BUY_CAPTURE: f64 = 0.6;
// ---- the flip score (columns PPH and Score ★) ----
/// Capital turns over about once an hour: the most a flip can use is what one hour of its flow costs.
const FLIP_TURNOVER_H: f64 = 1.0;
/// The most items you run at once.
const FLIP_SLOTS: f64 = 5.0;
/// Waiting up to this long to get out of one slot's position is fine; slower than that counts as being trapped.
const FLIP_EXIT_H: f64 = 4.0;
/// Items traded per hour (the thinner side) at which liquidity stops costing score.
const FLIP_FULL_LIQUIDITY: f64 = 500.0;
/// Nothing below this margin is a flip.
/// Margin matters only as a cushion: while you keep your orders at the top of the book the other side undercuts you, and each step eats margin. Fitted on four days of
/// one-minute candles of 27 items (chasing the book for 1 h: buy at the highest bid seen + 0.1, sell at the lowest ask seen − 0.1): the 80th-percentile margin lost in
/// one hour is about exp(-0.01) × spread^0.6 × (0.1 / price)^0.24 (R² 0.76), and the loss grows with time^0.74. The hours until that eats everything above 10% is the
/// opportunity life; a flip needs about LIFE_TARGET_H of it for full credit.
const FLIP_MIN_MARGIN: f64 = 0.10;
const FLIP_TAX: f64 = 0.0125;
/// Selling to the NPC needs the goods in the inventory; an item none of your sacks holds is claimed and sold in inventory-sized rounds, which is slower: its score loses this share.
/// (A bazaar sell offer is flipped straight from escrow and needs neither.) Remove it once there are sacks for everything.
const NO_SACK_NPC: f64 = 0.85;
const ERODE_A: f64 = -0.01;
const ERODE_SPREAD: f64 = 0.6;
const ERODE_TICK: f64 = 0.24;
const ERODE_TIME: f64 = 0.74;
const LIFE_TARGET_H: f64 = 3.0;
/// Coins per hour that earn 1 and 10 stars (log scale in between).
const SCORE_LO: f64 = 100_000.0;
const SCORE_HI: f64 = 30_000_000.0;

/// What the score worked out for one item: the better of buying at bid + 0.1 and selling on the bazaar at ask − 0.1, or selling to the NPC.
#[derive(Clone, Debug)]
pub struct FlipEst { pub no_sack: bool, pub pph: f64, pub score: f64, pub route: &'static str, pub buy: f64, pub sell: f64, pub net: f64, pub margin: f64, pub units_h: f64, pub liq: f64, pub exit: f64, pub mfac: f64, pub capital: f64, pub life: f64, pub erosion: f64 }
const CAT_ORDER: [&str; 6] = ["Farming", "Mining", "Combat", "Woods & Fishes", "Oddities", "Enchantments"];

/// A filter on one table column: keep rows whose value is within [min, max] (either may be empty); `hide_empty` also drops rows with no value ("—").
#[derive(Default, Clone)]
pub struct ColFilter { pub min: String, pub max: String, pub hide_empty: bool }
impl ColFilter {
    fn bounds(&self) -> (Option<f64>, Option<f64>) { (parse_num(&self.min), parse_num(&self.max)) }
    pub fn active(&self) -> bool { let (lo, hi) = self.bounds(); lo.is_some() || hi.is_some() || self.hide_empty }
}

/// "5", "1.5m", "-200k", "12h": a number with an optional k/m/b/t suffix (and a trailing "h" for hours).
fn parse_num(s: &str) -> Option<f64> {
    let t: String = s.trim().to_lowercase().chars().filter(|c| !matches!(c, ',' | '_' | ' ')).collect();
    let t = t.strip_suffix('h').unwrap_or(&t).to_string();
    if t.is_empty() { return None; }
    let (num, mult) = match t.chars().last()? { 'k' => (&t[..t.len() - 1], 1e3), 'm' => (&t[..t.len() - 1], 1e6), 'b' => (&t[..t.len() - 1], 1e9), 't' => (&t[..t.len() - 1], 1e12), _ => (&t[..], 1.0) };
    let v: f64 = num.parse().ok()?;
    if v.is_finite() { Some(v * mult) } else { None }
}

#[derive(Default)]
pub struct ScreenerState {
    pub cat: String, pub fam: String, pub filter: String, pub sort: u8, pub desc: bool, pub hidden: u32,
    /// column filters (all active ones apply at once)
    pub filters: HashMap<u8, ColFilter>,
    /// open right-click menus: (column, where) for a header, (item id, where) for a row
    pub col_menu: Option<(u8, egui::Pos2)>, pub row_menu: Option<(String, egui::Pos2)>,
    /// column values that cost a recipe walk, kept until the next price update
    cache: std::cell::RefCell<HashMap<(u8, String), Option<f64>>>, cache_at: std::cell::Cell<Option<Instant>>,
    /// lower-case names of the items the owned sacks accept, rebuilt every 15 s
    sack_names: std::cell::RefCell<(Option<Instant>, std::collections::HashSet<String>)>,
}

/// category -> [(family, [ids])], in display order. Items the generator did not know are added to Oddities > Other at runtime.
fn tree() -> &'static Vec<(String, Vec<(String, Vec<String>)>)> {
    static T: OnceLock<Vec<(String, Vec<(String, Vec<String>)>)>> = OnceLock::new();
    T.get_or_init(|| {
        let v: Value = serde_json::from_str(TREE_JSON).unwrap_or(Value::Null);
        let Some(o) = v.as_object() else { return vec![] };
        let mut cats: Vec<&String> = o.keys().collect();
        cats.sort_by_key(|c| CAT_ORDER.iter().position(|x| x == c).unwrap_or(99));
        cats.into_iter().map(|c| {
            let mut fams: Vec<(String, Vec<String>)> = o[c].as_object().map(|f| f.iter().map(|(k, ids)| (k.clone(), ids.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default())).collect()).unwrap_or_default();
            fams.sort_by(|a, b| (a.0 == "Other").cmp(&(b.0 == "Other")).then(a.0.cmp(&b.0)));
            (c.clone(), fams)
        }).collect()
    })
}

fn fmt_hours(h: f64) -> String {
    if h < 1.0 { format!("{:.0} min", h * 60.0) } else if h < 100.0 { format!("{h:.1} h") } else { format!("{} h", grouped(h.round() as u64)) }
}

fn nice_family(cat: &str, fam: &str) -> String {
    if cat == "Enchantments" { fam.split('_').map(|w| { let mut c = w.chars(); c.next().map(|f| f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase()).unwrap_or_default() }).collect::<Vec<_>>().join(" ") } else { fam.to_string() }
}

fn craft_recipes() -> &'static HashMap<String, (f64, Vec<(String, f64)>)> {
    static R: OnceLock<HashMap<String, (f64, Vec<(String, f64)>)>> = OnceLock::new();
    R.get_or_init(|| {
        let v: Value = serde_json::from_str(include_str!("../assets/craft_recipes.json")).unwrap_or(Value::Null);
        v.as_object().map(|o| o.iter().map(|(k, x)| (k.clone(), (x[0].as_f64().unwrap_or(1.0),
            x[1].as_array().map(|a| a.iter().filter_map(|i| Some((i[0].as_str()?.to_string(), i[1].as_f64()?))).collect()).unwrap_or_default()))).collect()).unwrap_or_default()
    })
}

impl Gui {
    /// Cost of one `id` from its crafting-table recipe: every bazaar ingredient bought at the ask (`orders` = false) or with a buy order just over the bid (`orders` = true).
    /// Ingredients that are not on the bazaar are priced from an NPC shop or crafted from their own recipe. None when something cannot be priced.
    pub fn craft_cost(&self, id: &str, orders: bool) -> Option<f64> { self.craft_cost_d(id, orders, &mut vec![id.to_string()]) }
    /// `chain` holds the items being crafted on the way here: a recipe that needs one of them is a loop (block <-> 9 items), not a real way to make it.
    fn craft_cost_d(&self, id: &str, orders: bool, chain: &mut Vec<String>) -> Option<f64> {
        let (n, ing) = craft_recipes().get(id)?;
        if chain.len() > 5 { return None; }
        let mut total = 0.0;
        for (a, c) in ing {
            if chain.iter().any(|x| x == a) { return None; }
            let unit = if let Some(p) = self.products.get(a) {
                // buy orders go just above the top bid; with no bids at all (a thin item can have none for a moment) the instant-buy price stands in, so the cost does not blink out
                let u = if orders { if p.bid() > 0.0 { p.bid() + 0.1 } else { p.ask() } } else { p.ask() };
                if u <= 0.0 { return None; } u
            } else if let Some(u) = npc_buy_of(a) { u } else { chain.push(a.clone()); let r = self.craft_cost_d(a, orders, chain); chain.pop(); r? };
            total += unit * c;
        }
        Some(total / n.max(1.0))
    }

    /// The bazaar raw materials one `id` is made of: (product, units per one `id`). Ingredients bought from an NPC shop are left out (unlimited stock).
    fn craft_leaves(&self, id: &str, mult: f64, chain: &mut Vec<String>, out: &mut HashMap<String, f64>) -> Option<()> {
        let (n, ing) = craft_recipes().get(id)?;
        if chain.len() > 5 { return None; }
        for (a, c) in ing {
            if chain.iter().any(|x| x == a) { return None; }
            let q = mult * c / n.max(1.0);
            if self.products.contains_key(a) { *out.entry(a.clone()).or_insert(0.0) += q; }
            else if npc_buy_of(a).is_some() { }
            else { chain.push(a.clone()); let r = self.craft_leaves(a, q, chain, out); chain.pop(); r?; }
        }
        Some(())
    }

    /// Hours to buy the raw materials for CRAFTING as many `id` as the 500M NPC limit takes, with buy orders: each material needs (units × amount per craft), filled at its
    /// 7-day insta-sell volume ÷ 168 per hour, of which you get BUY_CAPTURE; the slowest material decides. Returns (hours, that material, units of it needed).
    pub fn cnsmp_eta(&self, id: &str) -> Option<(f64, String, f64)> {
        let npc = npc_sell_of(id).filter(|v| *v > 0.0)?;
        let units = NPC_SELL_CAP / npc;
        let mut leaves = HashMap::new();
        self.craft_leaves(id, units, &mut vec![id.to_string()], &mut leaves)?;   // only items that can be crafted have a CNSMP
        if leaves.is_empty() { return None; }
        let mut worst: Option<(f64, String, f64)> = None;
        for (a, need) in leaves {
            let per_hour = self.products.get(&a)?.quick_status.sell_moving_week as f64 / 168.0 * BUY_CAPTURE;
            let h = if per_hour > 0.0 { need / per_hour } else { f64::INFINITY };
            if worst.as_ref().map_or(true, |w| h > w.0) { worst = Some((h, a, need)); }
        }
        worst.filter(|w| w.0.is_finite())
    }

    /// Hours to buy as many `id` as the 500M NPC limit takes by buying the ITEM ITSELF with buy orders (no crafting): its 7-day insta-sell volume ÷ 168 per hour, times BUY_CAPTURE.
    pub fn bnsmp_eta(&self, id: &str) -> Option<f64> {
        let npc = npc_sell_of(id).filter(|v| *v > 0.0)?;
        let per_hour = self.products.get(id)?.quick_status.sell_moving_week as f64 / 168.0 * BUY_CAPTURE;
        if per_hour <= 0.0 { return None; }
        Some(NPC_SELL_CAP / npc / per_hour)
    }

    /// Profit per hour and the 1 to 10 star score of flipping `id` (None when it cannot be flipped at a profit).
    /// Units per hour = BUY_CAPTURE × the thinner side's flow (7-day volume ÷ 168); the capital you have limits it to what one hour of that flow costs (opportunity cost);
    /// PPH = profit per unit × that. The score then multiplies by liquidity (items traded per hour), by an exit test (can one slot's position be sold within 4 h) and
    /// by a margin-cushion factor (hours the margin survives being undercut, see ERODE_*), and puts it on a log scale: 100k per hour is 1 star, 30M per hour is 10 stars.
    pub fn flip_est(&self, id: &str) -> Option<FlipEst> {
        let p = self.products.get(id)?;
        let (bid, ask) = (p.bid(), p.ask());
        if bid <= 0.0 { return None; }
        let rb = p.quick_status.sell_moving_week as f64 / 168.0;   // insta-sells per hour: what fills a buy order
        let rs = p.quick_status.buy_moving_week as f64 / 168.0;    // insta-buys per hour: what fills a sell offer
        let capital = { let c = self.purse.unwrap_or(0.0) + self.bank.unwrap_or(0.0); if c > 1_000_000.0 { c } else { 50_000_000.0 } };
        let buy = bid + 0.1;
        let mut best: Option<FlipEst> = None;
        let mut routes: Vec<(&'static str, f64, f64, f64, f64)> = vec![];   // route, sell price, net per unit, units per hour, sell-side flow
        if ask > 0.1 { let sell = ask - 0.1; routes.push(("bazaar", sell, sell * (1.0 - FLIP_TAX) - buy, BUY_CAPTURE * rb.min(rs), rs)); }
        if let Some(n) = npc_sell_of(id).filter(|v| *v > 0.0) { let cap_h = NPC_SELL_CAP / n / 24.0; routes.push(("NPC", n, n - buy, (BUY_CAPTURE * rb).min(cap_h), cap_h)); }
        let (sacks_known, has_sack) = self.item_has_sack(id);
        let spread = if ask > bid { (ask - bid) / bid } else { 0.0 };
        let e1 = (ERODE_A.exp() * spread.max(1e-4).powf(ERODE_SPREAD) * (0.1 / buy).powf(ERODE_TICK)).min(1.0);   // margin lost in 1 h of keeping up at the top of the book (80th percentile)
        for (route, sell, net, u, sell_flow) in routes {
            let margin = net / buy;
            if net <= 0.0 || u <= 0.0 || margin < FLIP_MIN_MARGIN { continue; }
            // an NPC sells at a fixed price: only the buy side gets pushed up
            let e = if route == "NPC" { e1 * 0.5 } else { e1 };
            let life = if e > 0.0 { ((margin - FLIP_MIN_MARGIN) / e).powf(1.0 / ERODE_TIME).min(48.0) } else { 48.0 };
            let net_eff = net - 0.4 * e * buy;   // what chasing the book costs on average over an hour
            if net_eff <= 0.0 { continue; }
            let units_h = u.min(capital / (buy * FLIP_TURNOVER_H));
            let pph = net_eff * units_h;
            let thin = if route == "NPC" { rb } else { rb.min(rs) };
            let liq = (thin / FLIP_FULL_LIQUIDITY).sqrt().min(1.0);
            let pos = capital / FLIP_SLOTS / buy;   // units one slot holds
            let exit_h = pos / (BUY_CAPTURE * sell_flow).max(1e-9);
            let exit = if exit_h > FLIP_EXIT_H { FLIP_EXIT_H / exit_h } else { 1.0 };
            let mfac = (life / LIFE_TARGET_H).min(1.0).sqrt();
            let no_sack = route == "NPC" && sacks_known && !has_sack;
            let adj = pph * liq * exit * mfac * if no_sack { NO_SACK_NPC } else { 1.0 };
            let score = if adj <= SCORE_LO { 1.0 } else { (1.0 + 9.0 * (adj / SCORE_LO).log10() / (SCORE_HI / SCORE_LO).log10()).min(10.0) };
            let est = FlipEst { no_sack, pph, score, route, buy, sell, net, margin, units_h, liq, exit, mfac, capital, life, erosion: e };
            if best.as_ref().map_or(true, |b| est.score > b.score || (est.score == b.score && est.pph > b.pph)) { best = Some(est); }
        }
        best
    }

    /// (are the sacks known, does one of them hold this item). Unknown until a sack scan has run.
    fn item_has_sack(&self, id: &str) -> (bool, bool) {
        if self.sacks.is_empty() { return (false, false); }
        let st = &self.screener;
        let mut c = st.sack_names.borrow_mut();
        if c.0.map_or(true, |t| t.elapsed() > Duration::from_secs(15)) { c.1 = self.sack_rows().into_iter().map(|r| r.name.to_lowercase()).collect(); c.0 = Some(Instant::now()); }
        (true, c.1.contains(&pretty_s(id).to_lowercase()))
    }

    /// Fill the Auto Flip form for `id` with what the score suggests, and open it (nothing starts until you press Start there).
    pub fn flip_prefill(&mut self, id: &str) {
        let est = self.flip_est(id);
        let bid = self.products.get(id).map_or(0.0, |p| p.bid());
        let (sacks_known, has_sack) = self.item_has_sack(id);
        let nice = |x: f64| -> u64 { let x = x.max(64.0); let step = if x >= 20_000.0 { 1000.0 } else if x >= 2_000.0 { 500.0 } else if x >= 500.0 { 100.0 } else { 10.0 }; ((x / step).round() * step).max(64.0) as u64 };
        let mut f = crate::autoflip::FlipForm::new();
        f.product = id.to_string(); f.buy_mode = 1;
        if bid > 0.0 {
            f.min = crate::autoflip::fmt_step(((bid * 0.8) * 10.0).round() / 10.0).to_string();
            f.max = crate::autoflip::fmt_step(((bid * 1.1) * 10.0).round() / 10.0);
        }
        let mut note = String::from("No profitable flip found for this item right now (margin under 10% or no trading): the defaults are filled in.");
        if let Some(e) = &est {
            // a cycle is about 20 minutes of flow, at most half of your coins; the cycles together cover about 4 hours
            let by_flow = e.units_h / 3.0;
            let by_coins = e.capital * 0.5 / e.buy;
            let total = nice(by_flow.min(by_coins).min(2_000_000.0));
            let slice = nice(total as f64 / 4.0).min(71_680).min(total);
            f.total = total.to_string(); f.slice = slice.to_string();
            f.requote = "60".into(); f.on_outbid = true; f.gap = "30".into();
            f.margin = "10".into();
            f.exchange = if e.route == "NPC" { 1 } else { 0 };
            f.sell_all = e.route == "NPC" && sacks_known && has_sack;
            f.requote_max_qty = if e.route == "NPC" && !(sacks_known && has_sack) { "2000".into() } else { "10000".into() };
            f.cycles = ((4.0 * e.units_h / total as f64).round().max(1.0).min(50.0) as u32).to_string();
            note = format!("Suggested from the score ({:.1} ★): {} route, buy near {:.1}, sell near {:.1}, about {} items per cycle, {} cycle(s). {}Review it, then press Start.",
                e.score, e.route, e.buy, e.sell, grouped(total), f.cycles,
                if e.route == "NPC" && sacks_known && !has_sack { "None of your sacks holds it, so goods are claimed and sold from the inventory in rounds (about 2,000 at a time). " } else { "" });
        }
        let _ = note;
        f.msg = None;
        self.flip_form = f;
        self.flip_filled_for = id.to_string();
        self.flip_tab = 0;
        self.open_screen(Screen::AutoFlip);
    }

    pub fn screener_screen(&mut self, ctx: &egui::Context) {
        self.screen_frame(ctx, "Screener  SCREENER", |g, ui| g.screener_body(ui));
    }

    /// The number shown in column `col` (see COL_NAMES) for product `k`; None where the table shows "—". The recipe-based ones are cached until prices change.
    #[cfg(test)]
    pub fn flip_est_for_tests(&self, k: &str) -> Option<FlipEst> { self.flip_est(k) }
    #[cfg(test)]
    pub fn screener_value_for_tests(&self, k: &str, col: u8) -> Option<f64> { self.col_value(k, col) }

    fn col_value(&self, k: &str, col: u8) -> Option<f64> {
        let p = self.products.get(k)?;
        let cheap = match col {
            1 => Some(p.bid()), 2 => Some(p.ask()),
            3 => return if p.ask() > 0.0 && p.bid() > 0.0 { Some(p.ask() - p.bid()) } else { None },
            4 => Some(p.quick_status.buy_moving_week as f64), 5 => Some(p.quick_status.sell_moving_week as f64),
            6 => return npc_sell_of(k).filter(|v| *v > 0.0),
            14 => return self.flip_est(k).map(|e| e.pph),
            15 => return self.flip_est(k).map(|e| e.score),
            16 => return self.flip_est(k).map(|e| e.life),
            9 => return { let n = npc_sell_of(k).filter(|v| *v > 0.0)?; let b = p.bid(); if b > 0.0 { Some(n - (b + 0.1)) } else { None } },
            _ => None,
        };
        if cheap.is_some() { return cheap; }
        if !(7..=13).contains(&col) { return None; }
        let st = &self.screener;
        if st.cache_at.get().map_or(true, |t| t.elapsed() > Duration::from_secs(15)) { st.cache.borrow_mut().clear(); st.cache_at.set(Some(Instant::now())); }   // prices move slowly: recompute these every 15 s, not on every refresh
        if let Some(v) = st.cache.borrow().get(&(col, k.to_string())) { return *v; }
        let v = match col {
            7 => self.craft_cost(k, true),
            8 => { let n = npc_sell_of(k).filter(|v| *v > 0.0); n.zip(self.craft_cost(k, true)).map(|(n, c)| n - c) }
            10 => { let n = npc_sell_of(k).filter(|v| *v > 0.0); n.zip(self.craft_cost(k, true)).map(|(n, c)| (n - c) / n * NPC_SELL_CAP) }
            12 => { let n = npc_sell_of(k).filter(|v| *v > 0.0)?; let b = p.bid(); if b > 0.0 { Some((n - (b + 0.1)) / n * NPC_SELL_CAP) } else { None } }
            13 => self.bnsmp_eta(k),
            _ => self.cnsmp_eta(k).map(|e| e.0),
        };
        st.cache.borrow_mut().insert((col, k.to_string()), v);
        v
    }

    fn screener_items(&self) -> Vec<String> {
        let st = &self.screener;
        let q = st.filter.trim().to_uppercase().replace(' ', "_");
        let t = tree();
        let known: std::collections::HashSet<&String> = t.iter().flat_map(|(_, f)| f.iter().flat_map(|(_, ids)| ids.iter())).collect();
        let mut out: Vec<String> = if !q.is_empty() {
            self.products.keys().filter(|k| k.contains(&q) || pretty_s(k).to_uppercase().replace(' ', "_").contains(&q)).cloned().collect()
        } else if st.cat == "All" {
            self.products.keys().cloned().collect()
        } else {
            let mut v: Vec<String> = t.iter().find(|(c, _)| *c == st.cat).and_then(|(_, f)| f.iter().find(|(n, _)| *n == st.fam)).map(|(_, ids)| ids.clone()).unwrap_or_default();
            if st.cat == "Oddities" && st.fam == "Other" { v.extend(self.products.keys().filter(|k| !known.contains(*k)).cloned()); }
            v
        };
        out.retain(|k| self.products.contains_key(k));
        // sort column: 0 name, 1 bid, 2 ask, 3 spread, 4 buy vol, 5 sell vol, 6 NPC sell, 7 craft cost (buy orders), 8 CNSΔ, 9 BNSΔ, 10 CNSMP, 11 CNSMP-ETA; items without a value always go last
        let col = st.sort;
        // filters first (every active one must pass), then the sort
        for (c, f) in st.filters.iter().filter(|(c, f)| **c > 0 && f.active()) {
            let (lo, hi) = f.bounds();
            out.retain(|k| match self.col_value(k, *c) {
                Some(v) => lo.map_or(true, |l| v >= l) && hi.map_or(true, |h| v <= h),
                None => !f.hide_empty && lo.is_none() && hi.is_none(),
            });
        }
        let val = |k: &String| self.col_value(k, col);
        if col == 0 {
            let mut named: Vec<(String, String)> = out.into_iter().map(|k| (pretty_s(&k), k)).collect();   // each name built once, not once per comparison
            named.sort_by(|a, b| a.0.cmp(&b.0));
            out = named.into_iter().map(|x| x.1).collect();
            if st.desc { out.reverse(); }
        }
        else {
            let mut keyed: Vec<(Option<f64>, String)> = out.into_iter().map(|k| (val(&k), k)).collect();
            let desc = st.desc;
            keyed.sort_by(|a, b| match (a.0, b.0) {
                (Some(x), Some(y)) => { let o = x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal); if desc { o.reverse() } else { o } }
                (Some(_), None) => std::cmp::Ordering::Less, (None, Some(_)) => std::cmp::Ordering::Greater, (None, None) => std::cmp::Ordering::Equal,
            });
            out = keyed.into_iter().map(|x| x.1).collect();
        }
        out
    }

    fn screener_body(&mut self, ui: &mut egui::Ui) {
        if self.screener.cat.is_empty() { if let Ok(c) = std::env::var("BLOCKBERG_SCREENER_SORT") { self.screener.sort = c.parse().unwrap_or(0); self.screener.desc = true; } if let Ok(m) = std::env::var("BLOCKBERG_SCREENER_HIDDEN") { self.screener.hidden = m.parse().unwrap_or(0); } if let Ok(fs) = std::env::var("BLOCKBERG_SCREENER_FILTERS") { for part in fs.split(';') { let mut it = part.split(','); if let Some(c) = it.next().and_then(|c| c.parse::<u8>().ok()) { let f = self.screener.filters.entry(c).or_default(); f.min = it.next().unwrap_or("").into(); f.max = it.next().unwrap_or("").into(); } } } if let Ok(m) = std::env::var("BLOCKBERG_SCREENER_COLMENU") { self.screener.col_menu = m.parse().ok().map(|c| (c, egui::pos2(900.0, 300.0))); } if let Ok(m) = std::env::var("BLOCKBERG_SCREENER_ROWMENU") { self.screener.row_menu = Some((m, egui::pos2(500.0, 300.0))); } if let Ok(f) = std::env::var("BLOCKBERG_SCREENER_FIND") { self.screener.filter = f; } }
        if self.screener.cat.is_empty() && std::env::var("BLOCKBERG_SCREENER_ALL").is_ok() { self.screener.cat = "All".into(); }
        if self.screener.cat.is_empty() { if let Some((c, f)) = tree().first() { self.screener.cat = c.clone(); self.screener.fam = f.first().map(|x| x.0.clone()).unwrap_or_default(); } }
        let h = (ui.available_height() - 4.0).max(300.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Find").color(DIM).size(16.0));
            let r = ui.add(egui::TextEdit::singleline(&mut self.screener.filter).desired_width(240.0).hint_text("search every item"));
            if !self.screener.filter.is_empty() && ui.fx_small_button("Clear").clicked() { self.screener.filter.clear(); }
            let _ = r;
            ui.add_space(14.0);
            ui.label(RichText::new("Columns").color(DIM).size(16.0));
            let mut changed = false;
            for (i, n) in COL_NAMES.iter().enumerate().skip(1) {
                let on = self.screener.hidden & (1 << i) == 0;
                if ui.selectable_label(on, *n).fx_tip(if on { "Click to hide this column" } else { "Click to show this column" }).clicked() { self.screener.hidden ^= 1 << i; changed = true; }
            }
            if changed { self.save_layout(); }
        });
        ui.add_space(4.0);
        let searching = !self.screener.filter.trim().is_empty();
        ui.horizontal_top(|ui| {
            // 1. categories
            ui.vertical(|ui| {
                ui.set_width(170.0);
                ui.label(RichText::new("Category").color(ORANGE).size(18.0));
                egui::ScrollArea::vertical().id_salt("scr_cat").max_height(h - 50.0).auto_shrink([false, false]).show(ui, |ui| {
                    if ui.selectable_label(!searching && self.screener.cat == "All", RichText::new(format!("All  ({})", self.products.len())).size(18.0)).clicked() {
                        self.screener.cat = "All".into(); self.screener.fam.clear(); self.screener.filter.clear();
                    }
                    for (c, fams) in tree() {
                        let n: usize = fams.iter().map(|f| f.1.len()).sum();
                        let sel = !searching && self.screener.cat == *c;
                        if ui.selectable_label(sel, RichText::new(format!("{c}  ({n})")).size(18.0)).clicked() {
                            self.screener.cat = c.clone(); self.screener.fam = fams.first().map(|f| f.0.clone()).unwrap_or_default(); self.screener.filter.clear();
                        }
                    }
                });
            });
            ui.separator();
            // 2. families of the chosen category (not shown for All or a search: nothing to pick there, so the table gets the room)
            let show_fams = !searching && self.screener.cat != "All";
            if show_fams { ui.vertical(|ui| {
                ui.set_width(250.0);
                ui.label(RichText::new(if searching { "Search results" } else { self.screener.cat.as_str() }).color(ORANGE).size(18.0));
                egui::ScrollArea::vertical().id_salt("scr_fam").max_height(h - 50.0).auto_shrink([false, false]).show(ui, |ui| {
                    if searching { return; }
                    if let Some((_, fams)) = tree().iter().find(|(c, _)| *c == self.screener.cat) {
                        for (f, ids) in fams {
                            let n = ids.iter().filter(|i| self.products.contains_key(*i)).count();
                            if n == 0 && f != "Other" { continue; }
                            let sel = self.screener.fam == *f;
                            if ui.selectable_label(sel, RichText::new(format!("{}  ({n})", nice_family(&self.screener.cat, f))).size(17.0)).clicked() { self.screener.fam = f.clone(); }
                        }
                    }
                });
            }); ui.separator(); }
            // 3. items (virtualized: only the visible rows are built, so "All" stays fast)
            ui.vertical(|ui| {
                let items = self.screener_items();
                ui.label(RichText::new(format!("{} item(s)", items.len())).color(ORANGE).size(18.0));
                let mut flip_item: Option<String> = None;
                let (mut open, mut watch_add, mut pick, mut clicked): (Option<String>, Option<String>, Option<String>, Option<u8>) = (None, None, None, None);
                let (mut col_req, mut row_req, mut clear_filters): (Option<(u8, egui::Pos2)>, Option<(String, egui::Pos2)>, bool) = (None, None, false);
                let fil_active: Vec<bool> = (0..17u8).map(|i| self.screener.filters.get(&i).map_or(false, |f| f.active())).collect();
                let heads: [(&str, &str); 17] = [("Item", "Sort by name"), ("Bid", "Top buy order: what you get when you insta-sell"), ("Ask", "Lowest sell offer: what you pay when you insta-buy"), ("Spread", "Ask minus bid, in coins"),
                    ("Buy vol 7d", "Units insta-bought in the last 7 days"), ("Sell vol 7d", "Units insta-sold in the last 7 days"), ("NPC sell", "What an NPC pays you for one"),
                    ("Craft cost", "Cost to craft one from its recipe with every ingredient bought by buy order (bid + 0.1)"), ("CNSΔ", "Craft → NPC sell spread: NPC sell price minus the craft cost (ingredients bought by buy order)"), ("BNSΔ", "Bid → NPC sell spread: NPC sell price minus a buy order at bid + 0.1"),
                    ("CNSMP", "Craft to NPC sell max profit: the profit if you crafted and sold to the NPC until you hit the 500M daily NPC limit (spread per unit × units that fit under the limit)"),
                    ("CNSMP-ETA", "Estimated hours to buy the RAW MATERIALS for CNSMP with buy orders: each material needs (units for the 500M limit × amount per craft), filled at its 7-day insta-sell volume ÷ 168 per hour (you get about 60% of that); the slowest one sets the time."),
                    ("BNSMP", "Bid to NPC sell max profit: the profit if you bought the item itself with buy orders (bid + 0.1, no crafting) and sold it to the NPC until you hit the 500M daily limit"),
                    ("BNSMP-ETA", "Estimated hours to buy that many of the ITEM ITSELF with buy orders: units for the 500M limit ÷ (its 7-day insta-sell volume ÷ 168 per hour × about 60%). This is the time for the BNSMP profit."),
                    ("PPH", "Estimated profit per hour: profit per unit (buy at bid + 0.1, sell at ask − 0.1 with 1.25% tax, or to the NPC) × units per hour. Units per hour = about 60% of the thinner side's flow, limited to what your coins (purse + bank) can turn over in an hour"),
                    ("Score ★", "1 to 10 stars. PPH with penalties for thin trading, for a position you could not sell within 4 hours (one of 5 slots), and for a margin that would not last (it must survive being undercut for about 3 h; nothing under 10% counts). 100k per hour is 1 star, 30M per hour is 10"),
                    ("Life", "How many hours the margin lasts while you keep your orders at the top of the book and get undercut (until less than 10% is left). Estimated from the spread and the price: about 80% of the time it lasts at least this long")];
                let widths = [230.0, 80.0, 80.0, 80.0, 105.0, 105.0, 80.0, 90.0, 85.0, 85.0, 140.0, 120.0, 130.0, 125.0, 105.0, 95.0, 75.0];
                // the score columns sit right after the item name
                let order: [usize; 17] = [0, 15, 14, 16, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13];
                let (sort, desc) = (self.screener.sort, self.screener.desc);
                let mut tb = TableBuilder::new(ui).striped(true).id_salt("scr_table").max_scroll_height(h - 60.0).auto_shrink([false, false]).cell_layout(egui::Layout::left_to_right(egui::Align::Center));
                let hidden = self.screener.hidden;
                let vis = move |i: usize| i == 0 || hidden & (1 << i) == 0;
                for &i in &order { if vis(i) { tb = tb.column(Column::exact(widths[i])); } }
                tb = tb.column(Column::exact(215.0));
                tb.header(28.0, |mut hd| {
                    for &i in &order {
                        let (t, tip) = heads[i];
                        if !vis(i) { continue; }
                        hd.col(|ui| {
                            let active = sort == i as u8;
                            let arrow = if active { if desc { " ▼" } else { " ▲" } } else { "" };
                            let filtered = fil_active[i];
                            let r = ui.add(egui::Label::new(RichText::new(format!("{t}{arrow}{}", if filtered { " ⏷" } else { "" })).color(if active || filtered { ORANGE } else { Color32::WHITE }).strong()).sense(egui::Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand);
                            if r.clicked() { clicked = Some(i as u8); }
                            if r.secondary_clicked() && i > 0 { col_req = Some((i as u8, r.rect.left_bottom())); }
                            fx::tip(r, format!("{tip}. Click to sort; click again to reverse. Right-click to filter or sort this column."));
                        });
                    }
                    hd.col(|ui| { if ui.add_enabled(fil_active.iter().any(|x| *x), egui::Button::new("Clear filters").small()).fx_tip("Remove the filters on every column").clicked() { clear_filters = true; } });
                }).body(|body| {
                    body.rows(30.0, items.len(), |mut row| {
                        let id = &items[row.index()];
                        let Some(p) = self.products.get(id) else { return };
                        macro_rules! cell { ($i:expr, $f:expr) => { if vis($i) { row.col($f); } }; }
                        let (ask, bid) = (p.ask(), p.bid());
                        let sel = self.selected.as_deref() == Some(id.as_str());
                        cell!(0, |ui| {
                            icon_cell(ui, &self.icons, id, "");
                            let r = ui.selectable_label(sel, RichText::new(pretty_s(id)).size(17.0));
                            if r.clicked() { pick = Some(id.clone()); }
                            if r.secondary_clicked() { pick = Some(id.clone()); row_req = Some((id.clone(), r.rect.left_bottom())); }
                            if r.double_clicked() { open = Some(id.clone()); }
                        });
                        let est = self.flip_est(id);
                        cell!(15, |ui| match &est {
                            Some(e) => {
                                let col = if e.score >= 8.0 { GREEN } else if e.score >= 5.0 { Color32::WHITE } else { DIM };
                                ui.label(RichText::new(format!("{:.1}", e.score)).color(col).strong()).fx_tip(format!(
                                    "{} route: buy @ {:.1}, sell @ {:.1}, net {:.1} per unit ({:.1}% margin). About {} units/h on {} of capital. Liquidity ×{:.2}, exit within 4 h ×{:.2}, margin cushion ×{:.2} (the margin lasts about {:.1} h of being undercut; about {:.1}% is lost per hour){}",
                                    e.route, e.buy, e.sell, e.net, e.margin * 100.0, grouped(e.units_h as u64), fmt_coins(e.capital), e.liq, e.exit, e.mfac, e.life, e.erosion * 100.0, if e.no_sack { ". No sack holds it: NPC sales go through the inventory, score ×0.85" } else { "" }));
                            }
                            None => { ui.label(RichText::new("—").color(DIM)); }
                        });
                        cell!(14, |ui| match &est {
                            Some(e) => { ui.label(fmt_coins(e.pph)); }
                            None => { ui.label(RichText::new("—").color(DIM)); }
                        });
                        cell!(16, |ui| match &est {
                            Some(e) => { ui.label(RichText::new(if e.life >= 48.0 { "48 h+".to_string() } else { fmt_hours(e.life) }).color(if e.life >= 3.0 { GREEN } else if e.life >= 1.0 { Color32::WHITE } else { DIM })); }
                            None => { ui.label(RichText::new("—").color(DIM)); }
                        });
                        cell!(1, |ui| { ui.label(fmt_coins(bid)); });
                        cell!(2, |ui| { ui.label(fmt_coins(ask)); });
                        cell!(3, |ui| { ui.label(if ask > 0.0 && bid > 0.0 { fmt_coins(ask - bid) } else { "—".into() }); });
                        cell!(4, |ui| { ui.label(grouped(p.quick_status.buy_moving_week)); });
                        cell!(5, |ui| { ui.label(grouped(p.quick_status.sell_moving_week)); });
                        let npc = npc_sell_of(id).filter(|v| *v > 0.0);
                        cell!(6, |ui| { ui.label(match npc { Some(v) => fmt_coins(v), None => "—".into() }); });
                        let cc = self.craft_cost(id, true);
                        cell!(7, |ui| match cc {
                            Some(c) => {
                                let net = bid * 0.9875;   // what insta-selling one pays after the 1.25% bazaar tax
                                let good = bid > 0.0 && c < net;
                                let tip = if bid > 0.0 { format!("Insta-selling it pays {} after 1.25% tax: {} {} per unit{}", fmt_coins(net), if good { "profit" } else { "loss" }, fmt_coins((net - c).abs()), if ask > 0.0 { format!(". Insta-buying it costs {}", fmt_coins(ask)) } else { String::new() }) } else { String::new() };
                                ui.label(RichText::new(fmt_coins(c)).color(if good { GREEN } else { Color32::WHITE })).fx_tip(tip);
                            }
                            None => { ui.label(RichText::new("—").color(DIM)); }
                        });
                        let spread_cell = |ui: &mut egui::Ui, v: Option<f64>, tip: &str| match v {
                            Some(x) => { let t = format!("{}{}", if x < 0.0 { "−" } else { "" }, fmt_coins(x.abs())); ui.label(RichText::new(t).color(if x > 0.0 { GREEN } else { RED })).fx_tip(tip); }
                            None => { ui.label(RichText::new("—").color(DIM)); }
                        };
                        cell!(8, |ui| spread_cell(ui, npc.zip(cc).map(|(n, c)| n - c), "NPC sell price minus the craft cost"));
                        cell!(9, |ui| spread_cell(ui, npc.filter(|_| bid > 0.0).map(|n| n - (bid + 0.1)), "NPC sell price minus a buy order at bid + 0.1"));
                        cell!(10, |ui| {
                            let mp = npc.zip(cc).map(|(n, c)| (n - c) / n * NPC_SELL_CAP);
                            spread_cell(ui, mp, &format!("Profit if you crafted and NPC-sold {} worth (the whole daily limit): about {} units at {} each", fmt_coins(NPC_SELL_CAP), npc.map_or("—".into(), |n| grouped((NPC_SELL_CAP / n) as u64)), fmt_coins(npc.zip(cc).map_or(0.0, |(n, c)| n - c))));
                        });
                        cell!(11, |ui| match self.cnsmp_eta(id) {
                            Some((h, slow, need)) => { ui.label(fmt_hours(h)).fx_tip(format!("Raw materials for crafting. Slowest one: {} (need {} units)", pretty_s(&slow), grouped(need as u64))); }
                            None => { ui.label(RichText::new("—").color(DIM)); }
                        });
                        cell!(12, |ui| {
                            let mp = npc.filter(|_| bid > 0.0).map(|n| (n - (bid + 0.1)) / n * NPC_SELL_CAP);
                            spread_cell(ui, mp, "Profit if you bought the item itself with buy orders (bid + 0.1) and sold it to the NPC until you hit the 500M daily limit");
                        });
                        cell!(13, |ui| match self.bnsmp_eta(id) {
                            Some(h) => { ui.label(fmt_hours(h)).fx_tip(format!("Buying about {} of the item itself with buy orders, getting about {:.0}% of its insta-sell flow", npc.map_or("—".into(), |n| grouped((NPC_SELL_CAP / n) as u64)), BUY_CAPTURE * 100.0)); }
                            None => { ui.label(RichText::new("—").color(DIM)); }
                        });
                        row.col(|ui| {
                            if ui.fx_small_button("Open").fx_tip("Show it on the Markets screen (adds it to the watchlist)").clicked() { open = Some(id.clone()); }
                            if !self.watch.iter().any(|w| w == id) && ui.fx_small_button("+ Watch").clicked() { watch_add = Some(id.clone()); }
                            if ui.fx_small_button("Flip").fx_tip("Fill the Auto Flip form with the suggested size, prices and sell route for this item, and open it (nothing starts until you press Start)").clicked() { flip_item = Some(id.clone()); }
                        });
                    });
                });
                if clear_filters { self.screener.filters.clear(); }
                if col_req.is_some() { self.screener.col_menu = col_req; self.screener.row_menu = None; }
                if row_req.is_some() { self.screener.row_menu = row_req; self.screener.col_menu = None; }
                if let Some(c) = clicked { if self.screener.sort == c { self.screener.desc = !self.screener.desc; } else { self.screener.sort = c; self.screener.desc = c != 0; } }
                if let Some(id) = pick { self.selected = Some(id); }
                if let Some(id) = watch_add { self.watch.push(id); self.save_layout(); }
                if let Some(id) = flip_item { self.flip_prefill(&id); }
                if let Some(id) = open { self.pick_product(&id); self.screen = Screen::Main; }
            });
        });
        let ctx = ui.ctx().clone();
        self.screener_menus(&ctx);
    }

    /// The right-click menus: a column header (sort + filter that column) and an item row (the watchlist's menu).
    fn screener_menus(&mut self, ctx: &egui::Context) {
        let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let click = ctx.input(|i| i.pointer.primary_pressed()).then(|| ctx.input(|i| i.pointer.interact_pos())).flatten();
        if let Some((c, pos)) = self.screener.col_menu {
            let mut close = esc;
            let area = egui::Area::new(egui::Id::new("scr_col_menu")).order(egui::Order::Foreground).fixed_pos(pos).show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(270.0);
                    ui.label(RichText::new(COL_NAMES[c as usize]).color(ORANGE).strong().size(18.0));
                    ui.add_space(2.0);
                    ui.label(RichText::new("Sort").color(DIM).size(15.0));
                    ui.horizontal(|ui| {
                        let (cur, desc) = (self.screener.sort == c, self.screener.desc);
                        if ui.selectable_label(cur && desc, "High → low").clicked() { self.screener.sort = c; self.screener.desc = true; }
                        if ui.selectable_label(cur && !desc, "Low → high").clicked() { self.screener.sort = c; self.screener.desc = false; }
                    });
                    ui.add_space(4.0);
                    ui.label(RichText::new("Filter (rows outside are hidden; k, m, b suffixes work)").color(DIM).size(15.0));
                    let f = self.screener.filters.entry(c).or_default();
                    ui.horizontal(|ui| {
                        ui.label("Min"); ui.add(egui::TextEdit::singleline(&mut f.min).desired_width(80.0).hint_text("none"));
                        ui.label("Max"); ui.add(egui::TextEdit::singleline(&mut f.max).desired_width(80.0).hint_text("none"));
                    });
                    for (t, label) in [(&f.min, "Min"), (&f.max, "Max")] { if !t.trim().is_empty() && parse_num(t).is_none() { ui.label(RichText::new(format!("{label} is not a number")).color(RED).size(14.0)); } }
                    ui.checkbox(&mut f.hide_empty, "Hide rows with no value (—)");
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui.fx_small_button("Clear this filter").clicked() { *f = ColFilter::default(); }
                        if ui.fx_small_button("Close").clicked() { close = true; }
                    });
                });
            });
            if let Some(p) = click { if !area.response.rect.expand(2.0).contains(p) { close = true; } }
            if close { self.screener.col_menu = None; }
        }
        if let Some((id, pos)) = self.screener.row_menu.clone() {
            let mut close = esc;
            let mut act = None;
            let area = egui::Area::new(egui::Id::new("scr_row_menu")).order(egui::Order::Foreground).fixed_pos(pos).show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| { act = self.watch_menu(ui, &id); });
            });
            let sub: Option<egui::Rect> = ctx.data_mut(|m| m.get_temp(egui::Id::new(("watch_sub", id.as_str()))));
            if let Some(p) = click { if !area.response.rect.expand(2.0).contains(p) && !sub.map_or(false, |r| r.expand(4.0).contains(p)) { close = true; } }
            if let Some(a) = act { self.apply_menu_act(ctx, &id, a); close = true; }
            if close { self.screener.row_menu = None; }
        }
    }
}

// ---------------------------------------------------------------- NPC sell limit calculator
impl Gui {
    /// Right-click the NPC SOLD number: pick an item and see how many of it still fit under today's 500M NPC limit.
    pub fn npc_calc_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut c) = self.npc_calc.clone() else { return };
        let mut open = true;
        egui::Window::new("NPC sell limit").id(egui::Id::new("npc_calc_win")).open(&mut open).collapsible(false).resizable(false).anchor(egui::Align2::LEFT_TOP, [640.0, 138.0]).show(ctx, |ui| {
            ui.set_min_width(420.0);
            let sold = self.npc_sold_today();
            let left = (NPC_SELL_CAP - sold).max(0.0);
            let secs_left = 86400.0 - (now_secs() % 86400.0);
            ui.label(RichText::new(format!("Sold today {} of {} · left {} · resets in {}", fmt_coins(sold), fmt_coins(NPC_SELL_CAP), fmt_coins(left), fmt_eta(secs_left))).color(DIM).size(15.0));
            ui.add_space(4.0);
            let key = c.item.trim().to_uppercase().replace(' ', "_");
            let known = self.products.contains_key(&key) || npc_sell_of(&key).is_some();
            ui.horizontal(|ui| {
                ui.label(RichText::new("Item").size(18.0));
                let r = ui.add(egui::TextEdit::singleline(&mut c.item).text_color(ORANGE).font(FontId::proportional(20.0)).desired_width(310.0).hint_text("search any item"));
                if r.changed() { c.search_open = true; }
            });
            if !known && !c.item.trim().is_empty() && c.search_open {
                let hits: Vec<String> = self.search_items(&c.item).into_iter().filter(|i| npc_sell_of(i).is_some()).take(7).collect();
                if hits.is_empty() { ui.label(RichText::new("no item matches").color(DIM).size(15.0)); }
                for id in hits { ui.horizontal(|ui| { icon_cell(ui, &self.icons, &id, ""); if ui.selectable_label(false, RichText::new(pretty_s(&id)).size(17.0)).clicked() { c.item = id.clone(); c.search_open = false; } }); }
            }
            if known {
                match npc_sell_of(&key).filter(|v| *v > 0.0) {
                    None => { ui.label(RichText::new("No NPC buys this item.").color(RED).size(17.0)); }
                    Some(npc) => {
                        let max = (left / npc).floor() as u64;
                        ui.add_space(4.0);
                        ui.label(RichText::new(format!("{} — the NPC pays {} each", pretty_s(&key), fmt_coins(npc))).size(17.0));
                        ui.label(RichText::new(format!("You can still sell {} ({})", grouped(max), fmt_coins(max as f64 * npc))).color(GREEN).size(22.0).strong());
                        ui.horizontal(|ui| {
                            ui.label("I have");
                            ui.add(egui::TextEdit::singleline(&mut c.have).text_color(ORANGE).desired_width(110.0).hint_text("optional, e.g. 50k"));
                            if let Some(h) = parse_qty(&c.have) {
                                let value = h as f64 * npc;
                                if h <= max { ui.label(RichText::new(format!("all fit ({} of what is left)", format!("{:.0}%", value / left.max(1.0) * 100.0))).color(GREEN)); }
                                else { ui.label(RichText::new(format!("{} do not fit: sell {} today, {} tomorrow", grouped(h - max), grouped(max), grouped(h - max))).color(ORANGE)); }
                            }
                        });
                    }
                }
            }
        });
        self.npc_calc = if open { Some(c) } else { None };
    }
}
