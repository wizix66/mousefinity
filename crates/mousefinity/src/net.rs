//! P2P networking over iroh: authenticated QUIC with NAT hole-punching and
//! relay fallback. Peers are trusted purely by their public key (EndpointId);
//! anything else is refused at accept time.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use mousefinity_proto::{
    read_frame, write_frame, Edge, FileOffer, JoinRequest, JoinResponse, Layout, Member, Msg,
    Neighbors, ALPN_CONTROL, ALPN_FILE, ALPN_JOIN, PROTO_VERSION,
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
    /// Self-hosted relay all peers share, when `[network] relay` is a URL.
    custom_relay: Option<iroh::RelayUrl>,
    /// Mesh token, when this host is part of a mesh.
    mesh: RwLock<Option<[u8; 32]>>,
    /// Live control-link senders by peer name, for roster broadcasts.
    /// Epoch-tagged so a stale link's teardown can't evict its successor.
    control_txs: Mutex<HashMap<String, (u64, UnboundedSender<Msg>)>>,
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

/// Rewrite every screen key and neighbour reference through `f`.
///
/// References that do not resolve are dropped: a name that means nothing
/// outside this host would only add a phantom screen on the other side.
fn map_layout(layout: &Layout, f: impl Fn(&str) -> Option<String>) -> Layout {
    let mut out = BTreeMap::new();
    for (screen, neighbors) in &layout.0 {
        let Some(key) = f(screen) else { continue };
        let mut mapped = Neighbors::default();
        for edge in [Edge::Left, Edge::Right, Edge::Up, Edge::Down] {
            if let Some(target) = neighbors.get(edge) {
                *mapped.get_mut(edge) = f(target);
            }
        }
        if !mapped.is_empty() {
            out.insert(key, mapped);
        }
    }
    Layout(out)
}

/// Build and bind the iroh endpoint per the config's network policy.
/// Returns the endpoint plus the custom relay URL, if one is configured.
pub async fn bind_endpoint(
    cfg: &Config,
    secret: [u8; 32],
) -> Result<(Endpoint, Option<iroh::RelayUrl>)> {
    let relay_setting = cfg.network.relay.as_deref();
    let mut custom_relay: Option<iroh::RelayUrl> = None;
    let mut builder = if relay_setting == Some("off") {
        // Direct connections only: no relay servers are ever contacted.
        Endpoint::builder(presets::N0DisableRelay)
    } else {
        Endpoint::builder(presets::N0)
    };
    builder = builder
        .secret_key(SecretKey::from_bytes(&secret))
        .alpns(vec![
            ALPN_CONTROL.to_vec(),
            ALPN_FILE.to_vec(),
            ALPN_JOIN.to_vec(),
        ])
        // Advertise and resolve peers directly on the local network:
        // same-LAN hosts find each other with zero internet access.
        .address_lookup(iroh_mdns_address_lookup::MdnsAddressLookup::builder())
        // Resolve discovery records over HTTPS as well as DNS, for
        // networks that filter TXT lookups (common on corporate nets).
        .address_lookup(iroh::address_lookup::PkarrResolver::n0_dns());
    if let Some(r) = relay_setting {
        if r != "off" && r != "auto" {
            // Self-hosted relay: use it exclusively, and remember it so
            // dials can target peers on it without any discovery.
            let url: iroh::RelayUrl = r
                .parse()
                .with_context(|| format!("[network] relay = \"{r}\" is not a valid url"))?;
            custom_relay = Some(url.clone());
            builder = builder.relay_mode(iroh::RelayMode::Custom(iroh::RelayMap::from(url)));
        }
    }
    // Trust the OS certificate store in addition to the built-in webpki
    // roots, so relay connections survive corporate TLS-inspection
    // proxies whose root CA is installed on this machine. (The full
    // platform verifier is NOT used: on Windows it rejects the relays'
    // trailing-dot hostnames with NotValidForName.)
    let native = rustls_native_certs::load_native_certs();
    if !native.certs.is_empty() {
        builder = builder
            .ca_tls_config(iroh::tls::CaTlsConfig::default().with_extra_roots(native.certs));
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
    Ok((endpoint, custom_relay))
}

impl Net {
    pub async fn bind(
        cfg: &Config,
        secret: [u8; 32],
        my_screen: (u32, u32),
        engine_tx: UnboundedSender<EngineIn>,
    ) -> Result<Arc<Self>> {
        let (peers_by_name, names_by_id, static_addrs) = parse_peers(cfg)?;
        let mesh = cfg.mesh_secret_bytes()?;
        let (endpoint, custom_relay) = bind_endpoint(cfg, secret).await?;
        Ok(Arc::new(Self {
            endpoint,
            my_name: cfg.name.clone(),
            my_screen,
            peers_by_name: RwLock::new(peers_by_name),
            names_by_id: RwLock::new(names_by_id),
            static_addrs: RwLock::new(static_addrs),
            custom_relay,
            mesh: RwLock::new(mesh),
            control_txs: Mutex::new(HashMap::new()),
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
        *self.mesh.write().unwrap() = cfg.mesh_secret_bytes()?;
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
                // Join handshakes are how unknown machines BECOME trusted;
                // they authenticate with the mesh token instead of the peer
                // list, so dispatch them before the trust check.
                if conn.alpn() == ALPN_JOIN {
                    if let Err(e) = net.handle_join_accept(conn).await {
                        debug!("join handshake failed: {e:#}");
                    }
                    return;
                }
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
        // With a shared self-hosted relay, peers are reachable through it
        // even when every discovery mechanism is blocked.
        if let Some(url) = &self.custom_relay {
            addr = addr.with_relay_url(url.clone());
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
        self: &Arc<Self>,
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
            tx: tx.clone(),
        });
        self.control_txs
            .lock()
            .unwrap()
            .insert(name.clone(), (epoch, tx.clone()));
        // Mesh members swap rosters on every link-up, so machines that were
        // offline when someone joined catch up as soon as they reconnect.
        if self.mesh.read().unwrap().is_some() {
            let _ = tx.send(Msg::Roster {
                members: self.roster(),
            });
        }

        let write_loop = async {
            while let Some(msg) = rx.recv().await {
                // The engine speaks local names; the wire speaks ids.
                let msg = match msg {
                    Msg::Layout { rev, layout } => match self.layout_to_wire(&layout) {
                        Some(layout) => Msg::Layout { rev, layout },
                        // Untranslatable: dropping the message is safer than
                        // gossiping an empty layout at a winning revision.
                        None => continue,
                    },
                    other => other,
                };
                write_frame(&mut send, &msg).await?;
            }
            Ok::<_, anyhow::Error>(())
        };
        let engine_tx = self.engine_tx.clone();
        let peer_name = name.clone();
        let net = self.clone();
        let read_loop = async {
            loop {
                let msg: Msg = read_frame(&mut recv).await?;
                match msg {
                    // Rosters are a trust concern; they stop at the net layer.
                    Msg::Roster { members } => {
                        if net.mesh.read().unwrap().is_some() {
                            if let Err(e) = net.apply_roster(&members) {
                                warn!("roster merge failed: {e:#}");
                            }
                        }
                    }
                    // Ids on the wire become whatever this host calls those
                    // machines, so a peer's naming never leaks into ours.
                    Msg::Layout { rev, layout } => {
                        let _ = engine_tx.send(EngineIn::PeerMsg {
                            name: peer_name.clone(),
                            msg: Msg::Layout {
                                rev,
                                layout: net.layout_from_wire(&layout),
                            },
                        });
                    }
                    msg => {
                        let _ = engine_tx.send(EngineIn::PeerMsg {
                            name: peer_name.clone(),
                            msg,
                        });
                    }
                }
            }
            #[allow(unreachable_code)]
            Ok::<_, anyhow::Error>(())
        };
        let result = tokio::select! {
            r = write_loop => r,
            r = read_loop => r,
        };
        {
            let mut txs = self.control_txs.lock().unwrap();
            if txs.get(&name).is_some_and(|(e, _)| *e == epoch) {
                txs.remove(&name);
            }
        }
        let _ = self.engine_tx.send(EngineIn::PeerDown { name, epoch });
        result
    }

    // ---- layout translation ----

    /// Local names -> endpoint ids, for a layout about to go on the wire.
    ///
    /// Ids already in the layout (machines this host never named) pass through
    /// untouched, so gossiping through a host that has not paired with every
    /// machine does not shrink the layout.
    ///
    /// Returns `None` when a non-empty layout translates to nothing. Every
    /// name being unresolvable means this host's own config is wrong, and
    /// `rev` is newer than whatever the peer holds — so sending the empty
    /// result would overwrite a perfectly good arrangement on every other
    /// machine with our local mistake. Staying quiet keeps the damage here.
    fn layout_to_wire(&self, layout: &Layout) -> Option<Layout> {
        let my_id = self.endpoint.id().to_string();
        let by_name = self.peers_by_name.read().unwrap();
        let wire = map_layout(layout, |r: &str| {
            if r == self.my_name {
                return Some(my_id.clone());
            }
            if let Some(id) = by_name.get(r) {
                return Some(id.to_string());
            }
            r.parse::<EndpointId>().ok().map(|_| r.to_string())
        });
        if layout.0.is_empty() {
            return Some(wire);
        }
        if wire.0.is_empty() {
            warn!(
                "not sharing the screen layout: none of {:?} is a paired peer on this \
                 host, so there is nothing meaningful to send. run `mousefinity doctor` \
                 — those edges cannot hop from here either",
                layout.0.keys().collect::<Vec<_>>()
            );
            return None;
        }
        if wire.0.len() < layout.0.len() {
            warn!(
                "sharing a partial layout: {} of {} screens reference machines this host \
                 has not paired with and were left out",
                layout.0.len() - wire.0.len(),
                layout.0.len()
            );
        }
        Some(wire)
    }

    /// Endpoint ids -> local names, for a layout that just arrived.
    ///
    /// An id we have no name for is kept verbatim rather than dropped; the
    /// TUI shows it abbreviated until the machine is paired and named.
    fn layout_from_wire(&self, layout: &Layout) -> Layout {
        let my_id = self.endpoint.id().to_string();
        let by_id = self.names_by_id.read().unwrap();
        map_layout(layout, |r: &str| {
            if r == my_id {
                return Some(self.my_name.clone());
            }
            // Anything that is not an id came from a host speaking an older
            // dialect of this message; its names are not ours to interpret.
            match r.parse::<EndpointId>() {
                Ok(id) => Some(by_id.get(&id).cloned().unwrap_or_else(|| r.to_string())),
                Err(_) => None,
            }
        })
    }

    // ---- mesh ----

    /// Every member this host knows about, including itself.
    fn roster(&self) -> Vec<Member> {
        let mut members = vec![Member {
            name: self.my_name.clone(),
            id: self.endpoint.id().to_string(),
        }];
        for (name, id) in self.peers_by_name.read().unwrap().iter() {
            members.push(Member {
                name: name.clone(),
                id: id.to_string(),
            });
        }
        members
    }

    /// Merge roster members: persist newcomers, trust them, start dialing
    /// them, and re-gossip the grown roster to every connected member.
    /// Convergent: re-broadcast happens only when something was new.
    fn apply_roster(self: &Arc<Self>, members: &[Member]) -> Result<()> {
        let my_id_hex = self.endpoint.id().to_string();
        let candidates: Vec<Member> = members
            .iter()
            .filter(|m| m.id != my_id_hex)
            .cloned()
            .collect();
        let added = crate::config::add_members(&candidates)?;
        if added.is_empty() {
            return Ok(());
        }
        for m in &added {
            match m.id.parse::<EndpointId>() {
                Ok(id) => {
                    info!("mesh: added member `{}`", m.name);
                    self.peers_by_name.write().unwrap().insert(m.name.clone(), id);
                    self.names_by_id.write().unwrap().insert(id, m.name.clone());
                }
                Err(_) => warn!("mesh: member `{}` has an unparseable id", m.name),
            }
        }
        self.spawn_dials();
        let roster = self.roster();
        for (_, (_, tx)) in self.control_txs.lock().unwrap().iter() {
            let _ = tx.send(Msg::Roster {
                members: roster.clone(),
            });
        }
        Ok(())
    }

    /// Accept side of a mesh join: verify the token proof, admit the member,
    /// and hand back the full roster.
    async fn handle_join_accept(
        self: &Arc<Self>,
        conn: iroh::endpoint::Connection,
    ) -> Result<()> {
        let remote = conn.remote_id();
        let (mut send, mut recv) = conn.accept_bi().await?;
        let req: JoinRequest = read_frame(&mut recv).await?;
        let deny = |reason: &str| JoinResponse::Denied {
            reason: reason.to_string(),
        };
        let secret = *self.mesh.read().unwrap();
        let verdict = match secret {
            None => Err(deny("this host is not part of a mesh")),
            Some(secret) => {
                let expected_proof = crate::mesh::proof(
                    &secret,
                    remote.as_bytes(),
                    self.endpoint.id().as_bytes(),
                );
                if req.mesh_id != crate::mesh::mesh_id(&secret) {
                    Err(deny("different mesh"))
                } else if req.proof != expected_proof {
                    Err(deny("invalid mesh proof"))
                } else if req.member.id != remote.to_string() {
                    Err(deny("claimed id does not match the connection"))
                } else if req.member.name.is_empty() || req.member.name == self.my_name {
                    Err(deny("that name is taken by this host"))
                } else if self
                    .peers_by_name
                    .read()
                    .unwrap()
                    .get(&req.member.name)
                    .is_some_and(|known| *known != remote)
                {
                    Err(deny("that name is taken by another member"))
                } else {
                    Ok(())
                }
            }
        };
        let response = match verdict {
            Err(denied) => denied,
            Ok(()) => {
                info!("mesh: `{}` joined via token", req.member.name);
                self.apply_roster(std::slice::from_ref(&req.member))?;
                JoinResponse::Welcome {
                    members: self.roster(),
                }
            }
        };
        let denied = matches!(response, JoinResponse::Denied { .. });
        write_frame(&mut send, &response).await?;
        send.finish()?;
        let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
        if denied {
            warn!("mesh: denied join attempt from {remote}");
        }
        Ok(())
    }

    /// Dial side of a mesh join (used by `mesh join` through the daemon).
    pub async fn join_bootstrap(self: &Arc<Self>, bootstrap: &str) -> Result<String> {
        let secret = self
            .mesh
            .read()
            .unwrap()
            .context("no mesh token in the config — run `mousefinity mesh join <ticket>`")?;
        let id = self
            .peers_by_name
            .read()
            .unwrap()
            .get(bootstrap)
            .copied()
            .with_context(|| format!("unknown bootstrap peer `{bootstrap}`"))?;
        let target = self.dial_target(bootstrap, id);
        let me = Member {
            name: self.my_name.clone(),
            id: self.endpoint.id().to_string(),
        };
        let members = join_handshake(&self.endpoint, &secret, target, me).await?;
        let count = members.len();
        self.apply_roster(&members)?;
        Ok(format!("joined mesh: {count} member(s) known"))
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

/// One-shot join handshake against a bootstrap member. Standalone so the
/// `mesh join` CLI can run it without a daemon (it binds its own endpoint).
pub async fn join_handshake(
    endpoint: &Endpoint,
    secret: &[u8; 32],
    target: EndpointAddr,
    me: Member,
) -> Result<Vec<Member>> {
    let conn = endpoint
        .connect(target, ALPN_JOIN)
        .await
        .context("cannot reach the bootstrap member (is it running?)")?;
    let (mut send, mut recv) = conn.open_bi().await?;
    let req = JoinRequest {
        mesh_id: crate::mesh::mesh_id(secret),
        proof: crate::mesh::proof(
            secret,
            endpoint.id().as_bytes(),
            conn.remote_id().as_bytes(),
        ),
        member: me,
    };
    write_frame(&mut send, &req).await?;
    send.finish()?;
    let resp: JoinResponse = read_frame(&mut recv).await?;
    conn.close(0u32.into(), b"join done");
    match resp {
        JoinResponse::Welcome { members } => Ok(members),
        JoinResponse::Denied { reason } => anyhow::bail!("join denied: {reason}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(entries: &[(&str, Option<&str>, Option<&str>)]) -> Layout {
        let mut m = BTreeMap::new();
        for (screen, left, right) in entries {
            m.insert(
                screen.to_string(),
                Neighbors {
                    left: left.map(str::to_string),
                    right: right.map(str::to_string),
                    ..Default::default()
                },
            );
        }
        Layout(m)
    }

    /// The bug this translation exists for: two hosts calling the same machine
    /// different things must not produce two screens after a round trip.
    #[test]
    fn foreign_names_come_back_as_local_ones() {
        // "desktop" knows machine X as `laptop`; we know the same X as `mac`.
        let wire = map_layout(&layout(&[("desktop", None, Some("laptop"))]), |r: &str| {
            Some(match r {
                "desktop" => "id-desktop",
                "laptop" => "id-x",
                other => other,
            }
            .to_string())
        });
        let local = map_layout(&wire, |r: &str| {
            Some(match r {
                "id-desktop" => "desktop",
                "id-x" => "mac",
                other => other,
            }
            .to_string())
        });
        assert_eq!(local, layout(&[("desktop", None, Some("mac"))]));
    }

    #[test]
    fn unresolvable_references_are_dropped() {
        // A name that resolves to nothing is meaningless to the other side.
        let out = map_layout(
            &layout(&[("a", None, Some("ghost")), ("ghost", Some("a"), None)]),
            |r: &str| (r != "ghost").then(|| r.to_string()),
        );
        assert_eq!(out, layout(&[]), "an emptied screen is pruned too");
    }

    /// A layout naming only unpaired machines must not travel: it arrives at
    /// a winning revision and would wipe a working arrangement everywhere.
    #[test]
    fn a_layout_that_resolves_to_nothing_is_not_sent() {
        let local = layout(&[("a", None, Some("ghost"))]);
        let wire = map_layout(&local, |r: &str| (r != "ghost" && r != "a").then(|| r.to_string()));
        assert!(
            wire.0.is_empty() && !local.0.is_empty(),
            "this is the shape layout_to_wire refuses to send"
        );
    }

    #[test]
    fn partially_resolvable_screen_keeps_its_other_edges() {
        let out = map_layout(
            &layout(&[("a", Some("ghost"), Some("b"))]),
            |r: &str| (r != "ghost").then(|| r.to_string()),
        );
        assert_eq!(out, layout(&[("a", None, Some("b"))]));
    }
}
