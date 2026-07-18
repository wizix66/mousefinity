//! `mousefinity report`: write a diagnostic bundle for a bug report.
//!
//! The bundle is only ever written to a local file and printed for the user to
//! read before they attach it anywhere — nothing is uploaded, and there is no
//! telemetry. That makes redaction a hard requirement rather than a courtesy:
//! the mesh token is a shared credential and the identity key must never leave
//! the machine, so neither is ever read into the bundle.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::{config, doctor};

const REDACTED: &str = "<redacted by `mousefinity report`>";

pub fn run(output: Option<PathBuf>) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(output))
}

async fn run_async(output: Option<PathBuf>) -> Result<()> {
    let mut out = String::new();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    writeln!(out, "# mousefinity diagnostic report")?;
    writeln!(out)?;
    writeln!(out, "generated: unix {stamp}")?;
    writeln!(out, "version:   {}", env!("CARGO_PKG_VERSION"))?;
    writeln!(out, "target:    {}", target_triple())?;
    writeln!(
        out,
        "os:        {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    )?;
    writeln!(
        out,
        "screen:    {}",
        match rdev::display_size() {
            Ok((w, h)) => format!("{w}x{h}"),
            Err(e) => format!("undetectable ({e:?})"),
        }
    )?;
    writeln!(
        out,
        "daemon:    {}",
        if crate::ipc::daemon_reachable() {
            "running on this machine"
        } else {
            "not running"
        }
    )?;
    writeln!(
        out,
        "RUST_LOG:  {}",
        std::env::var("RUST_LOG").unwrap_or_else(|_| "(unset)".into())
    )?;

    writeln!(out)?;
    writeln!(out, "## config")?;
    writeln!(out)?;
    match config::config_path() {
        Ok(p) => writeln!(out, "path: {}", p.display())?,
        Err(e) => writeln!(out, "path: unavailable ({e:#})")?,
    }
    match config::key_path() {
        // Presence and permissions are diagnostic; the contents never are.
        Ok(p) => writeln!(
            out,
            "identity key: {} ({})",
            if p.exists() { "present" } else { "MISSING" },
            key_permissions(&p)
        )?,
        Err(e) => writeln!(out, "identity key: unavailable ({e:#})")?,
    }
    writeln!(out)?;
    writeln!(out, "```toml")?;
    match redacted_config() {
        Ok(toml) => write!(out, "{toml}")?,
        Err(e) => writeln!(out, "# could not read config: {e:#}")?,
    }
    writeln!(out, "```")?;

    writeln!(out)?;
    writeln!(out, "## doctor")?;
    writeln!(out)?;
    println!("running network diagnostics — this takes up to ~30s…");
    let mut report = doctor::Report::new(false);
    let doctor_result = doctor::collect(&mut report).await;
    writeln!(out, "```")?;
    writeln!(out, "{}", report.text())?;
    if let Err(e) = &doctor_result {
        writeln!(out, "\ndiagnostics stopped early: {e:#}")?;
    }
    writeln!(out, "```")?;

    let path = match output {
        Some(p) => p,
        None => default_path(stamp),
    };
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).ok();
        }
    }
    std::fs::write(&path, &out).with_context(|| format!("cannot write {}", path.display()))?;

    println!();
    println!("wrote {}", path.display());
    println!(
        "  {} check(s) failed; {} lines captured",
        report.failures(),
        report.text().lines().count()
    );
    println!();
    println!("the mesh token and identity key are excluded, but peer names, pairing ids,");
    println!("your public IP and relay choice are included — read it before you share it.");
    println!("attach it to an issue at https://github.com/wizix66/mousefinity/issues");
    Ok(())
}

fn default_path(stamp: u64) -> PathBuf {
    let name = format!("mousefinity-report-{stamp}.md");
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(name)
}

/// The config as stored, minus the shared secret that grants mesh membership.
fn redacted_config() -> Result<String> {
    let mut cfg = config::load()?;
    if cfg.mesh_secret.is_some() {
        cfg.mesh_secret = Some(REDACTED.to_string());
    }
    Ok(toml::to_string_pretty(&cfg)?)
}

#[cfg(unix)]
fn key_permissions(path: &std::path::Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(m) => {
            let mode = m.permissions().mode() & 0o777;
            let warn = if mode & 0o077 != 0 {
                " — WARNING: readable by other users"
            } else {
                ""
            };
            format!("mode {mode:04o}{warn}")
        }
        Err(e) => format!("cannot stat: {e}"),
    }
}

#[cfg(not(unix))]
fn key_permissions(path: &std::path::Path) -> String {
    match std::fs::metadata(path) {
        Ok(_) => "profile directory ACL".to_string(),
        Err(e) => format!("cannot stat: {e}"),
    }
}

fn target_triple() -> String {
    format!(
        "{}-{}",
        std::env::consts::ARCH,
        if cfg!(windows) {
            "pc-windows"
        } else if cfg!(target_os = "macos") {
            "apple-darwin"
        } else {
            std::env::consts::OS
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mesh_secret_never_reaches_the_bundle() {
        let secret = "de1e7ed".repeat(8);
        let cfg = config::Config {
            name: "host".into(),
            screen: None,
            downloads: None,
            network: Default::default(),
            mesh_secret: Some(secret.clone()),
            peers: Default::default(),
            layout: Default::default(),
            layout_rev: 0,
        };
        let mut redacted = cfg.clone();
        redacted.mesh_secret = Some(REDACTED.to_string());
        let rendered = toml::to_string_pretty(&redacted).unwrap();
        assert!(!rendered.contains(&secret));
        assert!(rendered.contains(REDACTED));
        // Guard against the field being dropped from Config: if it stops
        // serializing entirely this test would pass vacuously.
        assert!(toml::to_string_pretty(&cfg).unwrap().contains(&secret));
    }
}
