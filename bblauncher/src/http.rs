use anyhow::{Context, Result};
use reqwest::blocking::Client;
use sha1::{Digest, Sha1};
use std::{fs, io::Write, path::Path, sync::OnceLock, time::Duration};

pub fn client() -> &'static Client {
    static C: OnceLock<Client> = OnceLock::new();
    C.get_or_init(|| Client::builder().user_agent("bblauncher/0.1").timeout(Duration::from_secs(300)).build().expect("http client"))
}

pub fn get_json(url: &str) -> Result<serde_json::Value> {
    let r = client().get(url).send().with_context(|| format!("GET {url}"))?;
    let st = r.status();
    if !st.is_success() { anyhow::bail!("GET {url}: HTTP {st}"); }
    Ok(r.json()?)
}

pub fn sha1_hex(bytes: &[u8]) -> String { Sha1::digest(bytes).iter().map(|b| format!("{b:02x}")).collect() }

/// Download to `dest` (via a temp file, so a half-finished download is never mistaken for a good one).
/// When `sha1` is given the file is checked, and an existing file that matches is kept.
pub fn download(url: &str, dest: &Path, sha1: Option<&str>) -> Result<()> {
    if dest.exists() {
        match sha1 {
            Some(h) => { if sha1_hex(&fs::read(dest)?) == h { return Ok(()); } }
            None => return Ok(()),
        }
    }
    if let Some(p) = dest.parent() { fs::create_dir_all(p)?; }
    let mut last = None;
    for attempt in 0..3 {
        let res = (|| -> Result<()> {
            let r = client().get(url).send()?;
            if !r.status().is_success() { anyhow::bail!("HTTP {}", r.status()); }
            let bytes = r.bytes()?;
            if let Some(h) = sha1 { let got = sha1_hex(&bytes); if got != h { anyhow::bail!("checksum mismatch (got {got}, want {h})"); } }
            let tmp = dest.with_extension("part");
            fs::File::create(&tmp)?.write_all(&bytes)?;
            fs::rename(&tmp, dest)?;
            Ok(())
        })();
        match res { Ok(()) => return Ok(()), Err(e) => { last = Some(e); std::thread::sleep(Duration::from_millis(400 * (attempt + 1))); } }
    }
    Err(last.unwrap()).with_context(|| format!("download {url}"))
}

/// Run `f` over every item on `n` threads; returns the first error.
pub fn parallel<T: Send + Sync, F: Fn(&T) -> Result<()> + Sync>(items: &[T], n: usize, f: F) -> Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    let err: std::sync::Mutex<Option<anyhow::Error>> = std::sync::Mutex::new(None);
    std::thread::scope(|s| {
        for _ in 0..n.max(1) {
            s.spawn(|| loop {
                if err.lock().unwrap().is_some() { return; }
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(it) = items.get(i) else { return };
                if let Err(e) = f(it) { *err.lock().unwrap() = Some(e); return; }
            });
        }
    });
    match err.into_inner().unwrap() { Some(e) => Err(e), None => Ok(()) }
}
