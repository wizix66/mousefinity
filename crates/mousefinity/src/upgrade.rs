//! `mousefinity upgrade`: look for a newer GitHub release and replace this
//! binary with it.
//!
//! The download is checked against the `SHA256SUMS` file published alongside
//! the release assets, or failing that the digest GitHub reports for the
//! asset. TLS already authenticates github.com, but a self updater executes
//! whatever it fetches, so the extra hop from "the server said so" to "the
//! bytes hash to what was published" is worth the few lines. A release
//! offering neither is refused rather than trusted.

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

const REPO: &str = "wizix66/mousefinity";
const AGENT: &str = concat!("mousefinity/", env!("CARGO_PKG_VERSION"));
const CHECKSUMS: &str = "SHA256SUMS";
/// Refuse anything implausible for a stripped release binary.
const MAX_ASSET: u64 = 256 * 1024 * 1024;

#[cfg(windows)]
const BIN_NAME: &str = "mousefinity.exe";
#[cfg(not(windows))]
const BIN_NAME: &str = "mousefinity";

/// Asset naming comes from `.github/workflows/release.yml`; these are the
/// targets it builds for.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const TARGET: &str = "aarch64-apple-darwin";
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const TARGET: &str = "x86_64-apple-darwin";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const TARGET: &str = "x86_64-unknown-linux-gnu";
/// arm64 servers — Ampere, Graviton, and the arm64 Raspberry Pi images.
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const TARGET: &str = "aarch64-unknown-linux-gnu";
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
const TARGET: &str = "x86_64-pc-windows-msvc";
/// Built for something the release workflow does not publish: `upgrade` can
/// still report what is available, but has nothing to install.
#[cfg(not(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "macos", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64"),
    all(target_os = "windows", target_arch = "x86_64"),
)))]
const TARGET: &str = "";

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    /// GitHub's own digest of the uploaded bytes, as `sha256:<hex>`. Used when
    /// a release predates the workflow that publishes [`CHECKSUMS`].
    #[serde(default)]
    digest: Option<String>,
}

impl Asset {
    fn api_digest(&self) -> Option<String> {
        let d = self.digest.as_deref()?;
        d.strip_prefix("sha256:").map(str::to_ascii_lowercase)
    }
}

fn header(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)?
        .to_str()
        .ok()
        .map(str::to_string)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Explain a 403 that is really GitHub's rate limit.
///
/// Unauthenticated callers get 60 requests an hour **per IP address**, not per
/// machine, so anyone sharing an office NAT can use it up without this
/// computer having done anything. The bare "403 Forbidden" suggests the
/// release is unreachable or the machine is blocked, when in fact waiting is
/// all that is needed — so say which it is, and for how long.
fn rate_limited(
    status: u16,
    remaining: Option<&str>,
    reset_unix: Option<u64>,
    now: u64,
) -> Option<String> {
    if status != 403 && status != 429 {
        return None;
    }
    // A 403 without this header is a real refusal, not a quota.
    if remaining? != "0" {
        return None;
    }
    let wait = reset_unix
        .map(|r| r.saturating_sub(now).div_ceil(60).max(1))
        .map(|m| format!("about {m} minute(s)"))
        .unwrap_or_else(|| "up to an hour".to_string());
    Some(format!(
        "github's api rate limit is used up for this network, so the release \
         list cannot be read. it is counted per public IP address, so a shared \
         office or VPN connection can exhaust it without this machine doing \
         anything. it frees up in {wait} — nothing is wrong with the install. \
         to update sooner, download from \
         https://github.com/{REPO}/releases and replace the binary by hand"
    ))
}

/// `(major, minor, patch)`, ignoring any pre-release or build suffix.
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

pub fn run(check_only: bool, assume_yes: bool) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(check_only, assume_yes))
}

fn client() -> Result<reqwest::Client> {
    // iroh installs a provider when an endpoint is bound, but `upgrade` never
    // binds one; install ours so rustls has a default either way.
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .user_agent(AGENT)
        .timeout(Duration::from_secs(180))
        .build()
        .context("cannot build the http client")
}

async fn run_async(check_only: bool, assume_yes: bool) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let http = client()?;
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = http
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("cannot reach github.com — check your internet connection")?;
    if let Some(explanation) = rate_limited(
        resp.status().as_u16(),
        header(&resp, "x-ratelimit-remaining").as_deref(),
        header(&resp, "x-ratelimit-reset")
            .and_then(|v| v.parse().ok()),
        now_unix(),
    ) {
        bail!("{explanation}");
    }
    let release: Release = resp
        .error_for_status()
        .context("github rejected the release lookup")?
        .json()
        .await
        .context("could not parse github's release response")?;

    let latest = &release.tag_name;
    println!("installed: v{current}");
    println!("latest:    {latest}");

    let (Some(have), Some(want)) = (parse_version(current), parse_version(latest)) else {
        bail!("cannot compare version `{current}` with release tag `{latest}`");
    };
    if want <= have {
        println!("\nalready up to date.");
        return Ok(());
    }
    if !release.html_url.is_empty() {
        println!("\nrelease notes: {}", release.html_url);
    }
    if check_only {
        println!("a newer release is available — run `mousefinity upgrade` to install it.");
        return Ok(());
    }
    if TARGET.is_empty() {
        bail!(
            "no prebuilt binary is published for this platform; \
             build from source or grab an asset from {}",
            release.html_url
        );
    }

    // The relay server ships under its own prefix in the same release.
    let asset = release
        .assets
        .iter()
        .find(|a| {
            a.name.starts_with("mousefinity-")
                && !a.name.starts_with("mousefinity-relay-")
                && a.name.contains(TARGET)
        })
        .with_context(|| format!("release {latest} has no asset for {TARGET}"))?;
    if asset.size > MAX_ASSET {
        bail!(
            "{} is {} bytes, which is far larger than a release binary should be",
            asset.name,
            asset.size
        );
    }

    if !assume_yes && !crate::confirm(&format!("install {} over this binary?", asset.name))? {
        println!("cancelled.");
        return Ok(());
    }

    // Prefer the checksum file the release workflow signs off on; fall back to
    // the digest the API reports for the asset. Both describe the bytes GitHub
    // holds, so neither survives a compromised account — but either one turns
    // a silent swap in transit into a hard failure.
    let sums = release.assets.iter().find(|a| a.name == CHECKSUMS);
    let (expected, source) = match sums {
        Some(sums) => {
            let text = String::from_utf8(fetch(&http, &sums.browser_download_url, 1 << 20).await?)
                .context("checksum file is not valid utf-8")?;
            let digest = expected_digest(&text, &asset.name)
                .with_context(|| format!("{CHECKSUMS} has no entry for {}", asset.name))?;
            (digest, CHECKSUMS)
        }
        None => (
            asset.api_digest().with_context(|| {
                format!(
                    "release {latest} publishes neither {CHECKSUMS} nor an asset digest, so \
                     the download cannot be verified; install it manually from {}",
                    release.html_url
                )
            })?,
            "github asset digest",
        ),
    };

    println!("downloading {}…", asset.name);
    let archive = fetch(&http, &asset.browser_download_url, MAX_ASSET).await?;
    let actual = sha256_hex(&archive);
    if actual != expected {
        bail!(
            "checksum mismatch for {}\n  expected {expected}\n  got      {actual}\n\
             refusing to install — report this, it should never happen",
            asset.name
        );
    }
    println!("checksum verified ({source}).");

    let binary = extract_binary(&archive)?;
    let path = replace_running_binary(&binary)?;
    println!("upgraded to {latest} at {}", path.display());
    if crate::ipc::daemon_reachable() {
        println!("a daemon is running the old build — restart it to pick this up.");
    }
    Ok(())
}

async fn fetch(http: &reqwest::Client, url: &str, cap: u64) -> Result<Vec<u8>> {
    let resp = http
        .get(url)
        .send()
        .await
        .with_context(|| format!("cannot download {url}"))?
        .error_for_status()
        .with_context(|| format!("download failed for {url}"))?;
    if let Some(len) = resp.content_length() {
        if len > cap {
            bail!("{url} is {len} bytes, over the {cap} byte limit");
        }
    }
    let bytes = resp.bytes().await.context("download was interrupted")?;
    if bytes.len() as u64 > cap {
        bail!("{url} exceeded the {cap} byte limit mid-download");
    }
    Ok(bytes.to_vec())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Pull one entry out of a `shasum -a 256` listing (`<hex>  <name>`).
fn expected_digest(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hex, name) = line.split_once(char::is_whitespace)?;
        (name.trim().trim_start_matches('*') == asset).then(|| hex.trim().to_ascii_lowercase())
    })
}

/// Pull the binary out of a release asset.
///
/// Dispatch is on the archive's magic bytes rather than the host platform:
/// the format is a property of the file, and deciding it this way keeps the
/// zip reader exercised by tests on every platform, not just Windows.
fn extract_binary(archive: &[u8]) -> Result<Vec<u8>> {
    if archive.starts_with(b"PK\x03\x04") {
        extract_from_zip(archive)
    } else if archive.starts_with(&[0x1f, 0x8b]) {
        extract_from_tar_gz(archive)
    } else {
        bail!("downloaded asset is neither a gzip tarball nor a zip archive")
    }
}

fn extract_from_tar_gz(archive: &[u8]) -> Result<Vec<u8>> {
    let decoder = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(decoder);
    for entry in tar.entries().context("archive is not a readable tar")? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if path.file_name().and_then(|s| s.to_str()) == Some(BIN_NAME) {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            return Ok(buf);
        }
    }
    bail!("archive did not contain a `{BIN_NAME}` binary")
}

fn extract_from_zip(archive: &[u8]) -> Result<Vec<u8>> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive))
        .context("archive is not a readable zip")?;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i)?;
        let leaf = file
            .name()
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or_default()
            .to_string();
        if leaf == BIN_NAME {
            let mut buf = Vec::new();
            file.read_to_end(&mut buf)?;
            return Ok(buf);
        }
    }
    bail!("archive did not contain a `{BIN_NAME}` binary")
}

fn replace_running_binary(new_bytes: &[u8]) -> Result<PathBuf> {
    let current = std::env::current_exe().context("cannot locate this binary")?;
    // Follow symlinks so we replace the real file rather than the link.
    let current = std::fs::canonicalize(&current).unwrap_or(current);
    install_at(&current, new_bytes)?;
    Ok(current)
}

/// Swap `new_bytes` in as `target`.
///
/// Stage, move the old file aside, rename the new one in, then drop the old
/// one. Windows needs the move-aside because it refuses to replace a running
/// image; unix does not, but running the same sequence everywhere means the
/// path that upgrades a live binary is the one the tests exercise, rather than
/// a `cfg(windows)` branch nothing off Windows ever executes. Deleting the old
/// file is best-effort for the same reason: on Windows it stays locked until
/// the process exits, and [`clean_stale`] sweeps it on the next run.
///
/// Every step is a rename, so a failure leaves the working binary in place —
/// for the binary you are currently running that is the difference between
/// "try again" and "reinstall by hand".
fn install_at(target: &std::path::Path, new_bytes: &[u8]) -> Result<()> {
    let dir = target
        .parent()
        .context("this binary has no parent directory")?
        .to_path_buf();
    let staged = dir.join(format!(".{BIN_NAME}.new"));
    let old = dir.join(format!(".{BIN_NAME}.old"));

    std::fs::write(&staged, new_bytes).with_context(|| {
        format!(
            "cannot write {} — is {} writable? (a system-wide install may need sudo)",
            staged.display(),
            dir.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)) {
            let _ = std::fs::remove_file(&staged);
            return Err(e).context("cannot mark the new binary executable");
        }
    }

    let _ = std::fs::remove_file(&old);
    swap_in(&staged, target, &old)
}

/// Move `staged` onto `target`, keeping `old` as the undo step.
///
/// Split out from [`install_at`] so the rollback branch is reachable from a
/// test: it is the one path that could leave a user with no binary at all, and
/// it is otherwise only taken when the filesystem misbehaves mid-upgrade.
fn swap_in(staged: &std::path::Path, target: &std::path::Path, old: &std::path::Path) -> Result<()> {
    let moved_aside = target.exists();
    if moved_aside {
        if let Err(e) = std::fs::rename(target, old) {
            let _ = std::fs::remove_file(staged);
            return Err(e).with_context(|| format!("cannot move {} aside", target.display()));
        }
    }
    if let Err(e) = std::fs::rename(staged, target) {
        // Put the original back before giving up, so the failure costs the
        // user nothing.
        if moved_aside {
            let _ = std::fs::rename(old, target);
        }
        let _ = std::fs::remove_file(staged);
        return Err(e)
            .with_context(|| format!("cannot move the new binary into {}", target.display()));
    }
    let _ = std::fs::remove_file(old);
    Ok(())
}

/// Sweep the previous binary if an upgrade could not delete it in place —
/// which on Windows is every upgrade, since the running image stays locked.
pub fn clean_stale() {
    if let Ok(current) = std::env::current_exe() {
        if let Some(dir) = current.parent() {
            let _ = std::fs::remove_file(dir.join(format!(".{BIN_NAME}.old")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The failure that is not a failure: waiting fixes it, and the bare 403
    /// says nothing about that.
    #[test]
    fn a_spent_rate_limit_is_explained_rather_than_shown_as_forbidden() {
        let now = 1_000_000;
        let msg = rate_limited(403, Some("0"), Some(now + 610), now)
            .expect("a spent quota must be recognised");
        assert!(msg.contains("rate limit"), "{msg}");
        assert!(msg.contains("11 minute"), "must say how long to wait: {msg}");
        assert!(msg.contains("per public IP"), "must explain the shared-address trap");
        // 429 is the other shape GitHub uses for this.
        assert!(rate_limited(429, Some("0"), None, now).is_some());
    }

    #[test]
    fn a_genuine_refusal_is_not_mistaken_for_a_rate_limit() {
        let now = 1_000_000;
        // Quota left, so a 403 means something else and must surface as-is.
        assert!(rate_limited(403, Some("57"), Some(now + 60), now).is_none());
        // No quota header at all: also a real refusal.
        assert!(rate_limited(403, None, None, now).is_none());
        // Success is never a rate limit.
        assert!(rate_limited(200, Some("0"), None, now).is_none());
    }

    #[test]
    fn versions_compare_numerically() {
        // String ordering would put v0.10.0 before v0.9.0.
        assert!(parse_version("v0.10.0") > parse_version("v0.9.0"));
        assert_eq!(parse_version("0.3.0"), parse_version("v0.3.0"));
        assert_eq!(parse_version("v1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(parse_version("v2"), Some((2, 0, 0)));
        assert_eq!(parse_version("not-a-version"), None);
    }

    #[test]
    fn digest_lookup_matches_only_the_named_asset() {
        let sums = "\
aaa1  mousefinity-v0.4.0-aarch64-apple-darwin.tar.gz
bbb2  mousefinity-relay-v0.4.0-aarch64-apple-darwin.tar.gz
ccc3 *mousefinity-v0.4.0-x86_64-pc-windows-msvc.zip";
        assert_eq!(
            expected_digest(sums, "mousefinity-v0.4.0-aarch64-apple-darwin.tar.gz").as_deref(),
            Some("aaa1")
        );
        // The binary-mode `*` marker must not defeat the match.
        assert_eq!(
            expected_digest(sums, "mousefinity-v0.4.0-x86_64-pc-windows-msvc.zip").as_deref(),
            Some("ccc3")
        );
        assert_eq!(
            expected_digest(sums, "mousefinity-v0.4.0-linux.tar.gz"),
            None
        );
    }

    #[test]
    fn api_digest_is_parsed_and_normalised() {
        let asset = |d: Option<&str>| Asset {
            name: "a".into(),
            browser_download_url: String::new(),
            size: 0,
            digest: d.map(str::to_string),
        };
        assert_eq!(
            asset(Some("sha256:AABB")).api_digest().as_deref(),
            Some("aabb")
        );
        // An algorithm we cannot check must not be silently accepted.
        assert_eq!(asset(Some("sha512:aabb")).api_digest(), None);
        assert_eq!(asset(None).api_digest(), None);
    }

    /// Mirrors the layout `release.yml` produces: a versioned directory
    /// holding the binary alongside the docs.
    #[cfg(not(windows))]
    #[test]
    fn extracts_the_binary_from_the_release_layout() {
        let dir = "mousefinity-v9.9.9-aarch64-apple-darwin";
        let mut builder = tar::Builder::new(Vec::new());
        for (name, body) in [
            ("README.md", &b"not the binary"[..]),
            ("mousefinity", &b"\x7fELF payload"[..]),
            ("LICENSE-MIT", &b"also not the binary"[..]),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, format!("{dir}/{name}"), body)
                .unwrap();
        }
        let tarball = builder.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tarball).unwrap();

        let extracted = extract_binary(&gz.finish().unwrap()).unwrap();
        assert_eq!(extracted, b"\x7fELF payload");
    }

    /// The Windows asset shape, checked from whatever platform runs the tests.
    #[test]
    fn extracts_the_binary_from_a_zip_asset() {
        use std::io::Cursor;
        let dir = "mousefinity-v9.9.9-x86_64-pc-windows-msvc";
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        writer.start_file(format!("{dir}/README.md"), opts).unwrap();
        writer.write_all(b"not the binary").unwrap();
        writer
            .start_file(format!("{dir}/{BIN_NAME}"), opts)
            .unwrap();
        writer.write_all(b"MZ payload").unwrap();
        let archive = writer.finish().unwrap().into_inner();

        assert!(archive.starts_with(b"PK\x03\x04"), "expected a zip");
        assert_eq!(extract_binary(&archive).unwrap(), b"MZ payload");
    }

    #[test]
    fn an_unrecognised_archive_is_rejected() {
        assert!(extract_binary(b"just some bytes").is_err());
    }

    #[test]
    fn install_replaces_the_target_and_leaves_no_staging_file() {
        let dir = std::env::temp_dir().join(format!("mousefinity-install-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join(BIN_NAME);
        std::fs::write(&target, b"the old build").unwrap();

        install_at(&target, b"the new build").unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"the new build");
        assert!(
            !dir.join(format!(".{BIN_NAME}.new")).exists(),
            "staging file must not survive a successful install"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "the replacement must be executable");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failed_install_leaves_the_existing_binary_untouched() {
        let dir = std::env::temp_dir().join(format!("mousefinity-fail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join(BIN_NAME);
        std::fs::write(&target, b"the old build").unwrap();
        // A directory where the staging file wants to go makes the write fail.
        std::fs::create_dir_all(dir.join(format!(".{BIN_NAME}.new"))).unwrap();

        assert!(install_at(&target, b"the new build").is_err());
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"the old build",
            "a failed upgrade must not damage the installed binary"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The move-aside step is what Windows needs, and it is the one step that
    /// could strand a user with no binary at all. A staging file that is not
    /// there forces the swap to fail after the original has been moved.
    #[test]
    fn a_failed_swap_restores_the_original() {
        let dir = std::env::temp_dir().join(format!("mousefinity-swap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join(BIN_NAME);
        let old = dir.join(format!(".{BIN_NAME}.old"));
        std::fs::write(&target, b"the only copy").unwrap();

        assert!(swap_in(&dir.join("never-written"), &target, &old).is_err());
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"the only copy",
            "the original must be renamed back after a failed swap"
        );
        assert!(!old.exists(), "the undo copy must not be left lying around");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn installing_where_nothing_exists_yet_still_works() {
        let dir = std::env::temp_dir().join(format!("mousefinity-fresh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join(BIN_NAME);

        install_at(&target, b"first install").unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"first install");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(not(windows))]
    #[test]
    fn archive_without_a_binary_is_an_error() {
        let mut builder = tar::Builder::new(Vec::new());
        let body = &b"docs only"[..];
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "pkg/README.md", body)
            .unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&builder.into_inner().unwrap()).unwrap();

        assert!(extract_binary(&gz.finish().unwrap()).is_err());
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
