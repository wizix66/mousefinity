//! On-disk configuration: identity key, peer registry, screen layout.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use mousefinity_proto::{Layout, Neighbors};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    /// The peer's iroh endpoint id (its public key), as printed by
    /// `mousefinity id` on that machine.
    pub id: String,
    /// Optional static addresses (`"ip:port"`) tried in addition to anything
    /// discovery finds. Lets routed LANs and firewall-allowlisted setups
    /// connect with no discovery infrastructure at all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addrs: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Network {
    /// Fixed UDP port to listen on (IPv4, all interfaces). Makes inbound
    /// firewall rules and peers' static `addrs` entries possible.
    /// Absent or 0 = ephemeral port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Set to "off" to never use public relay servers: LAN/VPN-only
    /// operation, direct connections only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// This host's name, referenced from the layout.
    pub name: String,
    /// Optional override of the auto-detected primary screen size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<(u32, u32)>,
    /// Where received files land. Defaults to `<Downloads>/mousefinity`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downloads: Option<PathBuf>,
    /// Network tuning (fixed port, relay policy).
    #[serde(default, skip_serializing_if = "is_default_network")]
    pub network: Network,
    /// Trusted peers by name. Only these may connect.
    #[serde(default)]
    pub peers: BTreeMap<String, Peer>,
    /// Screen arrangement: for each host name, which host lies past each edge.
    #[serde(default)]
    pub layout: BTreeMap<String, Neighbors>,
    /// Revision of `layout` (unix ms of the last edit); drives layout sync.
    #[serde(default)]
    pub layout_rev: u64,
}

impl Config {
    pub fn layout(&self) -> Layout {
        Layout(self.layout.clone())
    }

    pub fn downloads_dir(&self) -> PathBuf {
        self.downloads.clone().unwrap_or_else(|| {
            dirs::download_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("mousefinity")
        })
    }
}

fn is_default_network(n: &Network) -> bool {
    n.port.is_none() && n.relay.is_none()
}

pub fn config_dir() -> Result<PathBuf> {
    // Override for tests and for running several instances on one machine.
    if let Ok(dir) = std::env::var("MOUSEFINITY_CONFIG_DIR") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    Ok(dirs::config_dir()
        .context("no config directory on this platform")?
        .join("mousefinity"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

pub fn key_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("secret.key"))
}

pub fn load() -> Result<Config> {
    let path = config_path()?;
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {} — run `mousefinity init` first", path.display()))?;
    let cfg: Config = toml::from_str(&raw).context("invalid config file")?;
    if cfg.name.is_empty() {
        bail!("config `name` must not be empty");
    }
    Ok(cfg)
}

/// Milliseconds since the Unix epoch; used as the layout revision.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Persist a layout adopted from a peer, unless the on-disk copy is newer.
pub fn save_synced_layout(rev: u64, layout: &Layout) -> Result<()> {
    let mut cfg = load()?;
    if cfg.layout_rev >= rev {
        return Ok(());
    }
    cfg.layout_rev = rev;
    cfg.layout = layout.0.clone();
    save(&cfg)
}

pub fn save(cfg: &Config) -> Result<()> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)?;
    let raw = toml::to_string_pretty(cfg)?;
    std::fs::write(config_path()?, raw)?;
    Ok(())
}

/// Load the identity key, generating and persisting one on first use.
pub fn load_or_create_secret() -> Result<[u8; 32]> {
    let path = key_path()?;
    match std::fs::read(&path) {
        Ok(bytes) => {
            let arr: [u8; 32] = bytes
                .try_into()
                .map_err(|_| anyhow::anyhow!("corrupt key file {}", path.display()))?;
            Ok(arr)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(config_dir()?)?;
            let mut key = [0u8; 32];
            use rand::Rng;
            rand::rng().fill_bytes(&mut key);
            std::fs::write(&path, key)?;
            restrict_permissions(&path);
            Ok(key)
        }
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) {
    // On Windows the profile directory ACL already restricts access to the user.
}
