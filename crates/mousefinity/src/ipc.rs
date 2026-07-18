//! Local IPC between the `mousefinity send` CLI and the running daemon, so
//! file transfers reuse the daemon's identity and connections. Loopback TCP
//! with a random token; the token file lives in the user-only config dir.

use std::io::{BufRead, BufReader, Write as _};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};
use tokio::net::TcpListener;
use tracing::{debug, info};

use crate::config;
use crate::net::Net;

#[derive(Serialize, Deserialize)]
struct IpcInfo {
    port: u16,
    token: String,
}

#[derive(Serialize, Deserialize)]
struct Request {
    token: String,
    cmd: String,
    #[serde(default)]
    peer: String,
    #[serde(default)]
    path: String,
}

#[derive(Serialize, Deserialize)]
struct Response {
    ok: bool,
    message: String,
}

fn info_path() -> Result<PathBuf> {
    Ok(config::config_dir()?.join("ipc.json"))
}

pub async fn serve(net: Arc<Net>) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let token = {
        use rand::Rng;
        let mut b = [0u8; 24];
        rand::rng().fill_bytes(&mut b);
        data_encoding::HEXLOWER.encode(&b)
    };
    let info = IpcInfo {
        port,
        token: token.clone(),
    };
    std::fs::write(info_path()?, serde_json::to_vec(&info)?)?;
    info!("ipc listening on 127.0.0.1:{port}");

    loop {
        let (stream, _) = listener.accept().await?;
        let net = net.clone();
        let token = token.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, net, &token).await {
                debug!("ipc request failed: {e:#}");
            }
        });
    }
}

async fn handle(stream: tokio::net::TcpStream, net: Arc<Net>, token: &str) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = TokioBufReader::new(read_half).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(());
    };
    let req: Request = serde_json::from_str(&line).context("bad request")?;
    let resp = if req.token != token {
        Response {
            ok: false,
            message: "bad token".into(),
        }
    } else if req.cmd == "send" {
        match net.send_file(&req.peer, std::path::Path::new(&req.path)).await {
            Ok(m) => Response {
                ok: true,
                message: m,
            },
            Err(e) => Response {
                ok: false,
                message: format!("{e:#}"),
            },
        }
    } else if req.cmd == "reload" {
        match net.reload() {
            Ok(m) => Response {
                ok: true,
                message: m,
            },
            Err(e) => Response {
                ok: false,
                message: format!("{e:#}"),
            },
        }
    } else {
        Response {
            ok: false,
            message: format!("unknown command `{}`", req.cmd),
        }
    };
    let mut out = serde_json::to_vec(&resp)?;
    out.push(b'\n');
    write_half.write_all(&out).await?;
    Ok(())
}

/// Send one request to the local daemon; the token is filled in from the
/// info file the daemon wrote at startup.
fn client_request(cmd: &str, peer: &str, path: &str) -> Result<Response> {
    let raw = std::fs::read(info_path()?)
        .context("cannot read ipc info — is `mousefinity run` active on this machine?")?;
    let info: IpcInfo = serde_json::from_slice(&raw).context("corrupt ipc info")?;
    let req = Request {
        token: info.token,
        cmd: cmd.to_string(),
        peer: peer.to_string(),
        path: path.to_string(),
    };
    let stream = std::net::TcpStream::connect(("127.0.0.1", info.port))
        .context("daemon not reachable — is `mousefinity run` active?")?;
    let mut w = stream.try_clone()?;
    let mut line = serde_json::to_vec(&req)?;
    line.push(b'\n');
    w.write_all(&line)?;
    let mut reader = BufReader::new(stream);
    let mut resp_line = String::new();
    reader.read_line(&mut resp_line)?;
    serde_json::from_str(resp_line.trim()).context("daemon returned an unreadable response")
}

/// True if a daemon's IPC endpoint answers on this machine.
pub fn daemon_reachable() -> bool {
    (|| -> Result<bool> {
        let raw = std::fs::read(info_path()?)?;
        let info: IpcInfo = serde_json::from_slice(&raw)?;
        Ok(std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], info.port)),
            std::time::Duration::from_millis(300),
        )
        .is_ok())
    })()
    .unwrap_or(false)
}

/// Tell a running daemon to re-read its config. Returns its status message.
pub fn client_reload() -> Result<String> {
    let resp = client_request("reload", "", "")?;
    if resp.ok {
        Ok(resp.message)
    } else {
        anyhow::bail!("{}", resp.message)
    }
}

/// CLI side: ask the running daemon to send files. Blocking/synchronous.
pub fn client_send(peer: &str, files: &[PathBuf]) -> Result<()> {
    for f in files {
        let abs =
            std::fs::canonicalize(f).with_context(|| format!("no such file: {}", f.display()))?;
        let resp = client_request("send", peer, &abs.to_string_lossy())?;
        if resp.ok {
            println!("{}", resp.message);
        } else {
            bail!("sending {} failed: {}", f.display(), resp.message);
        }
    }
    Ok(())
}
