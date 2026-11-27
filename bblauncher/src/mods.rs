//! Mods from Modrinth, with their required dependencies.
use crate::{http, install::Instance};
use anyhow::{bail, Context, Result};
use std::{collections::HashSet, fs};

pub fn add(inst: &Instance, slug: &str) -> Result<()> {
    let mut done = HashSet::new();
    add_one(inst, slug, &mut done)
}

fn add_one(inst: &Instance, id: &str, done: &mut HashSet<String>) -> Result<()> {
    if !done.insert(id.to_string()) { return Ok(()); }
    let loaders = format!("[\"{}\"]", inst.loader);
    let games = format!("[\"{}\"]", inst.mc);
    let url = format!("https://api.modrinth.com/v2/project/{id}/version?loaders={}&game_versions={}", enc(&loaders), enc(&games));
    let vs = http::get_json(&url).with_context(|| format!("looking up '{id}' on Modrinth"))?;
    let v = vs.as_array().and_then(|a| a.first()).with_context(|| format!("'{id}' has no {} build for Minecraft {}", inst.loader, inst.mc))?;
    let files = v["files"].as_array().context("files")?;
    let f = files.iter().find(|f| f["primary"] == true).or(files.first()).context("no file")?;
    let (fname, furl) = (f["filename"].as_str().context("filename")?, f["url"].as_str().context("url")?);
    if fname.contains('/') || fname.contains("..") { bail!("refusing odd file name {fname}"); }
    let dir = inst.game_dir().join("mods");
    fs::create_dir_all(&dir)?;
    http::download(furl, &dir.join(fname), f["hashes"]["sha1"].as_str())?;
    println!("  + {fname}");
    for d in v["dependencies"].as_array().into_iter().flatten() {
        if d["dependency_type"] == "required" { if let Some(p) = d["project_id"].as_str() { add_one(inst, p, done)?; } }
    }
    Ok(())
}

fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

pub fn list(inst: &Instance) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(inst.game_dir().join("mods")).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
    v.sort();
    v
}
pub fn remove(inst: &Instance, name: &str) -> Result<()> {
    if name.contains('/') || name.contains("..") { bail!("bad name"); }
    fs::remove_file(inst.game_dir().join("mods").join(name)).context("no such mod file")
}
