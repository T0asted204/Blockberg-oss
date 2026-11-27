//! One-minute price candles, shared by the recorder (`blockberg-rec`) and the app.
//! Files: <data dir>/candles/<PRODUCT_ID>.bin, append-only, 48-byte fixed records:
//!   t:i64 | ask[open,high,low,close]:4xf32 | bid[open,high,low,close]:4xf32 | n:u32 (snapshots in the minute) | 4 pad bytes
#![allow(dead_code)]
use std::{collections::BTreeMap, fs, io::{Read, Seek, SeekFrom, Write}, path::PathBuf};

pub const REC: usize = 48;

/// Items recorded even before the app has added them to the list.
pub const DEFAULT_RECORD: &[&str] = &[
    "COAL", "ENCHANTED_COAL", "ENCHANTED_COAL_BLOCK", "IRON_INGOT", "ENCHANTED_IRON", "GOLD_INGOT", "ENCHANTED_GOLD",
    "DIAMOND", "ENCHANTED_DIAMOND", "COBBLESTONE", "ENCHANTED_COBBLESTONE", "WHEAT", "ENCHANTED_BREAD", "SUGAR_CANE",
    "ENCHANTED_SUGAR", "SEEDS", "ENCHANTED_SEEDS", "BOX_OF_SEEDS", "CARROT_ITEM", "ENCHANTED_CARROT", "POTATO_ITEM",
    "ENCHANTED_POTATO", "PUMPKIN", "ENCHANTED_PUMPKIN", "MELON", "ENCHANTED_MELON", "REDSTONE", "ENCHANTED_REDSTONE",
    "EMERALD", "ENCHANTED_EMERALD", "LAPIS_LAZULI", "ENCHANTED_LAPIS_LAZULI", "OBSIDIAN", "ENCHANTED_OBSIDIAN",
];

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Candle { pub t: i64, pub ask: [f32; 4], pub bid: [f32; 4], pub n: u32 }

impl Candle {
    pub fn new(t: i64, ask: f32, bid: f32) -> Self { Candle { t, ask: [ask; 4], bid: [bid; 4], n: 1 } }

    /// Fold one price snapshot into this (still open) candle.
    pub fn tick(&mut self, ask: f32, bid: f32) {
        for (s, v) in [(&mut self.ask, ask), (&mut self.bid, bid)] { s[1] = s[1].max(v); s[2] = s[2].min(v); s[3] = v; }
        self.n += 1;
    }

    /// Absorb a *later* candle into this one (used to build 5m / 1h / 1d candles from minutes).
    pub fn absorb(&mut self, o: &Candle) {
        for (s, x) in [(&mut self.ask, &o.ask), (&mut self.bid, &o.bid)] { s[1] = s[1].max(x[1]); s[2] = s[2].min(x[2]); s[3] = x[3]; }
        self.n += o.n;
    }

    /// [open, high, low, close] for 0 = ask (insta-buy), 1 = bid (insta-sell), 2 = mid.
    pub fn series(&self, which: u8) -> [f32; 4] {
        match which {
            0 => self.ask,
            1 => self.bid,
            _ => [(self.ask[0] + self.bid[0]) / 2.0, (self.ask[1] + self.bid[1]) / 2.0, (self.ask[2] + self.bid[2]) / 2.0, (self.ask[3] + self.bid[3]) / 2.0],
        }
    }

    pub fn encode(&self) -> [u8; REC] {
        let mut b = [0u8; REC];
        b[..8].copy_from_slice(&self.t.to_le_bytes());
        for (i, v) in self.ask.iter().chain(self.bid.iter()).enumerate() { b[8 + i * 4..12 + i * 4].copy_from_slice(&v.to_le_bytes()); }
        b[40..44].copy_from_slice(&self.n.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Option<Candle> {
        if b.len() < REC { return None; }
        let f = |i: usize| f32::from_le_bytes(b[8 + i * 4..12 + i * 4].try_into().unwrap());
        Some(Candle { t: i64::from_le_bytes(b[..8].try_into().ok()?), ask: [f(0), f(1), f(2), f(3)], bid: [f(4), f(5), f(6), f(7)], n: u32::from_le_bytes(b[40..44].try_into().ok()?) })
    }
}

pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("BLOCKBERG_DATA") { return d.into(); }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/blockberg")).unwrap_or_else(|| PathBuf::from("."))
}

pub fn candle_path(id: &str) -> PathBuf { data_dir().join("candles").join(format!("{id}.bin")) }

pub fn append(id: &str, c: &Candle) -> std::io::Result<()> {
    let p = candle_path(id);
    if let Some(d) = p.parent() { fs::create_dir_all(d)?; }
    fs::OpenOptions::new().create(true).append(true).open(p)?.write_all(&c.encode())
}

/// The last `max_records` minute candles of an item (oldest first). Reads only the tail of the file.
pub fn load(id: &str, max_records: usize) -> Vec<Candle> {
    let Ok(mut f) = fs::File::open(candle_path(id)) else { return vec![] };
    let n = f.metadata().map(|m| m.len() as usize / REC).unwrap_or(0);
    let start = n.saturating_sub(max_records);
    if f.seek(SeekFrom::Start((start * REC) as u64)).is_err() { return vec![]; }
    let mut buf = Vec::with_capacity((n - start) * REC);
    if f.read_to_end(&mut buf).is_err() { return vec![]; }
    let v: Vec<Candle> = buf.chunks_exact(REC).filter_map(Candle::decode).collect();
    merge(&v, &[]) // a restart inside one minute can leave two records for it: keep the fuller one
}

/// Group (oldest-first) minute candles into `tf`-second buckets.
pub fn aggregate(src: &[Candle], tf: i64) -> Vec<Candle> {
    if tf <= 60 { return src.to_vec(); }
    let mut out: Vec<Candle> = Vec::new();
    for c in src {
        let b = c.t - c.t.rem_euclid(tf);
        match out.last_mut() {
            Some(l) if l.t == b => l.absorb(c),
            _ => { let mut n = *c; n.t = b; out.push(n); }
        }
    }
    out
}

/// Minute candles from the app's in-memory (unix secs, ask, bid) snapshots.
/// Candles of `secs` seconds from (unix secs, ask, bid) snapshots (for the sub-minute chart sizes).
pub fn from_snapshots_secs(h: &[(f64, f64, f64)], secs: i64) -> Vec<Candle> {
    let secs = secs.max(1);
    let mut out: Vec<Candle> = Vec::new();
    for &(t, a, b) in h {
        let t = t as i64;
        let m = t - t.rem_euclid(secs);
        match out.last_mut() {
            Some(l) if l.t == m => l.tick(a as f32, b as f32),
            _ => out.push(Candle::new(m, a as f32, b as f32)),
        }
    }
    out
}

pub fn from_snapshots(h: &[(f64, f64, f64)]) -> Vec<Candle> {
    let mut out: Vec<Candle> = Vec::new();
    for &(t, a, b) in h {
        let t = t as i64;
        let m = t - t.rem_euclid(60);
        match out.last_mut() {
            Some(l) if l.t == m => l.tick(a as f32, b as f32),
            _ => out.push(Candle::new(m, a as f32, b as f32)),
        }
    }
    out
}

/// Union of two minute series; where both have a minute, keep the one built from more snapshots.
pub fn merge(a: &[Candle], b: &[Candle]) -> Vec<Candle> {
    let mut m: BTreeMap<i64, Candle> = BTreeMap::new();
    for c in a.iter().chain(b.iter()) {
        match m.get(&c.t) { Some(old) if old.n >= c.n => {} _ => { m.insert(c.t, *c); } }
    }
    m.into_values().collect()
}

pub fn list_path() -> PathBuf { data_dir().join("record_list.json") }

fn read_list() -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(list_path()).ok()?).ok()?;
    Some(v.as_array()?.iter().filter_map(|x| x.as_str().map(String::from)).collect())
}

/// What the recorder should record: the saved list, or the defaults.
pub fn load_list() -> Vec<String> {
    read_list().unwrap_or_else(|| DEFAULT_RECORD.iter().map(|s| s.to_string()).collect())
}

/// Make sure these items are in the recorder's list (the app calls this for its watchlist).
pub fn ensure_in_list(ids: &[String]) {
    let mut list = load_list();
    let before = list.len();
    for id in ids { if !list.contains(id) { list.push(id.clone()); } }
    if list.len() != before || !list_path().exists() {
        if let Some(d) = list_path().parent() { let _ = fs::create_dir_all(d); }
        let _ = fs::write(list_path(), serde_json::to_string(&list).unwrap_or_default());
    }
}

// ---------------------------------------------------------------- whole-file operations (merge, prune, list)

/// Every item that has a candle file.
pub fn list_ids() -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(data_dir().join("candles")).into_iter().flatten().flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".bin")).map(String::from)).collect();
    v.sort();
    v
}

/// Replace an item's file with exactly these (oldest-first) candles, atomically: a crash leaves the old file or the new one, never half.
pub fn replace_all(id: &str, cs: &[Candle]) -> std::io::Result<()> {
    let p = candle_path(id);
    if let Some(d) = p.parent() { fs::create_dir_all(d)?; }
    let tmp = p.with_extension("bin.tmp");
    let mut buf = Vec::with_capacity(cs.len() * REC);
    for c in cs { buf.extend_from_slice(&c.encode()); }
    fs::write(&tmp, buf)?;
    fs::rename(&tmp, &p)
}

/// Fold incoming candles into an item's file (union by minute, the fuller candle wins). Returns how many minutes were new.
pub fn merge_into_file(id: &str, incoming: &[Candle]) -> std::io::Result<usize> {
    let have = load(id, usize::MAX);
    let before = have.len();
    let merged = merge(&have, incoming);
    let added = merged.len().saturating_sub(before);
    if merged != have { replace_all(id, &merged)?; }
    Ok(added)
}

/// Drop everything older than `min_t` (unix seconds) from an item's file.
pub fn prune_before(id: &str, min_t: i64) -> std::io::Result<()> {
    let have = load(id, usize::MAX);
    let keep: Vec<Candle> = have.iter().copied().filter(|c| c.t >= min_t).collect();
    if keep.len() != have.len() { replace_all(id, &keep)?; }
    Ok(())
}

/// Candles newer than `since` (unix seconds).
pub fn load_since(id: &str, since: i64) -> Vec<Candle> { load(id, usize::MAX).into_iter().filter(|c| c.t >= since).collect() }

#[cfg(test)]
mod store_tests {
    use super::*;
    #[test]
    fn merge_prune_and_dedupe_on_disk() {
        let dir = std::env::temp_dir().join(format!("bb_candles_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        std::env::set_var("BLOCKBERG_DATA", &dir);
        let c = |t: i64, p: f32, n: u32| Candle { t, ask: [p; 4], bid: [p; 4], n };
        // main has minutes 0..4 (60 s apart); the standby kept 2..8 with a fuller minute 3
        let main: Vec<Candle> = (0..5).map(|i| c(i * 60, 1.0, 3)).collect();
        replace_all("X", &main).unwrap();
        let standby: Vec<Candle> = (2..9).map(|i| c(i * 60, 2.0, if i == 3 { 6 } else { 1 })).collect();
        let added = merge_into_file("X", &standby).unwrap();
        assert_eq!(added, 4, "minutes 5..8 are new");
        let all = load("X", usize::MAX);
        assert_eq!(all.len(), 9);
        assert_eq!(all[3].ask[0], 2.0, "the fuller candle (n=6) won minute 3");
        assert_eq!(all[2].ask[0], 1.0, "a thinner incoming candle (n=1) does not replace n=3");
        // merging the same thing again changes nothing
        assert_eq!(merge_into_file("X", &standby).unwrap(), 0);
        // prune keeps only recent minutes
        prune_before("X", 5 * 60).unwrap();
        let kept = load("X", usize::MAX);
        assert_eq!(kept.iter().map(|k| k.t).collect::<Vec<_>>(), vec![300, 360, 420, 480]);
        assert_eq!(list_ids(), vec!["X".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }
}

/// Roll minute candles older than `before` (unix secs) up into `tf`-second candles; already-rolled ones stay as they are.
pub fn compact_vec(have: &[Candle], before: i64, tf: i64) -> Vec<Candle> {
    let cut = before - before.rem_euclid(tf);
    let (old, new): (Vec<Candle>, Vec<Candle>) = have.iter().copied().partition(|c| c.t < cut);
    let mut out = aggregate(&old, tf);
    out.extend(new);
    out
}

/// Same, on an item's file, to keep the files small. Returns how many records were saved.
pub fn compact_before(id: &str, before: i64, tf: i64) -> std::io::Result<usize> {
    let have = load(id, usize::MAX);
    let out = compact_vec(&have, before, tf);
    if out.len() == have.len() { return Ok(0); }
    replace_all(id, &out)?;
    Ok(have.len() - out.len())
}

#[cfg(test)]
mod compact_tests {
    #[test]
    fn rolls_old_minutes_into_hours() {
        let cs: Vec<super::Candle> = (0..180).map(|i| super::Candle::new(i * 60, i as f32, 1.0)).collect();
        let after = super::compact_vec(&cs, 7200, 3600);
        assert_eq!(after.len(), 62);
        assert_eq!(after[0].ask, [0.0, 59.0, 0.0, 59.0]);
        assert_eq!(after[1].t, 3600);
        assert_eq!(super::compact_vec(&after, 7200, 3600), after);
    }
}
