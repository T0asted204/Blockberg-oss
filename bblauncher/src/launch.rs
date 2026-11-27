use crate::{auth, install::{self, Instance}, paths};
use anyhow::{Context, Result};
use serde_json::Value;
use std::{collections::HashMap, fs, path::PathBuf, process::Command};

pub struct Opts { pub server: Option<String>, pub headless: bool, pub dry: bool, pub offline: bool, pub size: Option<(u32, u32)>, pub extra_jvm: Vec<String> }

fn flatten(args: Option<&Value>, features: &[&str]) -> Vec<String> {
    let mut out = vec![];
    for a in args.and_then(|a| a.as_array()).into_iter().flatten() {
        match a {
            Value::String(s) => out.push(s.clone()),
            Value::Object(_) => {
                if !install::rules_allow(a.get("rules"), features) { continue; }
                match &a["value"] { Value::String(s) => out.push(s.clone()), Value::Array(v) => out.extend(v.iter().filter_map(|x| x.as_str().map(String::from))), _ => {} }
            }
            _ => {}
        }
    }
    out
}

fn ga(name: &str) -> String { name.split(':').take(2).collect::<Vec<_>>().join(":") }

/// The loader profile layered over the vanilla one: loader libraries first, vanilla after, one jar per group:artifact.
fn merged(inst: &Instance) -> Result<Value> {
    let child = install::version_json(&inst.version_id)?;
    if inst.version_id == inst.mc { return Ok(child); }
    let parent = install::version_json(&inst.mc)?;
    let mut libs = vec![];
    let mut seen = std::collections::HashSet::new();
    for l in child["libraries"].as_array().into_iter().flatten().chain(parent["libraries"].as_array().into_iter().flatten()) {
        let name = l["name"].as_str().unwrap_or("");
        // keep native-classifier jars distinct from the plain jar
        let key = if name.split(':').count() > 3 { name.to_string() } else { ga(name) };
        if seen.insert(key) { libs.push(l.clone()); }
    }
    let mut m = parent.clone();
    m["libraries"] = Value::Array(libs);
    m["mainClass"] = child["mainClass"].clone();
    let cat = |k: &str| -> Value {
        let mut v = parent["arguments"][k].as_array().cloned().unwrap_or_default();
        v.extend(child["arguments"][k].as_array().cloned().unwrap_or_default());
        Value::Array(v)
    };
    m["arguments"] = serde_json::json!({"jvm": cat("jvm"), "game": cat("game")});
    Ok(m)
}

pub fn build(inst: &Instance, account: &str, o: &Opts) -> Result<(Command, Vec<String>)> {
    let offline_acc = || auth::Account { label: account.into(), name: "Offline".into(), uuid: "00000000000000000000000000000000".into(), refresh_token: String::new(), mc_token: "0".into(), mc_expires: 0 };
    let acc = if o.offline { offline_acc() } else { match auth::fresh(account) {
        Ok(a) => a,
        // a dry run only prints the command: it works without a login, with a stand-in player
        Err(_) if o.dry => auth::Account { label: account.into(), name: "DryRun".into(), uuid: "00000000000000000000000000000000".into(), refresh_token: String::new(), mc_token: "dry-run-token".into(), mc_expires: 0 },
        Err(e) => return Err(e),
    } };
    let prof = merged(inst)?;
    let java = install::ensure_java(inst.java_major)?;
    let mut cp: Vec<PathBuf> = install::classpath_libs(&prof)?;
    if o.headless { cp.insert(0, headless_agent()?); }   // the stub classes must be visible to the game's class loader too, not only to the agent
    cp.push(paths::versions().join(&inst.mc).join(format!("{}.jar", inst.mc)));
    for p in &cp { if !p.exists() { anyhow::bail!("missing file {} (run `bblauncher install` again)", p.display()); } }
    let sep = ":";
    let cp_s = cp.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>().join(sep);
    let game_dir = inst.game_dir();
    fs::create_dir_all(&game_dir)?;
    let natives = inst.dir().join("natives");
    fs::create_dir_all(&natives)?;

    let mut vars: HashMap<&str, String> = HashMap::new();
    vars.insert("auth_player_name", acc.name.clone());
    vars.insert("version_name", inst.version_id.clone());
    vars.insert("game_directory", game_dir.to_string_lossy().into());
    vars.insert("assets_root", paths::assets().to_string_lossy().into());
    vars.insert("assets_index_name", prof["assets"].as_str().unwrap_or(&inst.mc).into());
    vars.insert("auth_uuid", acc.uuid.clone());
    vars.insert("auth_access_token", acc.mc_token.clone());
    vars.insert("clientid", String::new());
    vars.insert("auth_xuid", String::new());
    vars.insert("user_type", "msa".into());
    vars.insert("version_type", prof["type"].as_str().unwrap_or("release").into());
    vars.insert("natives_directory", natives.to_string_lossy().into());
    vars.insert("launcher_name", "bblauncher".into());
    vars.insert("launcher_version", "0.1".into());
    vars.insert("classpath", cp_s.clone());
    vars.insert("classpath_separator", sep.into());
    vars.insert("library_directory", paths::libraries().to_string_lossy().into());
    let mut feats = vec![];
    if let Some((w, h)) = o.size { vars.insert("resolution_width", w.to_string()); vars.insert("resolution_height", h.to_string()); feats.push("has_custom_resolution"); }
    let sub = |s: &str| -> String { let mut r = s.to_string(); for (k, v) in &vars { r = r.replace(&format!("${{{k}}}"), v); } r };

    let mut jvm: Vec<String> = flatten(prof["arguments"].get("jvm"), &feats).iter().map(|s| sub(s)).collect();
    if !jvm.iter().any(|a| a == "-cp" || a == "-classpath") { jvm.push("-cp".into()); jvm.push(cp_s.clone()); }
    let mut game: Vec<String> = flatten(prof["arguments"].get("game"), &feats).iter().map(|s| sub(s)).collect();
    if let Some(s) = &o.server {
        // quick play only switches on when it also has a log path to write to
        let qp = game_dir.join("quickPlay"); fs::create_dir_all(&qp)?;
        game.push("--quickPlayPath".into()); game.push(qp.join("log.json").to_string_lossy().into());
        game.push("--quickPlayMultiplayer".into()); game.push(s.clone());
    }

    let mut args = vec![format!("-Xms{}M", (inst.memory_mb / 2).max(512)), format!("-Xmx{}M", inst.memory_mb)];
    // low-cost JVM: one GC thread set, no parallel collector overhead
    args.push("-XX:+UseSerialGC".into());
    args.extend(o.extra_jvm.iter().cloned());
    if o.headless {
        let agent = headless_agent()?;
        args.push(format!("-javaagent:{}", agent.display()));
    }
    args.extend(jvm);
    args.push(prof["mainClass"].as_str().context("mainClass")?.to_string());
    args.extend(game);

    let mut c = Command::new(&java);
    c.args(&args).current_dir(&game_dir);
    // for the dry run: the same arguments with the token hidden
    let shown: Vec<String> = args.iter().map(|a| if a == &acc.mc_token { "<token hidden>".to_string() } else { a.clone() }).collect();
    Ok((c, std::iter::once(java.to_string_lossy().to_string()).chain(shown).collect()))
}

pub fn headless_agent() -> Result<PathBuf> {
    let p = std::env::var("BBLAUNCHER_LWJGL_AGENT").map(PathBuf::from).unwrap_or_else(|_| paths::root().join("headless/lwjgl-agent.jar"));
    if p.exists() { Ok(p) } else { anyhow::bail!("headless mode needs the LWJGL agent jar at {} (see `bblauncher headless-build`)", p.display()) }
}
