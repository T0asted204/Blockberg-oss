//! The watchlist's right-click menu, the Item Info window and price alerts.
use super::*;
use std::sync::OnceLock;

// ---------------------------------------------------------------- item facts

/// id -> (rarity tier, category, unstackable); from the Hypixel items list (tools/npc_prices.py refreshes it).
fn item_meta(id: &str) -> Option<(&'static str, &'static str, bool)> {
    static M: OnceLock<HashMap<String, (String, String, bool)>> = OnceLock::new();
    let m = M.get_or_init(|| {
        let v: Value = serde_json::from_str(include_str!("../assets/item_meta.json")).unwrap_or(Value::Null);
        v.as_object().map(|o| o.iter().map(|(k, x)| (k.clone(), (x[0].as_str().unwrap_or("").to_string(), x[1].as_str().unwrap_or("").to_string(), x[2].as_u64() == Some(1)))).collect()).unwrap_or_default()
    });
    m.get(id).map(|(t, c, u)| (t.as_str(), c.as_str(), *u))
}

fn rarity(tier: &str) -> (&'static str, Color32) {
    match tier {
        "UNCOMMON" => ("Uncommon", Color32::from_rgb(85, 255, 85)), "RARE" => ("Rare", Color32::from_rgb(85, 85, 255)), "EPIC" => ("Epic", Color32::from_rgb(170, 0, 170)),
        "LEGENDARY" => ("Legendary", Color32::from_rgb(255, 170, 0)), "MYTHIC" => ("Mythic", Color32::from_rgb(255, 85, 255)), "DIVINE" => ("Divine", Color32::from_rgb(85, 255, 255)),
        _ => ("Common", Color32::WHITE),
    }
}

/// "ENCHANTED_HARD_STONE" -> "EHS", "WHEAT" -> "WHEA"
fn ticker(id: &str) -> String {
    let words: Vec<&str> = id.split('_').collect();
    if words.len() > 1 { words.iter().filter_map(|w| w.chars().next()).collect::<String>().chars().take(5).collect() } else { id.chars().take(5).collect() }
}

fn title_case(s: &str) -> String {
    s.split('_').map(|w| { let mut c = w.chars(); c.next().map(|f| f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase()).unwrap_or_default() }).collect::<Vec<_>>().join(" ")
}

/// unix seconds -> "HH:MM" or "MM-DD" (UTC, like the rest of the app's clocks)
fn fmt_ts(t: f64, date: bool) -> String {
    let secs = t as i64;
    if !date { let s = secs.rem_euclid(86400); return format!("{:02}:{:02}", s / 3600, s / 60 % 60); }
    let z = secs.div_euclid(86400) + 719468;
    let era = z.div_euclid(146097); let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let _ = (era, yoe);
    format!("{m:02}-{d:02}")
}

/// A "nice" step for about `n` gridlines across `span`.
fn nice_step(span: f64, n: f64) -> f64 {
    let raw = (span / n).max(1e-9);
    let p = 10f64.powf(raw.log10().floor());
    p * [1.0, 2.0, 5.0, 10.0].into_iter().find(|m| raw <= p * m).unwrap_or(10.0)
}

pub const RANGES: [(&str, f64); 6] = [("1D", 1440.0), ("1W", 10080.0), ("1M", 43200.0), ("3M", 129600.0), ("1Y", 525600.0), ("All", f64::MAX)];

// ---------------------------------------------------------------- alerts

#[derive(Clone)]
pub struct Alert { pub id: String, pub above: bool, pub price: f64, pub fired: bool }
pub struct AlertDlg { pub id: String, pub above: bool, pub price: String }

fn alerts_path() -> std::path::PathBuf { candles::data_dir().join("alerts.json") }

pub fn load_alerts() -> Vec<Alert> {
    let Ok(t) = std::fs::read_to_string(alerts_path()) else { return vec![] };
    let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
    v.as_array().map(|a| a.iter().filter_map(|x| Some(Alert { id: x["id"].as_str()?.to_string(), above: x["above"].as_bool()?, price: x["price"].as_f64()?, fired: x["fired"].as_bool().unwrap_or(false) })).collect()).unwrap_or_default()
}

// ---------------------------------------------------------------- the menu

/// One row of the right-click menu: glyph, label, shortcut hint on the right, optional arrow for a submenu.
fn menu_row(ui: &mut egui::Ui, glyph: &str, label: &str, hint: &str, arrow: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width().max(262.0), 30.0), egui::Sense::click());
    let t = fx::tween(ui.ctx(), resp.id.with("mh"), if resp.hovered() { 1.0 } else { 0.0 }, 0.12);
    if t > 0.01 { ui.painter().rect_filled(rect.shrink2(egui::vec2(3.0, 1.0)), 3.0, Color32::from_rgb(30, 22, 4).lerp_to_gamma(ORANGE, t)); }
    let col = Color32::from_gray(225).lerp_to_gamma(Color32::BLACK, t);
    let gc = rect.left_center() + egui::vec2(20.0, 0.0);
    match glyph {
        "info" => { ui.painter().circle_stroke(gc, 8.5, Stroke::new(1.8, col)); ui.painter().text(gc + egui::vec2(0.0, 0.5), egui::Align2::CENTER_CENTER, "i", FontId::new(15.0, egui::FontFamily::Name("terminal-bold".into())), col); }
        "bell" => {
            ui.painter().circle_filled(gc + egui::vec2(0.0, -2.0), 6.5, col);
            ui.painter().rect_filled(egui::Rect::from_min_max(gc + egui::vec2(-6.5, -2.0), gc + egui::vec2(6.5, 4.0)), 0.0, col);
            ui.painter().rect_filled(egui::Rect::from_min_max(gc + egui::vec2(-9.0, 3.0), gc + egui::vec2(9.0, 5.5)), 1.0, col);
            ui.painter().circle_filled(gc + egui::vec2(0.0, 8.0), 2.2, col);
        }
        _ => { ui.painter().text(gc, egui::Align2::CENTER_CENTER, glyph, FontId::proportional(19.0), col); }
    }
    ui.painter().text(rect.left_center() + egui::vec2(42.0, 0.0), egui::Align2::LEFT_CENTER, label, FontId::proportional(20.0), col);
    ui.painter().text(rect.right_center() - egui::vec2(12.0, 0.0), egui::Align2::RIGHT_CENTER, if arrow { "▶" } else { hint }, FontId::proportional(if arrow { 11.0 } else { 17.0 }), col.gamma_multiply(0.75));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[derive(Clone, Copy, PartialEq)]
pub enum MenuAct { Info, Chart, Alert, Copy, Remove, Trade }

impl Gui {
    /// The watchlist row's right-click menu. Returns what was chosen.
    pub fn watch_menu(&mut self, ui: &mut egui::Ui, id: &str) -> Option<MenuAct> {
        let mut act = None;
        ui.set_min_width(262.0);
        ui.spacing_mut().item_spacing.y = 1.0;
        if menu_row(ui, "info", "Item Info…", "Enter", false).clicked() { act = Some(MenuAct::Info); }
        let chart_row = menu_row(ui, "▤", "Add to Chart", "", true);
        // the sub-menu opens to the right while the pointer is on the row or on the sub-menu itself
        let sub_id = egui::Id::new(("watch_sub", id));
        let sub_rect: Option<egui::Rect> = ui.ctx().data_mut(|m| m.get_temp(sub_id));
        let over_sub = sub_rect.map_or(false, |r| ui.ctx().input(|i| i.pointer.hover_pos()).map_or(false, |p| r.expand(4.0).contains(p)));
        if chart_row.hovered() || over_sub || chart_row.clicked() {
            let a = egui::Area::new(sub_id.with("area")).order(egui::Order::Foreground).fixed_pos(chart_row.rect.right_top() + egui::vec2(4.0, -4.0)).show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(240.0);
                    if menu_row(ui, "▤", "Show on the main chart", "", false).clicked() { act = Some(MenuAct::Chart); }
                    if menu_row(ui, "info", "Show in the info window", "", false).clicked() { act = Some(MenuAct::Info); }
                });
            });
            ui.ctx().data_mut(|m| m.insert_temp(sub_id, a.response.rect));
        }
        if menu_row(ui, "bell", "Add Alert…", "A", false).clicked() { act = Some(MenuAct::Alert); }
        ui.add_space(3.0); ui.separator(); ui.add_space(3.0);
        if menu_row(ui, "⎘", "Copy Symbol", "Ctrl+C", false).clicked() { act = Some(MenuAct::Copy); }
        if menu_row(ui, "✕", "Remove from Watchlist", "", false).clicked() { act = Some(MenuAct::Remove); }
        ui.add_space(3.0); ui.separator(); ui.add_space(3.0);
        if menu_row(ui, "⇄", "Trade…", "T", false).clicked() { act = Some(MenuAct::Trade); }
        act
    }

    pub fn apply_menu_act(&mut self, ctx: &egui::Context, id: &str, act: MenuAct) {
        match act {
            MenuAct::Info => self.open_info(ctx, id),
            MenuAct::Chart => { self.selected = Some(id.to_string()); self.screen = Screen::Main; }
            MenuAct::Alert => self.open_alert_dialog(id),
            MenuAct::Copy => { ctx.copy_text(id.to_string()); let _ = self.toast_new(1, format!("Copied {id}")); }
            MenuAct::Remove => { self.watch.retain(|w| w != id); if self.selected.as_deref() == Some(id) { self.selected = self.watch.first().cloned(); } }
            MenuAct::Trade => { self.selected = Some(id.to_string()); self.show_ticket = true; self.screen = Screen::Main; }
        }
    }

    pub fn open_info(&mut self, ctx: &egui::Context, id: &str) {
        let _ = ctx;
        self.info_item = Some(id.to_string());
        self.info_open = true; self.info_min = false;
    }

    pub fn open_alert_dialog(&mut self, id: &str) {
        let now = self.products.get(id).map(|p| p.bid()).unwrap_or(0.0);
        self.alert_dlg = Some(AlertDlg { id: id.to_string(), above: true, price: format!("{now:.1}") });
    }

    // ---------------------------------------------------------------- alerts

    pub fn save_alerts(&self) {
        if self.mock { return; }
        let v: Vec<Value> = self.alerts.iter().map(|a| json!({"id": a.id, "above": a.above, "price": a.price, "fired": a.fired})).collect();
        let p = alerts_path();
        if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
        let tmp = p.with_extension("tmp");
        if std::fs::write(&tmp, Value::Array(v).to_string()).is_ok() { let _ = std::fs::rename(&tmp, &p); }
    }

    /// After every price update: tell the user about alerts whose price was reached.
    pub fn check_alerts(&mut self) {
        let mut fired = vec![];
        for (i, a) in self.alerts.iter().enumerate() {
            if a.fired { continue; }
            let Some(p) = self.products.get(&a.id) else { continue };
            let bid = p.bid();
            if (a.above && bid >= a.price) || (!a.above && bid <= a.price) { fired.push((i, bid)); }
        }
        for (i, bid) in fired {
            let a = self.alerts[i].clone();
            self.alerts[i].fired = true;
            let msg = format!("ALERT: {} bid is {} {} (now {})", pretty_s(&a.id), if a.above { "at or above" } else { "at or below" }, fmt_coins(a.price), fmt_coins(bid));
            self.say(ORANGE, msg.clone());
            let _ = self.toast_new(1, msg.clone());
            if !self.mock { let _ = std::process::Command::new("notify-send").args(["Blockberg", &msg]).spawn(); }
        }
        if self.alerts.iter().any(|a| a.fired) && !self.mock { self.save_alerts(); }
    }

    pub fn alert_window(&mut self, ctx: &egui::Context) {
        let Some(mut dlg) = self.alert_dlg.take() else { return };
        let mut keep = true; let mut add = false;
        egui::Window::new("Add alert").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            ui.label(RichText::new(format!("{}", pretty_s(&dlg.id))).color(ORANGE).size(22.0));
            ui.label(RichText::new("Tells you (a notification and a desktop message) when the bid price reaches this level. Alerts are checked while the app is open.").color(DIM).size(15.0));
            ui.horizontal(|ui| {
                if ui.selectable_label(dlg.above, "Goes above").clicked() { dlg.above = true; }
                if ui.selectable_label(!dlg.above, "Goes below").clicked() { dlg.above = false; }
            });
            let valid = dlg.price.trim().parse::<f64>().ok().filter(|v| *v > 0.0 && v.is_finite());
            ui.horizontal(|ui| {
                ui.label("Price");
                let r = ui.add(egui::TextEdit::singleline(&mut dlg.price).desired_width(110.0));
                fx::focus_ring(ui, &r, if dlg.price.trim().is_empty() { None } else { Some(valid.is_some()) });
                if let Some(p) = self.products.get(&dlg.id) { ui.label(RichText::new(format!("now {}", fmt_coins(p.bid()))).color(DIM)); }
            });
            ui.horizontal(|ui| {
                if ui.fx_button_en(valid.is_some(), "  Add alert  ").clicked() { add = true; }
                if ui.fx_button("  Close  ").clicked() { keep = false; }
            });
            let mine: Vec<usize> = self.alerts.iter().enumerate().filter(|(_, a)| a.id == dlg.id).map(|(i, _)| i).collect();
            if !mine.is_empty() { ui.separator(); ui.label(RichText::new("Your alerts on this item").color(DIM)); }
            let mut del = None;
            for i in mine {
                let a = &self.alerts[i];
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{} {}", if a.above { "above" } else { "below" }, fmt_coins(a.price))).color(if a.fired { DIM } else { Color32::WHITE }));
                    if a.fired { ui.label(RichText::new("(reached)").color(GREEN)); }
                    if ui.fx_small_button("Remove").clicked() { del = Some(i); }
                });
            }
            if let Some(i) = del { self.alerts.remove(i); self.save_alerts(); }
        });
        if add { if let Some(p) = dlg.price.trim().parse::<f64>().ok().filter(|v| *v > 0.0 && v.is_finite()) {
            self.alerts.push(Alert { id: dlg.id.clone(), above: dlg.above, price: p, fired: false });
            self.save_alerts();
            let _ = self.toast_new(1, format!("Alert set: {} bid {} {}", pretty_s(&dlg.id), if dlg.above { "above" } else { "below" }, fmt_coins(p)));
        } }
        if keep { self.alert_dlg = Some(dlg); }
    }

    // ---------------------------------------------------------------- the info window's data

    /// (time, price) points for the chart: the recorder's candles plus this session's snapshots, thinned to about 240.
    fn info_points(&mut self, id: &str, range_min: f64, series: u8) -> Vec<(f64, f64)> {
        let key = (id.to_string(), range_min as i64, series);
        if let Some((k, at, pts)) = &self.info_pts { if *k == key && at.elapsed() < Duration::from_secs(5) { return pts.clone(); } }
        let stale = self.info_cache.get(id).map_or(true, |(t, _)| t.elapsed() > Duration::from_secs(60));
        if stale { self.info_cache.insert(id.to_string(), (Instant::now(), candles::load(id, 600_000))); }
        let h = self.history.get(id).cloned().unwrap_or_default();
        let merged = candles::merge(&self.info_cache[id].1, &candles::from_snapshots(&h));
        let now = now_secs();
        let from = if range_min == f64::MAX { f64::MIN } else { now - range_min * 60.0 };
        let sel: Vec<_> = merged.into_iter().filter(|c| c.t as f64 >= from).collect();
        let span_min = sel.first().map_or(1.0, |c| (now - c.t as f64) / 60.0).max(1.0);
        let tf = (span_min / 240.0).ceil().max(1.0) as i64;
        let pts: Vec<(f64, f64)> = candles::aggregate(&sel, tf * 60).iter().map(|c| (c.t as f64 + tf as f64 * 30.0, c.series(series)[3] as f64)).filter(|p| p.1 > 0.0).collect();
        self.info_pts = Some((key, Instant::now(), pts.clone()));
        pts
    }

    // ---------------------------------------------------------------- the window

    pub fn info_window(&mut self, ctx: &egui::Context) {
        if !self.info_open { return; }
        // un-pinned: the window follows whatever is selected in the watchlist
        if !self.info_pinned { if let Some(s) = self.selected.clone() { self.info_item = Some(s); } }
        let Some(id) = self.info_item.clone() else { return };
        let prod = self.products.get(&id).cloned();
        let pts = if self.info_min { vec![] } else { self.info_points(&id, RANGES[self.info_range.min(5) as usize].1, [1u8, 0, 2][self.info_series.min(2) as usize]) };
        let mut close = false;
        let screen = ctx.screen_rect();
        egui::Window::new("iteminfo_window").title_bar(false).resizable(false).collapsible(false)
            .default_pos(egui::pos2((screen.right() - 700.0).max(40.0), 150.0)).default_width(690.0)
            .frame(egui::Frame::NONE.fill(Color32::from_rgb(4, 4, 6)).stroke(Stroke::new(1.0, Color32::from_gray(110))).corner_radius(3).inner_margin(0)).show(ctx, |ui| {
            ui.set_width(690.0);
            // ---- title bar
            let (bar, _) = ui.allocate_exact_size(egui::vec2(690.0, 42.0), egui::Sense::hover());
            gradient(ui, bar, Color32::from_rgb(255, 194, 16), Color32::from_rgb(255, 155, 0));
            ui.painter().text(bar.left_center() + egui::vec2(16.0, 0.0), egui::Align2::LEFT_CENTER, ticker(&id), FontId::new(30.0, egui::FontFamily::Name("terminal-bold".into())), Color32::BLACK);
            ui.painter().text(bar.left_center() + egui::vec2(110.0, 0.0), egui::Align2::LEFT_CENTER, pretty_s(&id), FontId::proportional(25.0), Color32::BLACK);
            let btn = |ui: &mut egui::Ui, x: f32, key: &str, tip: &str| -> egui::Response {
                let r = egui::Rect::from_center_size(egui::pos2(bar.right() - x, bar.center().y), egui::vec2(34.0, 34.0));
                let resp = ui.interact(r, ui.id().with(("infobtn", key)), egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                if resp.hovered() { ui.painter().rect_filled(r, 4.0, Color32::from_black_alpha(40)); }
                fx::tip(resp, tip)
            };
            let pin = btn(ui, 100.0, "pin", if self.info_pinned { "Pinned: this window stays on this item. Click to follow the watchlist selection again." } else { "Pin: keep this window on this item while you click around the watchlist" });
            { let c = egui::pos2(bar.right() - 100.0, bar.center().y); let k = Stroke::new(2.0, Color32::BLACK);
              if self.info_pinned { ui.painter().circle_filled(c, 8.0, Color32::BLACK); } else { ui.painter().circle_stroke(c, 8.0, k); }
              ui.painter().line_segment([c + egui::vec2(0.0, 8.0), c + egui::vec2(0.0, 16.0)], k); }
            if pin.clicked() { self.info_pinned = !self.info_pinned; }
            let mini = btn(ui, 58.0, "min", "Collapse to the title bar");
            ui.painter().line_segment([egui::pos2(bar.right() - 68.0, bar.center().y + 3.0), egui::pos2(bar.right() - 48.0, bar.center().y + 3.0)], Stroke::new(3.0, Color32::BLACK));
            if mini.clicked() { self.info_min = !self.info_min; }
            let x = btn(ui, 20.0, "close", "Close");
            { let c = egui::pos2(bar.right() - 20.0, bar.center().y); let k = Stroke::new(3.0, Color32::BLACK);
              ui.painter().line_segment([c + egui::vec2(-8.0, -8.0), c + egui::vec2(8.0, 8.0)], k); ui.painter().line_segment([c + egui::vec2(-8.0, 8.0), c + egui::vec2(8.0, -8.0)], k); }
            if x.clicked() { close = true; }
            if self.info_min { return; }
            ui.add_space(10.0);
            let Some(p) = prod else {
                // not loaded yet: shapes of what is coming
                for k in 0..4 { let (r, _) = ui.allocate_exact_size(egui::vec2(660.0, 40.0), egui::Sense::hover()); fx::skeleton(ui, egui::Rect::from_min_size(r.min + egui::vec2(15.0, 0.0), egui::vec2(660.0 * (1.0 - 0.15 * k as f32), 34.0)), 4.0); ui.add_space(8.0); }
                return;
            };
            let (bid, ask) = (p.bid(), p.ask());
            let (bid_qty, ask_qty) = (p.sell_summary.first().map_or(0, |l| l.amount), p.buy_summary.first().map_or(0, |l| l.amount));
            let first = pts.first().map(|x| x.1); let last = pts.last().map(|x| x.1).unwrap_or(bid);
            let (chg, pct) = first.filter(|f| *f > 0.0).map_or((0.0, 0.0), |f| (last - f, (last - f) / f * 100.0));
            let up = chg >= 0.0;
            let mood = if first.is_none() { Color32::WHITE } else if up { GREEN } else { RED };
            let vol = (p.quick_status.buy_moving_week + p.quick_status.sell_moving_week) as f64;
            // ---- header: icon, big price, bid / ask / spread
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                let (ir, _) = ui.allocate_exact_size(egui::vec2(100.0, 100.0), egui::Sense::hover());
                ui.painter().rect_filled(ir, 3.0, Color32::from_rgb(10, 10, 14)); ui.painter().rect_stroke(ir, 3.0, Stroke::new(1.0, Color32::from_gray(80)), egui::StrokeKind::Inside);
                if let Some(ic) = &self.icons { ic.paint(ui.painter(), &id, ir.shrink(14.0)); }
                let (hr, _) = ui.allocate_exact_size(egui::vec2(550.0, 100.0), egui::Sense::hover());
                ui.painter().rect_stroke(hr, 3.0, Stroke::new(1.0, Color32::from_gray(80)), egui::StrokeKind::Inside);
                let c = ui.ctx().clone();
                let price = fx::count_to(&c, egui::Id::new(("info_price", &id)), last);
                ui.painter().text(hr.left_top() + egui::vec2(16.0, 38.0), egui::Align2::LEFT_CENTER, format!("{price:.1}"), FontId::new(46.0, egui::FontFamily::Name("brand".into())), mood);
                ui.painter().text(hr.left_top() + egui::vec2(230.0, 36.0), egui::Align2::LEFT_CENTER, format!("{chg:+.1}"), FontId::new(24.0, egui::FontFamily::Name("terminal-bold".into())), mood);
                ui.painter().text(hr.left_top() + egui::vec2(310.0, 36.0), egui::Align2::LEFT_CENTER, format!("{pct:+.2}%"), FontId::new(24.0, egui::FontFamily::Name("terminal-bold".into())), mood);
                ui.painter().text(hr.right_top() + egui::vec2(-14.0, 36.0), egui::Align2::RIGHT_CENTER, format!("Vol {}", fmt_coins(vol)), FontId::proportional(22.0), Color32::from_gray(210));
                let y = hr.top() + 78.0;
                ui.painter().text(egui::pos2(hr.left() + 16.0, y), egui::Align2::LEFT_CENTER, format!("Bid {bid:.1} × {}", if bid_qty > 0 { grouped(bid_qty) } else { "—".into() }), FontId::proportional(20.0), Color32::from_gray(215));
                ui.painter().text(egui::pos2(hr.left() + 230.0, y), egui::Align2::LEFT_CENTER, format!("Ask {ask:.1} × {}", if ask_qty > 0 { grouped(ask_qty) } else { "—".into() }), FontId::proportional(20.0), Color32::from_gray(215));
                let sp = ask - bid; let sp_pct = if ask > 0.0 { sp / ask * 100.0 } else { 0.0 };
                ui.painter().text(egui::pos2(hr.right() - 14.0, y), egui::Align2::RIGHT_CENTER, format!("Sprd {sp:.1} ({sp_pct:.1}%)"), FontId::proportional(20.0), Color32::from_gray(215));
                fx::tip_rect(ui, egui::Rect::from_min_size(hr.left_top() + egui::vec2(220.0, 20.0), egui::vec2(180.0, 34.0)), "chg", "Change over the chart range below (bid price).");
            });
            ui.add_space(8.0);
            // ---- chart
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                let (box_, _) = ui.allocate_exact_size(egui::vec2(662.0, 296.0), egui::Sense::hover());
                ui.painter().rect_stroke(box_, 3.0, Stroke::new(1.0, Color32::from_gray(80)), egui::StrokeKind::Inside);
                // range tabs with a sliding underline
                let tab_y = box_.top() + 6.0; let mut x = box_.left() + 6.0; let mut sel_x = (x, x);
                for (i, (name, _)) in RANGES.iter().enumerate() {
                    let w = if *name == "All" { 52.0 } else { 56.0 };
                    let r = egui::Rect::from_min_size(egui::pos2(x, tab_y), egui::vec2(w, 30.0));
                    let resp = ui.interact(r, ui.id().with(("irange", i)), egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                    let sel = self.info_range as usize == i;
                    if sel { gradient(ui, r, Color32::from_rgb(255, 194, 16), Color32::from_rgb(255, 155, 0)); sel_x = (r.left(), r.right()); }
                    else if resp.hovered() { ui.painter().rect_filled(r, 0.0, Color32::from_gray(30)); }
                    ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, *name, if sel { FontId::new(21.0, egui::FontFamily::Name("terminal-bold".into())) } else { FontId::proportional(20.0) }, if sel { Color32::BLACK } else { Color32::WHITE });
                    if resp.clicked() { self.info_range = i as u8; self.info_pts = None; }
                    x += w + 2.0;
                }
                let (ux0, ux1) = fx::slide_indicator(ui.ctx(), ui.id().with("irange_ind"), sel_x.0, sel_x.1);
                ui.painter().rect_filled(egui::Rect::from_min_max(egui::pos2(ux0, tab_y + 28.0), egui::pos2(ux1, tab_y + 31.0)), 0.0, Color32::WHITE);
                // which price the chart follows
                let dd = egui::Rect::from_min_size(egui::pos2(box_.right() - 96.0, tab_y), egui::vec2(88.0, 30.0));
                let names = ["Bid", "Ask", "Mid"];
                let cur = self.info_series.min(2);
                let mut pick = None;
                fx::dropdown(ui, "infoseries", dd, names[cur as usize], Color32::WHITE, |ui| {
                    for (i, n) in names.iter().enumerate() { if ui.selectable_label(cur as usize == i, *n).clicked() { pick = Some(i as u8); fx::dd_close_all(ui.ctx()); } }
                });
                if let Some(p) = pick { self.info_series = p; self.info_pts = None; }
                let plot = egui::Rect::from_min_max(egui::pos2(box_.left() + 8.0, box_.top() + 46.0), egui::pos2(box_.right() - 64.0, box_.bottom() - 30.0));
                self.draw_info_chart(ui, plot, box_, &pts, mood, RANGES[self.info_range.min(5) as usize].1 <= 1440.0);
            });
            ui.add_space(8.0);
            // ---- panels
            let (tier, cat, unstackable) = item_meta(&id).unwrap_or(("", "", false));
            let (rar_name, rar_col) = rarity(tier);
            let day = self.info_points(&id, 1440.0, 0);   // 1D high / low on the ask series is not what the concept shows: use the bid series
            let _ = day;
            let day_bid = self.info_points(&id, 1440.0, 1);
            let (hi, lo) = (day_bid.iter().map(|x| x.1).fold(f64::MIN, f64::max), day_bid.iter().map(|x| x.1).fold(f64::MAX, f64::min));
            let (wsum, wamt) = p.sell_summary.iter().take(8).chain(p.buy_summary.iter().take(8)).fold((0.0, 0.0), |(s, a), l| (s + l.price * l.amount as f64, a + l.amount as f64));
            let vwap = if wamt > 0.0 { wsum / wamt } else { 0.0 };
            ui.horizontal_top(|ui| {
                ui.add_space(14.0);
                ui.vertical(|ui| {
                    ui.set_width(322.0);
                    info_head(ui, 322.0, "Market Data");
                    let hl = |v: f64, f: &dyn Fn(f64) -> String| if v.is_finite() && v.abs() < 1e15 && v != f64::MIN && v != f64::MAX { f(v) } else { "—".to_string() };
                    for (k, v, c, tip) in [("Bid", format!("{bid:.1}"), RED, "The highest price a buyer is offering: what you get if you sell instantly."),
                        ("Ask", format!("{ask:.1}"), GREEN, "The lowest price a seller wants: what you pay if you buy instantly."),
                        ("Last", format!("{last:.1}"), if up { GREEN } else { RED }, "The latest price on the chart (the bazaar publishes no last-trade price, so this is the latest bid)."),
                        ("Change", if first.is_some() { format!("{chg:+.1}") } else { "—".into() }, mood, "Price change over the chart range."),
                        ("Change %", if first.is_some() { format!("{pct:+.2}%") } else { "—".into() }, mood, "Percentage change over the chart range."),
                        ("Volume", fmt_coins(vol), Color32::WHITE, "Items traded over the last 7 days (instant buys plus instant sells)."),
                        ("VWAP", hl(vwap, &|x| format!("{x:.1}")), Color32::WHITE, "Average price weighted by how much is on offer: the top 8 bid and ask levels."),
                        ("High (1D)", hl(hi, &|x| format!("{x:.1}")), Color32::WHITE, "Highest bid in the last 24 hours."),
                        ("Low (1D)", hl(lo, &|x| format!("{x:.1}")), Color32::WHITE, "Lowest bid in the last 24 hours.")] {
                        info_line(ui, 322.0, k, &v, c, tip);
                    }
                });
                ui.add_space(8.0);
                ui.vertical(|ui| {
                    ui.set_width(332.0);
                    info_head(ui, 332.0, "Item Overview");
                    info_line(ui, 332.0, "Category", &if cat.is_empty() { "Material".to_string() } else { title_case(cat) }, Color32::WHITE, "What kind of item it is (from the SkyBlock item list; bazaar goods without one are materials).");
                    info_line(ui, 332.0, "Rarity", rar_name, rar_col, "The item's rarity tier.");
                    info_line(ui, 332.0, "NPC Sell", &npc_fmt(npc_sell_of(&id)), ORANGE, "What an NPC pays you for one.");
                    info_line(ui, 332.0, "NPC Buy", &npc_fmt(npc_buy_of(&id)), ORANGE, "What an NPC shop charges you for one. A dash means no shop sells it.");
                    info_line(ui, 332.0, "Stack Size", if unstackable { "1" } else { "64" }, Color32::WHITE, "How many fit in one inventory slot.");
                    info_line(ui, 332.0, "BZ Limit", "71,680 / order", Color32::WHITE, "The most the bazaar lets you put in one order.");
                    ui.add_space(8.0);
                    info_head(ui, 332.0, "Crafting");
                    let (cr, _) = ui.allocate_exact_size(egui::vec2(332.0, 58.0), egui::Sense::hover());
                    ui.painter().rect_stroke(cr, 0.0, Stroke::new(1.0, Color32::from_gray(70)), egui::StrokeKind::Inside);
                    if let Some((ing, n)) = iceberg::recipe_of(&id) {
                        let mut x = cr.left() + 10.0; let cy = cr.center().y;
                        if let Some(ic) = &self.icons { ic.paint(ui.painter(), &ing, egui::Rect::from_center_size(egui::pos2(x + 14.0, cy), egui::vec2(28.0, 28.0))); }
                        x += 34.0; let t = format!("{} ×{}", pretty_s(&ing), n);
                        let g = ui.painter().layout_no_wrap(t, FontId::proportional(17.0), Color32::WHITE); let w = g.size().x; ui.painter().galley(egui::pos2(x, cy - g.size().y / 2.0), g, Color32::WHITE); x += w + 10.0;
                        ui.painter().text(egui::pos2(x + 12.0, cy), egui::Align2::CENTER_CENTER, "→", FontId::proportional(24.0), ORANGE); x += 34.0;
                        if let Some(ic) = &self.icons { ic.paint(ui.painter(), &id, egui::Rect::from_center_size(egui::pos2(x + 14.0, cy), egui::vec2(28.0, 28.0))); }
                        ui.painter().text(egui::pos2(x + 34.0, cy), egui::Align2::LEFT_CENTER, format!("{} ×1", pretty_s(&id)), FontId::proportional(17.0), Color32::WHITE);
                    } else {
                        let used = iceberg::used_in(&id);
                        let txt = match used.first() { Some((out, n)) => format!("Raw material. {n} of it craft one {}", pretty_s(out)), None => "Not crafted from other bazaar goods".to_string() };
                        ui.painter().text(cr.left_center() + egui::vec2(12.0, 0.0), egui::Align2::LEFT_CENTER, txt, FontId::proportional(17.0), Color32::from_gray(200));
                    }
                });
            });
            ui.add_space(14.0);
        });
        if close { self.info_open = false; }
    }

    fn draw_info_chart(&self, ui: &mut egui::Ui, plot: egui::Rect, outer: egui::Rect, pts: &[(f64, f64)], col: Color32, intraday: bool) {
        let p = ui.painter();
        let line_col = if col == Color32::WHITE { ORANGE } else if col == GREEN { Color32::from_rgb(40, 210, 110) } else { Color32::from_rgb(255, 90, 80) };
        let line_col = if col == RED || col == GREEN { ORANGE.lerp_to_gamma(line_col, 0.0) } else { line_col };
        if pts.len() < 2 {
            if fx::loading_for(ui.ctx(), ui.id().with("info_chart_loading"), true, 0.2) && pts.is_empty() { fx::skeleton_chart(ui.ctx(), p, plot); }
            else { p.text(plot.center(), egui::Align2::CENTER_CENTER, "collecting price history…", FontId::proportional(20.0), DIM); }
            return;
        }
        let (t0, t1) = (pts.first().unwrap().0, pts.last().unwrap().0);
        let (mut lo, mut hi) = pts.iter().fold((f64::MAX, f64::MIN), |(l, h), x| (l.min(x.1), h.max(x.1)));
        let pad = ((hi - lo) * 0.08).max(hi.abs() * 0.001).max(0.05); lo -= pad; hi += pad;
        let px = |t: f64| plot.left() + ((t - t0) / (t1 - t0).max(1.0)) as f32 * plot.width();
        let py = |v: f64| plot.bottom() - ((v - lo) / (hi - lo)) as f32 * plot.height();
        let grid = Color32::from_rgba_unmultiplied(225, 236, 231, 26);
        let step = nice_step(hi - lo, 5.0);
        let mut v = (lo / step).ceil() * step;
        while v < hi {
            p.line_segment([egui::pos2(plot.left(), py(v)), egui::pos2(plot.right(), py(v))], Stroke::new(1.0, grid));
            p.text(egui::pos2(outer.right() - 8.0, py(v)), egui::Align2::RIGHT_CENTER, format!("{v:.1}"), FontId::proportional(17.0), Color32::from_gray(215));
            v += step;
        }
        // time axis
        let n = 5;
        for i in 0..n {
            let t = t0 + (t1 - t0) * (i as f64 + 0.5) / n as f64;
            let x = px(t);
            p.line_segment([egui::pos2(x, plot.top()), egui::pos2(x, plot.bottom())], Stroke::new(1.0, grid));
            p.text(egui::pos2(x, plot.bottom() + 14.0), egui::Align2::CENTER_CENTER, fmt_ts(t, !intraday), FontId::proportional(17.0), Color32::from_gray(215));
        }
        // the area under the line, fading downwards, then the line
        let mut mesh = egui::Mesh::default();
        let (top_c, bot_c) = (line_col.gamma_multiply(0.38), line_col.gamma_multiply(0.0));
        for (i, (t, val)) in pts.iter().enumerate() {
            mesh.colored_vertex(egui::pos2(px(*t), py(*val)), top_c);
            mesh.colored_vertex(egui::pos2(px(*t), plot.bottom()), bot_c);
            if i > 0 { let b = (i * 2) as u32; mesh.add_triangle(b - 2, b - 1, b); mesh.add_triangle(b - 1, b + 1, b); }
        }
        p.add(egui::Shape::mesh(mesh));
        p.add(egui::Shape::line(pts.iter().map(|(t, val)| egui::pos2(px(*t), py(*val))).collect(), Stroke::new(1.8, line_col)));
        // last price: dashed line and tag on the axis
        let last = pts.last().unwrap().1;
        p.extend(egui::Shape::dashed_line(&[egui::pos2(plot.left(), py(last)), egui::pos2(plot.right(), py(last))], Stroke::new(1.0, line_col), 3.0, 3.0));
        let tag = egui::Rect::from_center_size(egui::pos2(outer.right() - 34.0, py(last)), egui::vec2(60.0, 22.0));
        p.rect_filled(tag, 3.0, line_col);
        p.text(tag.center(), egui::Align2::CENTER_CENTER, format!("{last:.1}"), FontId::proportional(18.0), Color32::BLACK);
        p.rect_stroke(plot, 0.0, Stroke::new(1.0, Color32::from_rgb(70, 83, 80)), egui::StrokeKind::Inside);
        // crosshair with the value under the pointer
        if let Some(pos) = ui.input(|i| i.pointer.hover_pos()).filter(|q| plot.contains(*q)) {
            let tt = t0 + ((pos.x - plot.left()) / plot.width()) as f64 * (t1 - t0);
            if let Some((t, val)) = pts.iter().min_by(|a, b| (a.0 - tt).abs().total_cmp(&(b.0 - tt).abs())) {
                let (x, y) = (px(*t), py(*val));
                p.line_segment([egui::pos2(x, plot.top()), egui::pos2(x, plot.bottom())], Stroke::new(1.0, Color32::from_gray(140)));
                p.circle_filled(egui::pos2(x, y), 4.0, line_col);
                let label = format!("{val:.1}   {} {}", fmt_ts(*t, true), fmt_ts(*t, false));
                let g = p.layout_no_wrap(label, FontId::proportional(17.0), Color32::WHITE);
                let r = egui::Rect::from_min_size(egui::pos2((x + 10.0).min(plot.right() - g.size().x - 14.0), (y - 30.0).max(plot.top() + 2.0)), g.size() + egui::vec2(12.0, 6.0));
                p.rect_filled(r, 3.0, Color32::from_rgb(12, 12, 12)); p.rect_stroke(r, 3.0, Stroke::new(1.0, Color32::from_gray(100)), egui::StrokeKind::Inside);
                p.galley(r.min + egui::vec2(6.0, 3.0), g, Color32::WHITE);
            }
        }
    }
}

fn info_head(ui: &mut egui::Ui, w: f32, title: &str) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(w, 30.0), egui::Sense::hover());
    gradient(ui, r, Color32::from_rgb(255, 194, 16), Color32::from_rgb(255, 155, 0));
    ui.painter().text(r.left_center() + egui::vec2(10.0, 0.0), egui::Align2::LEFT_CENTER, title, FontId::new(22.0, egui::FontFamily::Name("terminal-bold".into())), Color32::BLACK);
}

fn info_line(ui: &mut egui::Ui, w: f32, k: &str, v: &str, col: Color32, tip: &str) {
    let (r, resp) = ui.allocate_exact_size(egui::vec2(w, 29.0), egui::Sense::hover());
    ui.painter().line_segment([r.left_bottom(), r.right_bottom()], Stroke::new(1.0, Color32::from_gray(48)));
    if resp.hovered() { ui.painter().rect_filled(r, 0.0, Color32::from_white_alpha(10)); }
    ui.painter().text(r.left_center() + egui::vec2(8.0, 0.0), egui::Align2::LEFT_CENTER, k, FontId::proportional(20.0), Color32::from_gray(225));
    ui.painter().text(r.right_center() - egui::vec2(8.0, 0.0), egui::Align2::RIGHT_CENTER, v, FontId::proportional(20.0), col);
    fx::tip(resp, tip);
}
