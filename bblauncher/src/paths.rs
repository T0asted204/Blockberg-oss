//! Where everything lives. Shared downloads (libraries, assets, versions, java) are kept once;
//! every instance has its own game directory, so two accounts never share mods, configs or logins.
use std::path::PathBuf;

pub fn root() -> PathBuf {
    if let Ok(p) = std::env::var("BBLAUNCHER_HOME") { return PathBuf::from(p); }
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".local/share/bblauncher")
}
pub fn libraries() -> PathBuf { root().join("libraries") }
pub fn assets() -> PathBuf { root().join("assets") }
pub fn versions() -> PathBuf { root().join("versions") }
pub fn java_dir() -> PathBuf { root().join("java") }
pub fn accounts() -> PathBuf { root().join("accounts") }
pub fn instances() -> PathBuf { root().join("instances") }
pub fn instance(name: &str) -> PathBuf { instances().join(name) }

/// Names become folder names: keep them to a safe alphabet so nothing can escape the data dir.
pub fn safe_name(n: &str) -> anyhow::Result<&str> {
    if !n.is_empty() && n.len() <= 48 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') && !n.starts_with('.') { Ok(n) }
    else { anyhow::bail!("'{n}' is not a valid name (letters, digits, - _ . only)") }
}
