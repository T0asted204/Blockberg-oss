//! The ZTrader chart (TradingView lightweight-charts) shown inside the app through a web view that sits
//! over the chart area. The page is assets/chart.html; the app pushes candles/ticks into it as JSON and
//! the page's toolbar (range, chart type, candle size, indicators) reports clicks back over IPC.
use std::sync::mpsc::{channel, Receiver};
use wry::{dpi::{PhysicalPosition, PhysicalSize}, Rect, WebView, WebViewBuilder};

pub struct WebChart { wv: WebView, rx: Receiver<String>, pub ready: bool }

fn page() -> String { include_str!("../assets/chart.html").replace("/*LWC*/", include_str!("../assets/lwc.js")) }

impl WebChart {
    pub fn new(parent: &impl raw_window_handle::HasWindowHandle, x: f64, y: f64, w: f64, h: f64) -> Result<Self, String> {
        gtk::init().map_err(|e| format!("gtk init: {e}"))?;
        let (tx, rx) = channel::<String>();
        let wv = WebViewBuilder::new()
            .with_html(page())
            .with_bounds(Rect { position: PhysicalPosition::new(x as i32, y as i32).into(), size: PhysicalSize::new(w.max(10.0) as u32, h.max(10.0) as u32).into() })
            .with_ipc_handler(move |req| { let _ = tx.send(req.body().to_string()); })
            .build_as_child(parent)
            .map_err(|e| format!("webview: {e}"))?;
        Ok(Self { wv, rx, ready: false })
    }
    pub fn set_bounds(&self, x: f64, y: f64, w: f64, h: f64) {
        let _ = self.wv.set_bounds(Rect { position: PhysicalPosition::new(x as i32, y as i32).into(), size: PhysicalSize::new(w.max(10.0) as u32, h.max(10.0) as u32).into() });
    }
    pub fn set_visible(&self, v: bool) { let _ = self.wv.set_visible(v); }
    /// Really get out of the way: set_visible alone is not honoured by every X11 setup, so also park the view off-screen, tiny.
    /// Let GTK carry out what was asked (move, show, hide). Without this a hidden view never actually hides.
    pub fn pump(&self) { while gtk::events_pending() { gtk::main_iteration_do(false); } }
    pub fn hide(&self) {
        let _ = self.wv.set_visible(false);
        let _ = self.wv.set_bounds(Rect { position: PhysicalPosition::new(-30000, -30000).into(), size: PhysicalSize::new(2u32, 2u32).into() });
        self.pump();
    }
    pub fn push(&self, json: &str) { let _ = self.wv.evaluate_script(&format!("window.bb&&window.bb.set({json})")); }
    /// Pump the GTK loop (the web view needs it) and return the page's messages.
    pub fn poll(&mut self) -> Vec<String> {
        while gtk::events_pending() { gtk::main_iteration_do(false); }
        let mut out = vec![];
        while let Ok(m) = self.rx.try_recv() { if m.contains("\"ready\"") { self.ready = true; } else { out.push(m); } }
        out
    }
}
