//! Microsoft → Xbox Live → XSTS → Minecraft login (device-code flow, so no password ever touches this program).
//! The account file holds the refresh token and the Minecraft token; it is created with mode 600 and never printed.
use crate::{http, paths};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{fs, io::Write, os::unix::fs::OpenOptionsExt, time::{Duration, SystemTime, UNIX_EPOCH}};

/// The public client id of the official Minecraft launcher; every community launcher signs in with it.
const CLIENT_ID: &str = "00000000402b5328";
const SCOPE: &str = "service::user.auth.xboxlive.com::MBI_SSL";

#[derive(Serialize, Deserialize, Clone)]
pub struct Account {
    pub label: String,
    pub name: String,        // in-game name
    pub uuid: String,        // profile id, no dashes
    pub refresh_token: String,
    pub mc_token: String,
    pub mc_expires: u64,     // unix seconds
}

fn now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() }

fn path(label: &str) -> Result<std::path::PathBuf> { Ok(paths::accounts().join(format!("{}.json", paths::safe_name(label)?))) }

pub fn save(a: &Account) -> Result<()> {
    let p = path(&a.label)?;
    fs::create_dir_all(p.parent().unwrap())?;
    let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&p)?;
    f.write_all(serde_json::to_string_pretty(a)?.as_bytes())?;
    Ok(())
}
pub fn load(label: &str) -> Result<Account> {
    let p = path(label)?;
    serde_json::from_slice(&fs::read(&p).with_context(|| format!("no account '{label}' (run: bblauncher login {label})"))?).context("account file is damaged")
}
pub fn list() -> Vec<(String, String)> {
    let mut v = vec![];
    if let Ok(rd) = fs::read_dir(paths::accounts()) {
        for e in rd.flatten() {
            if let Ok(b) = fs::read(e.path()) { if let Ok(a) = serde_json::from_slice::<Account>(&b) { v.push((a.label, a.name)); } }
        }
    }
    v.sort();
    v
}
pub fn remove(label: &str) -> Result<()> { fs::remove_file(path(label)?).context("no such account") }

fn post_form(url: &str, form: &[(&str, &str)]) -> Result<(u16, Value)> {
    let r = http::client().post(url).form(form).send()?;
    let st = r.status().as_u16();
    Ok((st, r.json().unwrap_or(Value::Null)))
}
fn post_json(url: &str, body: &Value) -> Result<(u16, Value)> {
    let r = http::client().post(url).header("Accept", "application/json").json(body).send()?;
    let st = r.status().as_u16();
    Ok((st, r.json().unwrap_or(Value::Null)))
}

/// Interactive sign-in: prints a code to enter at microsoft.com/link, waits, then builds the Minecraft session.
pub fn login(label: &str) -> Result<Account> {
    path(label)?;
    let (st, d) = post_form("https://login.live.com/oauth20_connect.srf", &[("client_id", CLIENT_ID), ("scope", SCOPE), ("response_type", "device_code")])?;
    if st != 200 { bail!("could not start the sign-in (HTTP {st}): {d}"); }
    let (code, dev, uri) = (d["user_code"].as_str().unwrap_or(""), d["device_code"].as_str().unwrap_or(""), d["verification_uri"].as_str().unwrap_or("https://www.microsoft.com/link"));
    let interval = d["interval"].as_u64().unwrap_or(5).max(2);
    let expires = d["expires_in"].as_u64().unwrap_or(900);
    println!("\nOpen  {uri}  and enter the code:  {code}\nSign in with the Microsoft account that owns Minecraft. Waiting (up to {} min)...", expires / 60);
    let start = now();
    let ms = loop {
        if now() - start > expires { bail!("the code expired; run the login again"); }
        std::thread::sleep(Duration::from_secs(interval));
        let (_, t) = post_form("https://login.live.com/oauth20_token.srf", &[("client_id", CLIENT_ID), ("device_code", dev), ("grant_type", "urn:ietf:params:oauth:grant-type:device_code")])?;
        if t["access_token"].is_string() { break t; }
        match t["error"].as_str() { Some("authorization_pending") | Some("slow_down") | None => continue, Some(e) => bail!("sign-in failed: {e}") }
    };
    let acc = session_from_ms(label, &ms)?;
    save(&acc)?;
    Ok(acc)
}

fn session_from_ms(label: &str, ms: &Value) -> Result<Account> {
    let access = ms["access_token"].as_str().context("no Microsoft access token")?;
    let refresh = ms["refresh_token"].as_str().context("no refresh token")?.to_string();
    // Xbox Live
    let (st, x) = post_json("https://user.auth.xboxlive.com/user/authenticate", &json!({
        "Properties": {"AuthMethod": "RPS", "SiteName": "user.auth.xboxlive.com", "RpsTicket": format!("t={access}")},
        "RelyingParty": "http://auth.xboxlive.com", "TokenType": "JWT"}))?;
    if st != 200 { bail!("Xbox Live sign-in failed (HTTP {st})"); }
    let xbl = x["Token"].as_str().context("no Xbox token")?;
    // XSTS for Minecraft
    let (st, s) = post_json("https://xsts.auth.xboxlive.com/xsts/authorize", &json!({
        "Properties": {"SandboxId": "RETAIL", "UserTokens": [xbl]},
        "RelyingParty": "rp://api.minecraftservices.com/", "TokenType": "JWT"}))?;
    if st != 200 {
        let why = match s["XErr"].as_u64() {
            Some(2148916233) => "this Microsoft account has no Xbox profile yet (sign in once at xbox.com)",
            Some(2148916238) => "this is a child account: add it to a Microsoft family first",
            Some(2148916235) => "Xbox Live is not available in this country",
            _ => "unknown reason",
        };
        bail!("Xbox authorisation refused (HTTP {st}): {why}");
    }
    let (xsts, uhs) = (s["Token"].as_str().context("no XSTS token")?, s["DisplayClaims"]["xui"][0]["uhs"].as_str().context("no uhs")?);
    // Minecraft
    let (st, m) = post_json("https://api.minecraftservices.com/authentication/login_with_xbox", &json!({"identityToken": format!("XBL3.0 x={uhs};{xsts}")}))?;
    if st != 200 { bail!("Minecraft sign-in refused (HTTP {st})"); }
    let tok = m["access_token"].as_str().context("no Minecraft token")?.to_string();
    let exp = now() + m["expires_in"].as_u64().unwrap_or(86400);
    let r = http::client().get("https://api.minecraftservices.com/minecraft/profile").bearer_auth(&tok).send()?;
    if r.status().as_u16() == 404 { bail!("this account does not own Minecraft: Java Edition"); }
    let p: Value = r.json()?;
    Ok(Account { label: label.into(), name: p["name"].as_str().context("no profile name")?.into(), uuid: p["id"].as_str().context("no profile id")?.into(), refresh_token: refresh, mc_token: tok, mc_expires: exp })
}

/// A session that is good for at least five more minutes: refreshes (and saves) when needed.
pub fn fresh(label: &str) -> Result<Account> {
    let a = load(label)?;
    if a.mc_expires > now() + 300 { return Ok(a); }
    let (_, t) = post_form("https://login.live.com/oauth20_token.srf", &[("client_id", CLIENT_ID), ("refresh_token", &a.refresh_token), ("grant_type", "refresh_token"), ("scope", SCOPE)])?;
    if !t["access_token"].is_string() { bail!("the saved login expired: run `bblauncher login {label}` again"); }
    let n = session_from_ms(label, &t)?;
    save(&n)?;
    Ok(n)
}
