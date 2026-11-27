//! Interface feel for the Blockberg desktop app: easing, counting numbers, switches, tooltips, toasts, skeletons,
//! focus rings, animated dropdowns, row highlights and sliding tab indicators.
//! Everything honours "reduce motion": with it on, every animation lands instantly.
use eframe::egui::{self, Color32, Context, Id, Pos2, Rect, Response, Stroke, Ui, Vec2};
use std::sync::atomic::{AtomicBool, Ordering};

pub const ACCENT: Color32 = Color32::from_rgb(255, 170, 0);
const BAD: Color32 = Color32::from_rgb(235, 60, 60);
const GOOD: Color32 = Color32::from_rgb(40, 210, 90);

// ---------------------------------------------------------------- motion preference

static REDUCED: AtomicBool = AtomicBool::new(false);
pub fn set_reduced(b: bool) { REDUCED.store(b, Ordering::Relaxed); }
pub fn reduced() -> bool { REDUCED.load(Ordering::Relaxed) }
fn dur(d: f32) -> f32 { if reduced() { 0.0 } else { d } }

/// The OS preference: BLOCKBERG_REDUCED_MOTION=1, or XFCE / GTK animations switched off.
pub fn system_prefers_reduced() -> bool {
    if std::env::var("BLOCKBERG_REDUCED_MOTION").map_or(false, |v| v == "1" || v == "true") { return true; }
    std::process::Command::new("xfconf-query").args(["-c", "xsettings", "-p", "/Gtk/EnableAnimations"]).output().ok()
        .map_or(false, |o| String::from_utf8_lossy(&o.stdout).trim() == "false")
}

pub fn ease_out(t: f32) -> f32 { let u = 1.0 - t.clamp(0.0, 1.0); 1.0 - u * u * u }

// ---------------------------------------------------------------- tweens

#[derive(Clone, Copy)]
struct Tw { from: f32, to: f32, t0: f64, cur: f32 }

/// Ease-out tween of a value towards `target` over `d` seconds. The first call for an id lands on `target` (no animation).
pub fn tween(ctx: &Context, id: Id, target: f32, d: f32) -> f32 { tween_from(ctx, id, target, target, d) }

/// Like `tween`, but a never-seen id starts at `start`.
pub fn tween_from(ctx: &Context, id: Id, start: f32, target: f32, d: f32) -> f32 {
    let d = dur(d);
    let now = ctx.input(|i| i.time);
    let mut s = ctx.data_mut(|m| m.get_temp::<Tw>(id)).unwrap_or(Tw { from: start, to: start, t0: now, cur: start });
    if d <= 0.0 { s = Tw { from: target, to: target, t0: now, cur: target }; ctx.data_mut(|m| m.insert_temp(id, s)); return target; }
    if (s.to - target).abs() > 1e-6 { s.from = s.cur; s.to = target; s.t0 = now; }
    let p = (((now - s.t0) as f32) / d).clamp(0.0, 1.0);
    s.cur = s.from + (s.to - s.from) * ease_out(p);
    if p < 1.0 { ctx.request_repaint(); } else { s.cur = s.to; }
    ctx.data_mut(|m| m.insert_temp(id, s));
    s.cur
}

#[derive(Clone, Copy)]
struct Cn { from: f64, to: f64, t0: f64, cur: f64, last_change: f64, rapid: u8 }

/// A displayed number that counts from its old value to the new one in 300 ms (ease-out). A value that keeps changing
/// every few hundred milliseconds is shown as it is instead, so busy numbers do not wobble.
pub fn count_to(ctx: &Context, id: Id, target: f64) -> f64 {
    if !target.is_finite() { return target; }
    let now = ctx.input(|i| i.time);
    let mut s = ctx.data_mut(|m| m.get_temp::<Cn>(id)).unwrap_or(Cn { from: target, to: target, t0: now, cur: target, last_change: -10.0, rapid: 0 });
    let d = dur(0.3) as f64;
    if (s.to - target).abs() > 1e-9 {
        s.rapid = if now - s.last_change < 0.6 { s.rapid.saturating_add(1) } else { 0 };
        s.last_change = now;
        if d <= 0.0 || s.rapid >= 2 { s.from = target; s.to = target; s.cur = target; }
        else { s.from = s.cur; s.to = target; s.t0 = now; }
    }
    if (s.cur - s.to).abs() > 1e-12 {
        let p = (((now - s.t0) / d.max(1e-6)) as f32).clamp(0.0, 1.0);
        s.cur = s.from + (s.to - s.from) * ease_out(p) as f64;
        if p < 1.0 { ctx.request_repaint(); } else { s.cur = s.to; }
    }
    ctx.data_mut(|m| m.insert_temp(id, s));
    s.cur
}

// ---------------------------------------------------------------- switches / checkboxes

/// A sliding switch with its label. Returns a response that reports `changed()` when toggled.
pub fn switch(ui: &mut Ui, on: &mut bool, label: &str) -> Response {
    let mut out = None;
    ui.horizontal(|ui| {
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(38.0, 20.0), egui::Sense::click());
        let lab = ui.add(egui::Label::new(label).sense(egui::Sense::click()));
        let mut resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        if resp.clicked() || lab.clicked() { *on = !*on; resp.mark_changed(); }
        let hot = resp.hovered() || lab.hovered();
        let t = tween(ui.ctx(), resp.id.with("sw"), if *on { 1.0 } else { 0.0 }, 0.2);
        let h = tween(ui.ctx(), resp.id.with("swh"), if hot { 1.0 } else { 0.0 }, 0.15);
        let off_bg = Color32::from_gray(38).lerp_to_gamma(Color32::from_gray(52), h);
        let on_bg = Color32::from_rgb(170, 105, 0).lerp_to_gamma(Color32::from_rgb(205, 130, 0), h);
        let r = Rect::from_center_size(rect.center(), egui::vec2(34.0, 16.0));
        ui.painter().rect_filled(r, 8.0, off_bg.lerp_to_gamma(on_bg, t));
        ui.painter().rect_stroke(r, 8.0, Stroke::new(1.0, Color32::from_gray(90).lerp_to_gamma(ACCENT, t)), egui::StrokeKind::Inside);
        let x = r.left() + 8.0 + (r.width() - 16.0) * t;
        ui.painter().circle_filled(Pos2::new(x, r.center().y), 6.0 + 0.5 * h, Color32::from_gray(200).lerp_to_gamma(Color32::WHITE, t));
        out = Some(resp);
    });
    out.unwrap()
}

// ---------------------------------------------------------------- tooltips

/// Tooltip with the app's look: appears after 300 ms of hovering (or keyboard focus), fades in while rising 4 px over 150 ms,
/// leaves quickly.
pub fn tip(resp: Response, text: impl Into<String>) -> Response {
    let ctx = resp.ctx.clone();
    let id = resp.id.with("fx_tip");
    let now = ctx.input(|i| i.time);
    let hovering = resp.hovered() || resp.has_focus();
    let start = ctx.data_mut(|m| m.get_temp::<f64>(id));
    if hovering { if start.is_none() { ctx.data_mut(|m| m.insert_temp(id, now)); } }
    else if start.is_some() { ctx.data_mut(|m| m.remove::<f64>(id)); }
    let waited = hovering && start.map_or(false, |s| now - s >= 0.3);
    if hovering && !waited { ctx.request_repaint_after(std::time::Duration::from_millis(40)); }
    let t = tween(&ctx, id.with("a"), if waited { 1.0 } else { 0.0 }, if waited { 0.15 } else { 0.06 });
    if t > 0.01 {
        let screen = ctx.screen_rect();
        let anchor = ctx.input(|i| i.pointer.hover_pos()).filter(|_| resp.hovered()).unwrap_or(resp.rect.center_bottom());
        let text: String = text.into();
        let w = 300.0f32.min(screen.width() - 20.0);
        let mut pos = anchor + egui::vec2(12.0, 18.0 + (1.0 - t) * 4.0);
        // keep it on screen, and off the pointer's way when near the right / bottom edge
        if pos.x + w > screen.right() - 6.0 { pos.x = (anchor.x - w - 12.0).max(6.0); }
        if pos.y + 60.0 > screen.bottom() { pos.y = (anchor.y - 40.0).max(6.0); }
        egui::Area::new(id.with("area")).order(egui::Order::Tooltip).fixed_pos(pos).interactable(false).show(&ctx, |ui| {
            ui.set_opacity(t);
            egui::Frame::NONE.fill(Color32::from_rgb(14, 14, 14)).stroke(Stroke::new(1.0, Color32::from_gray(120))).corner_radius(4).inner_margin(egui::Margin::symmetric(8, 5)).show(ui, |ui| {
                ui.set_max_width(w - 16.0);
                ui.add(egui::Label::new(egui::RichText::new(text).size(16.0).color(Color32::from_gray(225))).wrap());
            });
        });
    }
    resp
}

/// A tooltip for something that is painted rather than a widget: hover `rect`.
pub fn tip_rect(ui: &Ui, rect: Rect, key: impl std::hash::Hash, text: &str) {
    let r = ui.interact(rect, ui.id().with(("fxtip", key)), egui::Sense::hover());
    tip(r, text);
}

// ---------------------------------------------------------------- skeletons

/// A loading placeholder shaped like the content, with a slow shimmer sweeping left to right.
pub fn skeleton(ui: &Ui, rect: Rect, rounding: f32) { skeleton_at(ui.ctx(), ui.painter(), rect, rounding); }

pub fn skeleton_at(ctx: &Context, painter: &egui::Painter, rect: Rect, rounding: f32) {
    painter.rect_filled(rect, rounding, Color32::from_gray(22));
    if reduced() { return; }
    let t = ctx.input(|i| i.time);
    let band = (rect.width() * 0.45).max(50.0);
    let x = rect.left() - band + ((t * 0.8) % 1.0) as f32 * (rect.width() + band);
    let p = painter.with_clip_rect(rect);
    let mut mesh = egui::Mesh::default();
    let clear = Color32::from_white_alpha(0);
    let mid = Color32::from_white_alpha(20);
    for (pos, c) in [(Pos2::new(x, rect.top()), clear), (Pos2::new(x + band / 2.0, rect.top()), mid), (Pos2::new(x + band, rect.top()), clear),
                     (Pos2::new(x, rect.bottom()), clear), (Pos2::new(x + band / 2.0, rect.bottom()), mid), (Pos2::new(x + band, rect.bottom()), clear)] {
        mesh.colored_vertex(pos, c);
    }
    mesh.add_triangle(0, 1, 3); mesh.add_triangle(1, 4, 3); mesh.add_triangle(1, 2, 4); mesh.add_triangle(2, 5, 4);
    p.add(egui::Shape::mesh(mesh));
    ctx.request_repaint_after(std::time::Duration::from_millis(33));
}

/// A chart-shaped placeholder: faint candle columns of varying height with the shimmer over them.
pub fn skeleton_chart(ctx: &Context, painter: &egui::Painter, rect: Rect) {
    painter.rect_filled(rect, 0.0, Color32::from_rgb(1, 2, 2));
    let n = 30usize;
    let w = rect.width() / n as f32;
    for i in 0..n {
        let hh = 0.18 + 0.45 * (((i * 37 + 11) % 17) as f32 / 17.0);
        let y = rect.center().y + (((i * 53 + 5) % 13) as f32 / 13.0 - 0.5) * rect.height() * 0.3;
        let r = Rect::from_center_size(Pos2::new(rect.left() + w * (i as f32 + 0.5), y), Vec2::new((w * 0.45).max(2.0), rect.height() * hh * 0.5));
        painter.rect_filled(r.intersect(rect.shrink(8.0)), 1.0, Color32::from_gray(20));
    }
    skeleton_at(ctx, painter, rect.shrink2(Vec2::new(0.0, 0.0)), 0.0);
}

/// Show skeleton rows only when loading has lasted a moment (no flicker for things that load at once).
pub fn loading_for(ctx: &Context, id: Id, loading: bool, min_secs: f64) -> bool {
    let now = ctx.input(|i| i.time);
    if !loading { ctx.data_mut(|m| m.remove::<f64>(id)); return false; }
    let s = ctx.data_mut(|m| m.get_temp::<f64>(id)).unwrap_or_else(|| { ctx.data_mut(|m| m.insert_temp(id, now)); now });
    if now - s < min_secs { ctx.request_repaint_after(std::time::Duration::from_millis(50)); }
    now - s >= min_secs
}

// ---------------------------------------------------------------- inputs

/// Focus ring + validation edge for a text field, eased over 150 ms. `valid: Some(false)` shows the invalid state.
pub fn focus_ring(ui: &Ui, resp: &Response, valid: Option<bool>) {
    let t = tween(ui.ctx(), resp.id.with("fxfocus"), if resp.has_focus() { 1.0 } else { 0.0 }, 0.15);
    let bad = valid == Some(false);
    let good = valid == Some(true) && resp.has_focus();
    let edge = if bad { BAD } else if good { GOOD.gamma_multiply(0.9) } else { Color32::from_gray(90).lerp_to_gamma(ACCENT, t) };
    let r = resp.rect;
    if t > 0.01 || bad {
        let a = if bad { 0.28 } else { 0.22 * t };
        ui.painter().rect_stroke(r.expand(2.0), 4.0, Stroke::new(2.0, edge.gamma_multiply(a)), egui::StrokeKind::Outside);
    }
    if t > 0.01 || bad || good { ui.painter().rect_stroke(r, 3.0, Stroke::new(1.0 + 0.5 * if bad { 1.0 } else { t }, edge), egui::StrokeKind::Inside); }
}

// ---------------------------------------------------------------- rows

/// Hover tint (150 ms) and a thin accent bar on the left when selected, for a row painted by hand. Nothing moves or resizes.
pub fn row_bg(ui: &Ui, id: Id, rect: Rect, hovered: bool, selected: bool) { row_bg_p(ui.ctx(), ui.painter(), id, rect, hovered, selected); }

pub fn row_bg_p(ctx: &Context, painter: &egui::Painter, id: Id, rect: Rect, hovered: bool, selected: bool) {
    let h = tween(ctx, id.with("rh"), if hovered { 1.0 } else { 0.0 }, 0.15);
    let s = tween(ctx, id.with("rs"), if selected { 1.0 } else { 0.0 }, 0.15);
    if h > 0.01 { painter.rect_filled(rect, 0.0, Color32::from_white_alpha((14.0 * h) as u8)); }
    if s > 0.01 {
        painter.rect_filled(rect, 0.0, ACCENT.gamma_multiply(0.10 * s));
        painter.rect_filled(Rect::from_min_max(rect.left_top(), egui::pos2(rect.left() + 2.5, rect.bottom())), 0.0, ACCENT.gamma_multiply(s));
    }
}

/// Row highlighting for an `egui::Grid` table. Create it before the grid, feed `colors()` to `with_row_color`, call
/// `row_start` at the start of every row inside, and `finish` with the grid's rectangle afterwards.
pub struct GridRows { id: Id, prev: Vec<(f32, f32)>, x: (f32, f32), tops: Vec<f32>, header: usize, sel: Option<usize>, hover: Option<usize> }

impl GridRows {
    pub fn new(ctx: &Context, id: Id, header: usize) -> Self {
        let prev = ctx.data_mut(|m| m.get_temp::<Vec<(f32, f32)>>(id.with("rows"))).unwrap_or_default();
        let x = ctx.data_mut(|m| m.get_temp::<(f32, f32)>(id.with("x"))).unwrap_or((0.0, 0.0));
        let sel = ctx.data_mut(|m| m.get_temp::<usize>(id.with("sel")));
        let hover = ctx.input(|i| i.pointer.hover_pos()).and_then(|p| {
            if p.x < x.0 || p.x > x.1 { return None; }
            prev.iter().position(|(t, b)| p.y >= *t && p.y < *b).filter(|i| *i >= header)
        });
        Self { id, prev, x, tops: vec![], header, sel, hover }
    }
    pub fn colors(&self, ctx: &Context, n: usize) -> Vec<Option<Color32>> {
        (0..n).map(|i| {
            let stripe = if i % 2 == 0 { Some(Color32::from_rgb(12, 12, 12)) } else { None };   // keeps the table's zebra striping
            if i < self.header { return stripe; }
            let h = tween(ctx, self.id.with(("h", i)), if self.hover == Some(i) { 1.0 } else { 0.0 }, 0.15);
            let s = if self.sel == Some(i) { 1.0 } else { 0.0 };
            let f = 0.07 * h + 0.14 * s;
            if f < 0.002 { return stripe; }
            let base = stripe.unwrap_or(Color32::from_rgb(0, 0, 0));
            Some(base.lerp_to_gamma(Color32::from_rgb(255, 190, 80), f))
        }).collect()
    }
    pub fn row_start(&mut self, ui: &Ui) { self.tops.push(ui.next_widget_position().y); }
    pub fn finish(&mut self, ui: &Ui, grid: Rect, spacing: f32) {
        let ctx = ui.ctx();
        let mut rows = vec![];
        for (k, t) in self.tops.iter().enumerate() {
            let b = self.tops.get(k + 1).map_or(grid.bottom(), |n| n - spacing);
            rows.push((*t, b.max(*t + 1.0)));
        }
        ctx.data_mut(|m| { m.insert_temp(self.id.with("rows"), rows.clone()); m.insert_temp(self.id.with("x"), (grid.left(), grid.right())); });
        if ctx.input(|i| i.pointer.primary_clicked()) {
            if let Some(i) = self.hover { ctx.data_mut(|m| m.insert_temp(self.id.with("sel"), i)); self.sel = Some(i); }
        }
        // the accent bar slides from row to row
        if let Some(i) = self.sel.filter(|i| *i < rows.len()) {
            let (t, b) = rows[i];
            let y0 = tween(ctx, self.id.with("by0"), t, 0.15);
            let y1 = tween(ctx, self.id.with("by1"), b, 0.15);
            ui.painter().rect_filled(Rect::from_min_max(Pos2::new(grid.left() - 3.0, y0), Pos2::new(grid.left() - 0.5, y1)), 0.0, ACCENT);
        }
        let _ = (&self.prev, &self.x);
    }
}

// ---------------------------------------------------------------- tabs

/// Slide a thin underline between tab positions (ease-out, 200 ms). Returns the animated (x0, x1).
pub fn slide_indicator(ctx: &Context, id: Id, x0: f32, x1: f32) -> (f32, f32) {
    (tween(ctx, id.with("sx0"), x0, 0.2), tween(ctx, id.with("sx1"), x1, 0.2))
}

/// Opacity and upward offset (px) for tab content that fades in each time `key` changes.
pub fn content_in(ctx: &Context, id: Id, key: u64) -> (f32, f32) {
    let last = ctx.data_mut(|m| m.get_temp::<(u64, u32)>(id.with("ck")));
    let (k, epoch) = match last { Some((k, e)) if k == key => (k, e), Some((_, e)) => (key, e + 1), None => (key, 0) };
    ctx.data_mut(|m| m.insert_temp(id.with("ck"), (k, epoch)));
    let start = if epoch == 0 { 1.0 } else { 0.0 };
    let t = tween_from(ctx, id.with(("cin", epoch)), start, 1.0, 0.22);
    (t, (1.0 - t) * 8.0)
}

// ---------------------------------------------------------------- dropdowns

pub fn dd_is_open(ctx: &Context, id: Id) -> bool { ctx.data_mut(|m| m.get_temp::<bool>(id.with("ddopen"))).unwrap_or(false) }
pub fn dd_toggle(ctx: &Context, id: Id) {
    let now = dd_is_open(ctx, id);
    dd_close_all(ctx);
    ctx.data_mut(|m| m.insert_temp(id.with("ddopen"), !now));
    ctx.data_mut(|m| m.insert_temp(Id::new("fx_dd_current"), if now { None } else { Some(id) }));
}
pub fn dd_close_all(ctx: &Context) {
    if let Some(Some(cur)) = ctx.data_mut(|m| m.get_temp::<Option<Id>>(Id::new("fx_dd_current"))) {
        ctx.data_mut(|m| m.insert_temp(cur.with("ddopen"), false));
    }
    ctx.data_mut(|m| m.insert_temp(Id::new("fx_dd_current"), None::<Id>));
}

/// A drop-down panel under `anchor`. Opens by fading in while rising 5 px (150 ms); closes the same way, faster (100 ms).
/// Escape or a click outside closes it. `add` draws the contents (and may call `dd_close_all`).
pub fn dd_popup(ctx: &Context, id: Id, anchor: Rect, min_width: f32, add: impl FnOnce(&mut Ui)) {
    let open = dd_is_open(ctx, id);
    let t = tween(ctx, id.with("ddt"), if open { 1.0 } else { 0.0 }, if open { 0.15 } else { 0.10 });
    if !open && t <= 0.002 { return; }
    let pos = anchor.left_bottom() + egui::vec2(0.0, 2.0 + (1.0 - t) * 5.0);
    let area = egui::Area::new(id.with("ddarea")).order(egui::Order::Foreground).fixed_pos(pos).interactable(open).show(ctx, |ui| {
        ui.set_opacity(t);
        egui::Frame::popup(ui.style()).show(ui, |ui| { ui.set_min_width(min_width); add(ui); });
    });
    if open {
        let rect = area.response.rect;
        let (pressed, pos, esc) = ctx.input(|i| (i.pointer.any_pressed(), i.pointer.interact_pos(), i.key_pressed(egui::Key::Escape)));
        if esc { dd_close_all(ctx); }
        else if pressed { if let Some(p) = pos { if !rect.contains(p) && !anchor.contains(p) { dd_close_all(ctx); } } }
    }
}

/// A rotating chevron: 0 = pointing down, 1 = pointing up.
pub fn chevron(painter: &egui::Painter, center: Pos2, size: f32, t: f32, color: Color32) {
    let a = t * std::f32::consts::PI;
    let (s, c) = a.sin_cos();
    let rot = |v: Vec2| Vec2::new(v.x * c - v.y * s, v.x * s + v.y * c);
    let pts: Vec<Pos2> = [Vec2::new(-size, -size * 0.45), Vec2::new(size, -size * 0.45), Vec2::new(0.0, size * 0.55)].iter().map(|v| center + rot(*v)).collect();
    painter.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
}

/// The ticket-style field dropdown (black field, grey outline). `contents` draws the options; call `dd_close_all` on a pick.
pub fn dropdown(ui: &mut Ui, salt: &str, rect: Rect, text: &str, text_color: Color32, contents: impl FnOnce(&mut Ui)) {
    let ctx = ui.ctx().clone();
    let id = ui.id().with(("fxdd", salt));
    let resp = ui.interact(rect, id, egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
    if resp.clicked() { dd_toggle(&ctx, id); }
    let open = dd_is_open(&ctx, id);
    let h = tween(&ctx, id.with("h"), if resp.hovered() || open { 1.0 } else { 0.0 }, 0.15);
    let o = tween(&ctx, id.with("o"), if open { 1.0 } else { 0.0 }, 0.15);
    ui.painter().rect_filled(rect, 3.0, Color32::from_rgb(2, 2, 2).lerp_to_gamma(Color32::from_gray(22), h));
    ui.painter().rect_stroke(rect, 3.0, Stroke::new(1.0, Color32::from_gray(150).lerp_to_gamma(Color32::from_gray(215), h)), egui::StrokeKind::Inside);
    ui.painter().text(rect.left_center() + egui::vec2(9.0, 0.0), egui::Align2::LEFT_CENTER, text, egui::FontId::proportional(20.0), text_color);
    chevron(ui.painter(), rect.right_center() - egui::vec2(14.0, 0.0), 5.5, o, Color32::from_gray(220));
    dd_popup(&ctx, id, rect, rect.width(), contents);
}

// ---------------------------------------------------------------- toasts

#[derive(Clone)]
pub struct Toast { pub id: u64, pub text: String, pub kind: u8, pub born: f64, pub done: Option<f64> }   // kind: 0 working, 1 ok, 2 error

/// Compact notifications at the top right: they slide in from the right while fading in, stack downwards, and leave on
/// their own a few seconds after they have finished. Returns the ids that are fully gone.
pub fn show_toasts(ctx: &Context, toasts: &[Toast]) -> Vec<u64> {
    let now = ctx.input(|i| i.time);
    let screen = ctx.screen_rect();
    let mut gone = vec![];
    let mut y = 66.0;   // distance up from the bottom edge; the newest notification sits lowest
    for t in toasts.iter().rev() {
        let id = Id::new(("fxtoast", t.id));
        let age_done = t.done.map(|d| now - d);
        let leaving = age_done.map_or(false, |a| a > 3.2);
        let appear = tween_from(ctx, id.with("in"), 0.0, if leaving { 0.0 } else { 1.0 }, if leaving { 0.25 } else { 0.22 });
        if leaving && appear <= 0.01 { gone.push(t.id); continue; }
        let ty = tween(ctx, id.with("y"), screen.bottom() - y, 0.2);
        let slide = (1.0 - appear) * 36.0;
        let area = egui::Area::new(id).order(egui::Order::Tooltip).fixed_pos(Pos2::new(screen.right() - 12.0 + slide, ty)).pivot(egui::Align2::RIGHT_BOTTOM).interactable(false).show(ctx, |ui| {
            ui.set_opacity(appear);
            let (edge, icon) = match t.kind { 1 => (GOOD, "✔"), 2 => (BAD, "✖"), _ => (ACCENT, "") };
            egui::Frame::NONE.fill(Color32::from_rgb(12, 12, 12)).stroke(Stroke::new(1.0, Color32::from_gray(80))).corner_radius(4).inner_margin(egui::Margin { left: 12, right: 10, top: 6, bottom: 6 }).show(ui, |ui| {
                ui.set_max_width(360.0);
                ui.horizontal(|ui| {
                    if t.kind == 0 { ui.add(egui::Spinner::new().size(15.0).color(ACCENT)); } else { ui.label(egui::RichText::new(icon).color(edge).size(18.0)); }
                    ui.add(egui::Label::new(egui::RichText::new(&t.text).size(17.0).color(Color32::from_gray(230))).wrap());
                });
                let r = ui.min_rect();
                ui.painter().rect_filled(Rect::from_min_max(Pos2::new(r.left() - 12.0, r.top() - 6.0), Pos2::new(r.left() - 9.0, r.bottom() + 6.0)), 2.0, edge);
            });
        });
        let h = area.response.rect.height();
        ctx.data_mut(|m| m.insert_temp(id.with("h"), h));
        y += ctx.data_mut(|m| m.get_temp::<f32>(id.with("h"))).unwrap_or(40.0) + 8.0;
    }
    gone
}

#[cfg(test)]
mod tests {
    use super::*;
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());   // the motion preference is global: tests that touch it run one at a time

    fn at(ctx: &Context, t: f64, f: impl FnOnce(&Context)) {
        let mut input = egui::RawInput::default();
        input.time = Some(t);
        let mut f = Some(f);
        let _ = ctx.run(input, |c| { if let Some(f) = f.take() { f(c); } });
    }

    #[test]
    fn tween_eases_out_and_lands_exactly() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_reduced(false);
        let ctx = Context::default();
        let id = Id::new("t");
        let mut v = vec![];
        for (t, target) in [(0.0, 0.0f32), (0.1, 1.0), (0.2, 1.0), (0.3, 1.0), (0.5, 1.0)] { at(&ctx, t, |c| v.push(tween(c, id, target, 0.2))); }
        assert_eq!(v[0], 0.0);
        assert!(v[1] < 0.2, "just started: {v:?}");
        assert!(v[2] > 0.5 && v[2] < 1.0, "ease-out is past halfway at half time: {v:?}");
        assert_eq!(v[3], 1.0);
        assert_eq!(v[4], 1.0);
    }

    #[test]
    fn numbers_count_up_then_stop_and_busy_numbers_snap() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_reduced(false);
        let ctx = Context::default();
        let id = Id::new("n");
        let mut v = vec![];
        for (t, target) in [(0.0, 100.0), (1.0, 200.0), (1.15, 200.0), (1.5, 200.0)] { at(&ctx, t, |c| v.push(count_to(c, id, target))); }
        assert_eq!(v[0], 100.0, "the first value is shown as it is");
        assert!(v[1] >= 100.0 && v[1] < 150.0, "{v:?}");
        assert!(v[2] > 150.0 && v[2] < 200.0, "{v:?}");
        assert_eq!(v[3], 200.0);
        // a value that changes every 200 ms is not animated again and again
        let id2 = Id::new("busy");
        let mut w = vec![];
        for (i, t) in [0.0, 0.2, 0.4, 0.6].iter().enumerate() { at(&ctx, *t, |c| w.push(count_to(c, id2, 10.0 * (i + 1) as f64))); }
        assert_eq!(w[3], 40.0, "{w:?}");
    }

    #[test]
    fn reduced_motion_makes_everything_instant() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_reduced(true);
        let ctx = Context::default();
        let mut out = vec![];
        for (t, target) in [(0.0, 0.0f32), (0.01, 1.0)] { at(&ctx, t, |c| out.push((tween(c, Id::new("r"), target, 0.2), count_to(c, Id::new("rn"), target as f64 * 50.0)))); }
        set_reduced(false);
        assert_eq!(out[1], (1.0, 50.0));
    }

    #[test]
    fn skeletons_wait_a_moment_before_showing() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ctx = Context::default();
        let id = Id::new("l");
        let mut seen = vec![];
        for t in [0.0, 0.05, 0.3] { at(&ctx, t, |c| seen.push(loading_for(c, id, true, 0.15))); }
        at(&ctx, 0.4, |c| seen.push(loading_for(c, id, false, 0.15)));
        assert_eq!(seen, vec![false, false, true, false]);
    }
}

// ---------------------------------------------------------------- menu rows (shared by every dropdown)

/// A full-width menu row in the same look as the right-click menu: dark, orange highlight under the pointer.
pub fn menu_item_en(ui: &mut Ui, enabled: bool, label: &str) -> Response {
    let w = ui.available_width().max(250.0);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 30.0), if enabled { egui::Sense::click() } else { egui::Sense::hover() });
    let hot = enabled && resp.hovered();
    let t = tween(ui.ctx(), resp.id.with("mh"), if hot { 1.0 } else { 0.0 }, 0.12);
    if t > 0.01 { ui.painter().rect_filled(rect.shrink2(egui::vec2(3.0, 1.0)), 3.0, Color32::from_rgb(30, 22, 4).lerp_to_gamma(ACCENT, t)); }
    let base = if enabled { Color32::from_gray(225) } else { Color32::from_gray(100) };
    ui.painter().text(rect.left_center() + egui::vec2(14.0, 0.0), egui::Align2::LEFT_CENTER, label, egui::FontId::proportional(20.0), base.lerp_to_gamma(Color32::BLACK, t));
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}
pub fn menu_item(ui: &mut Ui, label: &str) -> Response { menu_item_en(ui, true, label) }

/// A full-width row with a switch on the right.
pub fn menu_switch(ui: &mut Ui, on: &mut bool, label: &str) -> Response {
    let w = ui.available_width().max(250.0);
    let (rect, mut resp) = ui.allocate_exact_size(egui::vec2(w, 30.0), egui::Sense::click());
    if resp.clicked() { *on = !*on; resp.mark_changed(); }
    let h = tween(ui.ctx(), resp.id.with("mh"), if resp.hovered() { 1.0 } else { 0.0 }, 0.12);
    if h > 0.01 { ui.painter().rect_filled(rect.shrink2(egui::vec2(3.0, 1.0)), 3.0, Color32::from_rgb(30, 22, 4).lerp_to_gamma(Color32::from_rgb(46, 34, 8), h)); }
    ui.painter().text(rect.left_center() + egui::vec2(14.0, 0.0), egui::Align2::LEFT_CENTER, label, egui::FontId::proportional(20.0), Color32::from_gray(225));
    let t = tween(ui.ctx(), resp.id.with("sw"), if *on { 1.0 } else { 0.0 }, 0.2);
    let r = Rect::from_center_size(rect.right_center() - egui::vec2(30.0, 0.0), egui::vec2(34.0, 16.0));
    ui.painter().rect_filled(r, 8.0, Color32::from_gray(38).lerp_to_gamma(Color32::from_rgb(190, 118, 0), t));
    ui.painter().rect_stroke(r, 8.0, Stroke::new(1.0, Color32::from_gray(90).lerp_to_gamma(ACCENT, t)), egui::StrokeKind::Inside);
    let x = r.left() + 8.0 + (r.width() - 16.0) * t;
    ui.painter().circle_filled(Pos2::new(x, r.center().y), 6.0, Color32::from_gray(200).lerp_to_gamma(Color32::WHITE, t));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A thin divider between groups of menu rows.
pub fn menu_sep(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 9.0), egui::Sense::hover());
    ui.painter().line_segment([rect.left_center() + egui::vec2(8.0, 0.0), rect.right_center() - egui::vec2(8.0, 0.0)], Stroke::new(1.0, Color32::from_gray(70)));
}
