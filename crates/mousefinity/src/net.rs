//! P2P networking over iroh: authenticated QUIC with NAT hole-punching and
//! relay fallback. Peers are trusted purely by their public key (EndpointId);
//! anything else is refused at accept time.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointId, SecretKey};
use mousefinity_proto::{
    read_frame, write_frame, FileOffer, Msg, ALPN_CONTROL, ALPN_FILE, PROTO_VERSION,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::{self, UnboundedSender};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::engine::EngineIn;

pub struct Net {
    pub endpoint: Endpoint,
    my_name: String,
    my_screen: (u32, u32),
    peers_by_name: HashMap<String, EndpointId>,
    names_by_id: HashMap<EndpointId, String>,
    engine_tx: UnboundedSender<EngineIn>,
    epoch: AtomicU64,
    downloads: PathBuf,
}

impl Net {
    pub async fn bind(
        cfg: &Config,
        secret: [u8; 32],
        my_screen: (u32, u32),
        engine_tx: UnboundedSender<EngineIn>,
    ) -> Result<Arc<Self>> {
        let mut peers_by_name = HashMap::new();
        let mut names_by_id = HashMap::new();
        for (name, peer) in &cfg.peers {
            let id: EndpointId = peer
                .id
                .parse()
                .with_context(|| format!("peer `{name}` has an invalid id"))?;
            peers_by_name.insert(name.clone(), id);
            names_by_id.insert(id, name.clone());
        }
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(SecretKey::from_bytes(&secret))
            .alpns(vec![ALPN_CONTROL.to_vec(), ALPN_FILE.to_vec()])
            // Besides the internet-wide relay/DNS discovery from the N0
            // preset, advertise and resolve peers directly on the local
            // network so LAN setups work even with filtered DNS.
            .address_lookup(iroh_mdns_address_lookup::MdnsAddressLookup::builder())
            .bind()
            .await
            .context("failed to bind iroh endpoint")?;
        Ok(Arc::new(Self {
            endpoint,
            my_name: cfg.name.clone(),
            my_screen,
            peers_by_name,
            names_by_id,
            engine_tx,
            epoch: AtomicU64::new(0),
            downloads: cfg.downloads_dir(),
        }))
    }

    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// Spawn dial loops and run the accept loop forever.
    pub async fn run(self: Arc<Self>) {
        // Between any pair exactly one side dials (the lexicographically
        // smaller id), so a pair never races to create duplicate links.
        let my_id = self.endpoint.id();
        for (name, id) in self.peers_by_name.clone() {
            if my_id.as_bytes() < id.as_bytes() {
                let net = self.clone();
                tokio::spawn(async move { net.dial_loop(name, id).await });
            }
        }
        while let Some(incoming) = self.endpoint.accept().await {
            let net = self.clone();
            tokio::spawn(async move {
                let conn = match incoming.await {
                    Ok(c) => c,
                    Err(e) => {
                        debug!("incoming connection failed: {e}");
                        return;
                    }
                };
                let id = conn.remote_id();
                let Some(name) = net.names_by_id.get(&id).cloned() else {
                    warn!("rejecting connection from unknown endpoint {id}");
                    conn.close(1u32.into(), b"not paired");
                    return;
                };
                match conn.alpn() {
                    a if a == ALPN_CONTROL => {
                        if let Err(e) = net.handle_control(name.clone(), conn, false).await {
                            debug!("control link with {name} ended: {e:#}");
                        }
                    }
                    a if a == ALPN_FILE => {
                        if let Err(e) = net.handle_file_recv(name.clone(), conn).await {
                            warn!("file receive from {name} failed: {e:#}");
                        }
                    }
                    other => {
                        debug!("unknown alpn {:?}", String::from_utf8_lossy(other));
                        conn.close(2u32.into(), b"bad alpn");
                    }
                }
            });
        }
    }

    async fn dial_loop(self: Arc<Self>, name: String, id: EndpointId) {
        loop {
            match self.endpoint.connect(id, ALPN_CONTROL).await {
                Ok(conn) => {
                    info!("connected to {name}");
                    if let Err(e) = self.handle_control(name.clone(), conn, true).await {
                        debug!("control link with {name} ended: {e:#}");
                    }
                    info!("link with {name} lost; retrying");
                }
                Err(e) => {
                    debug!("cannot reach {name}: {e}");
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    /// Run one control link: hello exchange, then pump messages both ways
    /// until the connection dies.
    async fn handle_control(
        &self,
        name: String,
        conn: iroh::endpoint::Connection,
        dialer: bool,
    ) -> Result<()> {
        let (mut send, mut recv) = if dialer {
            conn.open_bi().await?
        } else {
            conn.accept_bi().await?
        };
        write_frame(
            &mut send,
            &Msg::Hello {
                version: PROTO_VERSION,
                name: self.my_name.clone(),
                screen: self.my_screen,
            },
        )
        .await?;
        let hello: Msg = read_frame(&mut recv).await?;
        let Msg::Hello {
            version, screen, ..
        } = hello
        else {
            bail!("peer did not start with Hello");
        };
        if version != PROTO_VERSION {
            bail!("protocol version mismatch: ours {PROTO_VERSION}, theirs {version}");
        }

        let epoch = self.epoch.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
        let _ = self.engine_tx.send(EngineIn::PeerUp {
            name: name.clone(),
            screen,
            epoch,
            tx,
        });

        let write_loop = async {
            while let Some(msg) = rx.recv().await {
                write_frame(&mut send, &msg).await?;
            }
            Ok::<_, anyhow::Error>(())
        };
        let engine_tx = self.engine_tx.clone();
        let peer_name = name.clone();
        let read_loop = async {
            loop {
                let msg: Msg = read_frame(&mut recv).await?;
                let _ = engine_tx.send(EngineIn::PeerMsg {
                    name: peer_name.clone(),
                    msg,
                });
            }
            #[allow(unreachable_code)]
            Ok::<_, anyhow::Error>(())
        };
        let result = tokio::select! {
            r = write_loop => r,
            r = read_loop => r,
        };
        let _ = self.engine_tx.send(EngineIn::PeerDown { name, epoch });
        result
    }

    // ---- file transfer ----

    pub async fn send_file(&self, peer: &str, path: &Path) -> Result<String> {
        let id = *self
            .peers_by_name
            .get(peer)
            .with_context(|| format!("unknown peer `{peer}`"))?;
        let meta = tokio::fs::metadata(path)
            .await
            .with_context(|| format!("cannot read {}", path.display()))?;
        if !meta.is_file() {
            bail!("{} is not a regular file", path.display());
        }
        let name = path
            .file_name()
            .context("path has no file name")?
            .to_string_lossy()
            .into_owned();
        let conn = self.endpoint.connect(id, ALPN_FILE).await?;
        let (mut send, mut recv) = conn.open_bi().await?;
        write_frame(
            &mut send,
            &FileOffer {
                name: name.clone(),
                size: meta.len(),
            },
        )
        .await?;
        let mut file = tokio::fs::File::open(path).await?;
        let sent = tokio::io::copy(&mut file, &mut send).await?;
        send.finish()?;
        // Wait for the receiver's ack so we know it hit their disk.
        let _ack: u8 = {
            let mut b = [0u8; 1];
            recv.read_exact(&mut b).await.context("no receipt ack")?;
            b[0]
        };
        conn.close(0u32.into(), b"done");
        Ok(format!("sent {name} ({sent} bytes) to {peer}"))
    }

    async fn handle_file_recv(&self, name: String, conn: iroh::endpoint::Connection) -> Result<()> {
        let (mut send, mut recv) = conn.accept_bi().await?;
        let offer: FileOffer = read_frame(&mut recv).await?;
        // Only ever take the final path component; a peer must not be able to
        // write outside the download directory.
        let base = Path::new(&offer.name)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty() && s != ".." && s != ".")
            .unwrap_or_else(|| "received.bin".to_string());
        tokio::fs::create_dir_all(&self.downloads).await?;
        let target = unique_path(&self.downloads, &base).await;
        info!(
            "receiving {} ({} bytes) from {name} -> {}",
            offer.name,
            offer.size,
            target.display()
        );
        let mut file = tokio::fs::File::create(&target).await?;
        let written = tokio::io::copy(&mut recv, &mut file).await?;
        file.flush().await?;
        if written != offer.size {
            warn!(
                "size mismatch for {}: expected {}, got {written}",
                target.display(),
                offer.size
            );
        }
        send.write_all(&[1u8]).await?;
        send.finish()?;
        // Hold the connection open until the sender closes it after reading
        // our ack; dropping right away would race the ack's delivery.
        let _ = tokio::time::timeout(Duration::from_secs(10), conn.closed()).await;
        info!("received {}", target.display());
        Ok(())
    }
}

/// Pick a non-clobbering path in `dir` for `base` ("file.txt" -> "file (1).txt").
async fn unique_path(dir: &Path, base: &str) -> PathBuf {
    let candidate = dir.join(base);
    if !matches!(tokio::fs::try_exists(&candidate).await, Ok(true)) {
        return candidate;
    }
    let (stem, ext) = match base.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (base.to_string(), String::new()),
    };
    for i in 1..10_000 {
        let p = dir.join(format!("{stem} ({i}){ext}"));
        if !matches!(tokio::fs::try_exists(&p).await, Ok(true)) {
            return p;
        }
    }
    dir.join(format!("{stem} (overflow){ext}"))
}
