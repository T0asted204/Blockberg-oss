//! Downloads Minecraft (vanilla or Fabric) into the shared store and records an instance.
use crate::{http, paths};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fs, path::PathBuf};

#[derive(Serialize, Deserialize, Clone)]
pub struct Instance {
    pub name: String,
    pub mc: String,
    pub loader: String,            // "fabric" | "vanilla"
    pub version_id: String,        // the profile launched (fabric-loader-X-MC or the vanilla id)
    pub java_major: u64,
    pub memory_mb: u64,
}

impl Instance {
    pub fn dir(&self) -> PathBuf { paths::instance(&self.name) }
    pub fn game_dir(&self) -> PathBuf { self.dir().join("game") }
    pub fn load(name: &str) -> Result<Instance> {
        let p = paths::instance(paths::safe_name(name)?).join("instance.json");
        serde_json::from_slice(&fs::read(&p).with_context(|| format!("no instance '{name}' (run: bblauncher install {name} <mc-version>)"))?).context("instance.json is damaged")
    }
    pub fn save(&self) -> Result<()> {
        fs::create_dir_all(self.game_dir())?;
        fs::write(self.dir().join("instance.json"), serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

pub fn list_instances() -> Vec<Instance> {
    let mut v = vec![];
    if let Ok(rd) = fs::read_dir(paths::instances()) { for e in rd.flatten() { if let Ok(i) = Instance::load(&e.file_name().to_string_lossy()) { v.push(i); } } }
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// "org.lwjgl:lwjgl:3.4.1:natives-linux" → "org/lwjgl/lwjgl/3.4.1/lwjgl-3.4.1-natives-linux.jar"
pub fn maven_path(coord: &str) -> Result<String> {
    let p: Vec<&str> = coord.split(':').collect();
    if p.len() < 3 { bail!("bad library name {coord}"); }
    let (g, a, v) = (p[0].replace('.', "/"), p[1], p[2]);
    let ext_split: Vec<&str> = v.split('@').collect();
    let (v, ext) = (ext_split[0], ext_split.get(1).copied().unwrap_or("jar"));
    Ok(match p.get(3) { Some(c) => format!("{g}/{a}/{v}/{a}-{v}-{c}.{ext}"), None => format!("{g}/{a}/{v}/{a}-{v}.{ext}") })
}

/// The launcher's rule language: a list of allow/disallow entries; the last matching one wins.
pub fn rules_allow(rules: Option<&Value>, features: &[&str]) -> bool {
    let Some(rules) = rules.and_then(|r| r.as_array()) else { return true };
    let mut ok = false;
    for r in rules {
        let mut matches = true;
        if let Some(os) = r.get("os") {
            if let Some(n) = os["name"].as_str() { if n != "linux" { matches = false; } }
            if let Some(a) = os["arch"].as_str() { if a != "x86_64" && a != "amd64" { matches = false; } }
            if os.get("versionRange").is_some() { matches = false; }
        }
        if let Some(f) = r.get("features").and_then(|f| f.as_object()) {
            for (k, v) in f { if v.as_bool().unwrap_or(false) != features.contains(&k.as_str()) { matches = false; } }
        }
        if matches { ok = r["action"] == "allow"; }
    }
    ok
}

fn vanilla_json(mc: &str) -> Result<Value> {
    let p = paths::versions().join(mc).join(format!("{mc}.json"));
    if p.exists() { return Ok(serde_json::from_slice(&fs::read(&p)?)?); }
    println!("looking up Minecraft {mc}...");
    let m = http::get_json("https://piston-meta.mojang.com/mc/game/version_manifest_v2.json")?;
    let v = m["versions"].as_array().context("manifest")?.iter().find(|v| v["id"] == mc).with_context(|| format!("Minecraft {mc} does not exist"))?;
    let (url, sha) = (v["url"].as_str().unwrap(), v["sha1"].as_str());
    http::download(url, &p, sha)?;
    Ok(serde_json::from_slice(&fs::read(&p)?)?)
}

pub fn version_json(id: &str) -> Result<Value> {
    let p = paths::versions().join(id).join(format!("{id}.json"));
    serde_json::from_slice(&fs::read(&p).with_context(|| format!("version {id} is not installed"))?).context("version json")
}

fn library_jobs(libs: &Value, out: &mut Vec<(String, PathBuf, Option<String>)>) -> Result<()> {
    for l in libs.as_array().context("libraries")? {
        if !rules_allow(l.get("rules"), &[]) { continue; }
        let name = l["name"].as_str().unwrap_or("");
        if let Some(a) = l.get("downloads").and_then(|d| d.get("artifact")) {
            let path = a["path"].as_str().context("artifact path")?;
            out.push((a["url"].as_str().context("artifact url")?.to_string(), paths::libraries().join(path), a["sha1"].as_str().map(String::from)));
        } else if let Some(base) = l["url"].as_str() {
            let rel = maven_path(name)?;
            out.push((format!("{}{rel}", if base.ends_with('/') { base.to_string() } else { format!("{base}/") }), paths::libraries().join(&rel), l["sha1"].as_str().map(String::from)));
        }
    }
    Ok(())
}

/// Every library of a (possibly merged) profile, as local paths, in classpath order.
pub fn classpath_libs(profile: &Value) -> Result<Vec<PathBuf>> {
    let mut jobs = vec![];
    library_jobs(&profile["libraries"], &mut jobs)?;
    Ok(jobs.into_iter().map(|j| j.1).collect())
}

pub fn install(name: &str, mc: &str, fabric: bool, memory_mb: u64) -> Result<Instance> {
    paths::safe_name(name)?;
    let vj = vanilla_json(mc)?;
    let java_major = vj["javaVersion"]["majorVersion"].as_u64().unwrap_or(21);
    // Fabric profile
    let (version_id, loader) = if fabric {
        let list = http::get_json(&format!("https://meta.fabricmc.net/v2/versions/loader/{mc}"))?;
        let l = list.as_array().and_then(|a| a.iter().find(|x| x["loader"]["stable"] == true).or(a.first())).with_context(|| format!("Fabric has no loader for {mc}"))?;
        let lv = l["loader"]["version"].as_str().context("loader version")?;
        let id = format!("fabric-loader-{lv}-{mc}");
        let p = paths::versions().join(&id).join(format!("{id}.json"));
        if !p.exists() {
            let prof = http::get_json(&format!("https://meta.fabricmc.net/v2/versions/loader/{mc}/{lv}/profile/json"))?;
            fs::create_dir_all(p.parent().unwrap())?;
            fs::write(&p, serde_json::to_string_pretty(&prof)?)?;
        }
        (id, "fabric")
    } else { (mc.to_string(), "vanilla") };

    // client jar
    let c = &vj["downloads"]["client"];
    http::download(c["url"].as_str().context("client url")?, &paths::versions().join(mc).join(format!("{mc}.jar")), c["sha1"].as_str())?;
    // libraries (vanilla + loader)
    let mut jobs = vec![];
    library_jobs(&vj["libraries"], &mut jobs)?;
    if fabric { library_jobs(&version_json(&version_id)?["libraries"], &mut jobs)?; }
    println!("libraries: {}", jobs.len());
    http::parallel(&jobs, 8, |(u, p, h)| http::download(u, p, h.as_deref()))?;
    // assets
    let ai = &vj["assetIndex"];
    let idx_path = paths::assets().join("indexes").join(format!("{}.json", ai["id"].as_str().context("asset id")?));
    http::download(ai["url"].as_str().context("asset url")?, &idx_path, ai["sha1"].as_str())?;
    let idx: Value = serde_json::from_slice(&fs::read(&idx_path)?)?;
    let objs: Vec<(String, String)> = idx["objects"].as_object().context("objects")?.values().filter_map(|o| o["hash"].as_str().map(|h| (h.to_string(), h[..2].to_string()))).collect();
    println!("assets: {}", objs.len());
    http::parallel(&objs, 24, |(h, d)| http::download(&format!("https://resources.download.minecraft.net/{d}/{h}"), &paths::assets().join("objects").join(d).join(h), Some(h)))?;
    // java
    ensure_java(java_major)?;
    let inst = Instance { name: name.into(), mc: mc.into(), loader: loader.into(), version_id, java_major, memory_mb };
    inst.save()?;
    Ok(inst)
}

/// A Java of the needed major version: one already in the store, else a system one that matches, else Temurin from Adoptium.
pub fn ensure_java(major: u64) -> Result<PathBuf> {
    let own = paths::java_dir().join(major.to_string()).join("bin/java");
    if own.exists() { return Ok(own); }
    for cand in ["/usr/lib/jvm/default-java/bin/java", "/usr/bin/java"] {
        if let Ok(o) = std::process::Command::new(cand).arg("-version").output() {
            let s = String::from_utf8_lossy(&o.stderr);
            if s.contains(&format!("\"{major}.")) || s.contains(&format!("\"{major}\"")) { return Ok(PathBuf::from(cand)); }
        }
    }
    println!("downloading Java {major} (Temurin)...");
    let url = format!("https://api.adoptium.net/v3/binary/latest/{major}/ga/linux/x64/jdk/hotspot/normal/eclipse");
    let r = http::client().get(&url).send()?;
    if !r.status().is_success() { bail!("no Java {major} available (HTTP {})", r.status()); }
    let bytes = r.bytes()?;
    let dest = paths::java_dir().join(major.to_string());
    fs::create_dir_all(&dest)?;
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(&bytes[..]));
    for e in ar.entries()? {
        let mut e = e?;
        let rel: PathBuf = e.path()?.components().skip(1).collect();
        if rel.as_os_str().is_empty() || rel.components().any(|c| matches!(c, std::path::Component::ParentDir)) { continue; }
        let t = dest.join(&rel);
        if let Some(p) = t.parent() { fs::create_dir_all(p)?; }
        e.unpack(&t)?;
    }
    if !own.exists() { bail!("the Java download did not contain bin/java"); }
    Ok(own)
}
