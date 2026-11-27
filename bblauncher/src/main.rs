mod auth; mod http; mod join; mod install; mod launch; mod mods; mod paths;
use anyhow::{bail, Result};

const HELP: &str = "BBlauncher: Minecraft launcher for Blockberg agents

  bblauncher login <account>                         sign in with Microsoft (device code); saved under that label
  bblauncher accounts | bblauncher logout <account>
  bblauncher install <instance> <mc-version> [--vanilla] [--mem <MB>]     Fabric by default
  bblauncher instances
  bblauncher mods add <instance> <modrinth-slug>...   | mods list <instance> | mods remove <instance> <file>
  bblauncher launch <instance> <account> [--server host] [--headless] [--size WxH] [--dry] [--offline] [--after-join <chat>] [--skyblock] [--jvm <arg>]
  bblauncher where                                   print the data folder

Everything lives in ~/.local/share/bblauncher (override with BBLAUNCHER_HOME). Each instance has its own game folder.";

fn main() {
    if let Err(e) = run() { eprintln!("error: {e:#}"); std::process::exit(1); }
}

fn run() -> Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let s = |i: usize| a.get(i).map(|x| x.as_str());
    match s(0) {
        Some("login") => { let acc = auth::login(s(1).ok_or_else(|| anyhow::anyhow!("usage: bblauncher login <account>"))?)?; println!("signed in as {} ({})", acc.name, acc.label); }
        Some("accounts") => { for (l, n) in auth::list() { println!("{l:<16} {n}"); } }
        Some("logout") => { auth::remove(s(1).ok_or_else(|| anyhow::anyhow!("usage: bblauncher logout <account>"))?)?; println!("removed"); }
        Some("install") => {
            let (Some(name), Some(mc)) = (s(1), s(2)) else { bail!("usage: bblauncher install <instance> <mc-version> [--vanilla] [--mem MB]") };
            let mem = a.iter().position(|x| x == "--mem").and_then(|i| a.get(i + 1)).and_then(|x| x.parse().ok()).unwrap_or(3072);
            let i = install::install(name, mc, !a.iter().any(|x| x == "--vanilla"), mem)?;
            println!("instance '{}' ready: {} {} (Java {})", i.name, i.loader, i.mc, i.java_major);
        }
        Some("instances") => { for i in install::list_instances() { println!("{:<16} {} {}  {} MB", i.name, i.loader, i.mc, i.memory_mb); } }
        Some("mods") => {
            let inst = install::Instance::load(s(2).ok_or_else(|| anyhow::anyhow!("usage: bblauncher mods add|list|remove <instance> ..."))?)?;
            match s(1) {
                Some("add") => { if a.len() < 4 { bail!("usage: bblauncher mods add <instance> <slug>..."); } for slug in &a[3..] { println!("{slug}:"); mods::add(&inst, slug)?; } }
                Some("list") => { for m in mods::list(&inst) { println!("{m}"); } }
                Some("remove") => { mods::remove(&inst, s(3).ok_or_else(|| anyhow::anyhow!("usage: bblauncher mods remove <instance> <file>"))?)?; println!("removed"); }
                _ => bail!("mods: add | list | remove"),
            }
        }
        Some("launch") => {
            let (Some(name), Some(acc)) = (s(1), s(2)) else { bail!("usage: bblauncher launch <instance> <account> [options]") };
            let inst = install::Instance::load(name)?;
            let pos = |f: &str| a.iter().position(|x| x == f).and_then(|i| a.get(i + 1)).cloned();
            let size = pos("--size").and_then(|v| { let (w, h) = v.split_once('x')?; Some((w.parse().ok()?, h.parse().ok()?)) });
            let o = launch::Opts { server: pos("--server"), headless: a.iter().any(|x| x == "--headless"), dry: a.iter().any(|x| x == "--dry"), offline: a.iter().any(|x| x == "--offline"), size, extra_jvm: a.windows(2).filter(|w| w[0] == "--jvm").map(|w| w[1].clone()).collect() };
            let (mut cmd, shown) = launch::build(&inst, acc, &o)?;
            if o.dry { println!("{}", shown.join(" ")); return Ok(()); }
            // a second agent on the same machine: its own agent port, and its in-game helper port (agent port + 2)
            if let Some(port) = pos("--agent-port").and_then(|p| p.parse::<u16>().ok()) { cmd.env("BB_AGENT_PORT", port.to_string()).env("BB_FAST_PORT", (port + 2).to_string()); }
            // --after-join "<chat>": once the agent answers on its port (it only starts after the world loads), wait a moment and send that chat line once
            if let Some(text) = pos("--after-join") {
                let port: u16 = pos("--agent-port").and_then(|p| p.parse().ok()).unwrap_or(7777);
                std::thread::spawn(move || {
                    use std::{io::Write, net::TcpStream, time::Duration};
                    for _ in 0..600 {
                        std::thread::sleep(Duration::from_secs(2));
                        if let Ok(mut c) = TcpStream::connect(("127.0.0.1", port)) {
                            std::thread::sleep(Duration::from_secs(6));   // let the lobby finish loading
                            // "a;;b" sends a, waits for the server hop, then sends b (Hypixel wants /lobby before /play skyblock)
                            for (i, part) in text.split(";;").enumerate() {
                                if i > 0 { std::thread::sleep(Duration::from_secs(10)); if let Ok(n) = TcpStream::connect(("127.0.0.1", port)) { c = n; } }
                                let line = serde_json::json!({"cmd": "say", "text": part.trim()}).to_string() + "\n";
                                let _ = c.write_all(line.as_bytes());
                                std::thread::sleep(Duration::from_secs(2));
                                eprintln!("bblauncher: sent '{}' through the agent", part.trim());
                            }
                            return;
                        }
                    }
                });
            }
            // --skyblock: after the agent is up, go to SkyBlock (via the lobby only when Hypixel asks for it)
            if a.iter().any(|x| x == "--skyblock") {
                let port: u16 = pos("--agent-port").and_then(|p| p.parse().ok()).unwrap_or(7777);
                let w = std::sync::Arc::new(join::Watch::default());
                cmd.stdout(std::process::Stdio::piped());
                let mut child = cmd.spawn()?;
                join::pump(child.stdout.take().expect("piped"), w.clone(), child.id());
                join::skyblock(port, w);
                let st = child.wait()?;
                std::process::exit(st.code().unwrap_or(1));
            }
            let st = cmd.status()?;
            std::process::exit(st.code().unwrap_or(1));
        }
        Some("where") => println!("{}", paths::root().display()),
        _ => println!("{HELP}"),
    }
    Ok(())
}
