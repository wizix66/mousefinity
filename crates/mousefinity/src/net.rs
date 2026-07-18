//! P2P networking over iroh: authenticated QUIC with NAT hole-punching and
//! relay fallback. Peers are trusted purely by their public key (EndpointId);
//! anything else is refused at accept time.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
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
    peers_by_name: RwLock<HashMap<String, EndpointId>>,
    names_by_id: RwLock<HashMap<EndpointId, String>>,
    /// Static socket addresses per peer, tried alongside discovery results.
    static_addrs: RwLock<HashMap<String, Vec<std::net::SocketAddr>>>,
    /// Peer names that currently have a live dial task.
    dialing: Mutex<HashSet<String>>,
    engine_tx: UnboundedSender<EngineIn>,
    epoch: AtomicU64,
    downloads: PathBuf,
}

type PeerMaps = (
    HashMap<String, EndpointId>,
    HashMap<EndpointId, String>,
    HashMap<String, Vec<std::net::SocketAddr>>,
);

fn parse_peers(cfg: &Config) -> Result<PeerMaps> {
    let mut by_name = HashMap::new();
    let mut by_id = HashMap::new();
    let mut statics = HashMap::new();
    for (name, peer) in &cfg.peers {
        let id: EndpointId = peer
            .id
            .parse()
            .with_context(|| format!("peer `{name}` has an invalid id"))?;
        by_name.insert(name.clone(), id);
        by_id.insert(id, name.clone());
        if !peer.addrs.is_empty() {
            let mut socks = Vec::new();
            for a in &peer.addrs {
                socks.push(a.parse::<std::net::SocketAddr>().with_context(|| {
                    format!("peer `{name}`: `{a}` is not a valid ip:port address")
                })?);
            }
            statics.insert(name.clone(), socks);
        }
    }
    Ok((by_name, by_id, statics))
}

impl Net {
    pub async fn bind(
        cfg: &Config,
        secret: [u8; 32],
        my_screen: (u32, u32),
        engine_tx: UnboundedSender<EngineIn>,
    ) -> Result<Arc<Self>> {
        let (peers_by_name, names_by_id, static_addrs) = parse_peers(cfg)?;
        let relay_off = cfg.network.relay.as_deref() == Some("off");
        let mut builder = if relay_off {
            // Direct connections only: no relay servers are ever contacted.
            Endpoint::builder(presets::N0DisableRelay)
        } else {
            Endpoint::builder(presets::N0)
        };
        builder = builder
            .secret_key(SecretKey::from_bytes(&secret))
            .alpns(vec![ALPN_CONTROL.to_vec(), ALPN_FILE.to_vec()])
            // Advertise and resolve peers directly on the local network:
            // same-LAN hosts find each other with zero internet access.
            .address_lookup(iroh_mdns_address_lookup::MdnsAddressLookup::builder())
            // Resolve discovery records over HTTPS as well as DNS, for
            // networks that filter TXT lookups (common on corporate nets).
            .address_lookup(iroh::address_lookup::PkarrResolver::n0_dns());
        // Trust the OS certificate store in addition to the built-in webpki
        // roots, so relay connections survive corporate TLS-inspection
        // proxies whose root CA is installed on this machine. (The full
        // platform verifier is NOT used: on Windows it rejects the relays'
        // trailing-dot hostnames with NotValidForName.)
        let native = rustls_native_certs::load_native_certs();
        if !native.certs.is_empty() {
            builder = builder.ca_tls_config(
                iroh::tls::CaTlsConfig::default().with_extra_roots(native.certs),
            );
        }
        if let Some(port) = cfg.network.port.filter(|p| *p != 0) {
            builder = builder
                .bind_addr(std::net::SocketAddr::from((
                    std::net::Ipv4Addr::UNSPECIFIED,
                    port,
                )))
                .context("invalid fixed port")?;
            info!("listening on fixed udp port {port}");
        }
        let endpoint = builder
            .bind()
            .await
            .context("failed to bind iroh endpoint")?;
        Ok(Arc::new(Self {
            endpoint,
            my_name: cfg.name.clone(),
            my_screen,
            peers_by_name: RwLock::new(peers_by_name),
            names_by_id: RwLock::new(names_by_id),
            static_addrs: RwLock::new(static_addrs),
            dialing: Mutex::new(HashSet::new()),
            engine_tx,
            epoch: AtomicU64::new(0),
            downloads: cfg.downloads_dir(),
        }))
    }

    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// Re-read the config: refresh the trusted-peer maps, start dialing any
    /// newly added peers, and hand the (possibly newer) layout to the engine.
    pub fn reload(self: &Arc<Self>) -> Result<String> {
        let cfg = crate::config::load()?;
        let (by_name, by_id, statics) = parse_peers(&cfg)?;
        let n = by_name.len();
        *self.peers_by_name.write().unwrap() = by_name;
        *self.names_by_id.write().unwrap() = by_id;
        *self.static_addrs.write().unwrap() = statics;
        self.spawn_dials();
        let _ = self.engine_tx.send(EngineIn::SetLayout {
            rev: cfg.layout_rev,
            layout: cfg.layout(),
        });
        Ok(format!("reloaded: {n} peer(s), layout rev {}", cfg.layout_rev))
    }

    /// Start a dial task for every trusted peer that we are responsible for
    /// dialing and that doesn't have one yet. Between any pair exactly one
    /// side dials (the lexicographically smaller id), so a pair never races
    /// to create duplicate links.
    fn spawn_dials(self: &Arc<Self>) {
        let my_id = self.endpoint.id();
        let snapshot: Vec<(String, EndpointId)> = self
            .peers_by_name
            .read()
            .unwrap()
            .iter()
            .map(|(n, i)| (n.clone(), *i))
            .collect();
        for (name, id) in snapshot {
            if my_id.as_bytes() < id.as_bytes()
                && self.dialing.lock().unwrap().insert(name.clone())
            {
                let net = self.clone();
                tokio::spawn(async move { net.dial_loop(name).await });
            }
        }
    }

    /// Run the dial loops and the accept loop forever.
    pub async fn run(self: Arc<Self>) {
        self.spawn_dials();
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
                let name = net.names_by_id.read().unwrap().get(&id).cloned();
                let Some(name) = name else {
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

    /// Dial target for a peer: its id plus any statically configured
    /// addresses. iroh races these against discovery results and the relay
    /// path, so static LAN/VPN routes win whenever they are reachable.
    fn dial_target(&self, name: &str, id: EndpointId) -> EndpointAddr {
        let mut addr = EndpointAddr::new(id);
        if let Some(socks) = self.static_addrs.read().unwrap().get(name) {
            for s in socks {
                addr = addr.with_ip_addr(*s);
            }
        }
        addr
    }

    async fn dial_loop(self: Arc<Self>, name: String) {
        loop {
            // Re-resolve each round so a reload can retarget or retire us.
            let id = self.peers_by_name.read().unwrap().get(&name).copied();
            let Some(id) = id else { break };
            if self.endpoint.id().as_bytes() >= id.as_bytes() {
                break; // after a re-pair the other side dials now
            }
            let target = self.dial_target(&name, id);
            match self.endpoint.connect(target, ALPN_CONTROL).await {
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
        self.dialing.lock().unwrap().remove(&name);
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
        let id = self
            .peers_by_name
            .read()
            .unwrap()
            .get(peer)
            .copied()
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
        let target = self.dial_target(peer, id);
        let conn = self.endpoint.connect(target, ALPN_FILE).await?;
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
