mod capture;
mod clipboard;
mod config;
mod doctor;
mod engine;
mod inject;
mod ipc;
mod keymap;
mod mesh;
mod net;
mod tui;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use mousefinity_proto::Neighbors;
use tracing::info;

#[derive(Parser)]
#[command(
    name = "mousefinity",
    version,
    about = "Share mouse, keyboard, clipboard and files across hosts over secure P2P"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create this host's identity and a starter config.
    Init {
        /// Host name used in the layout (defaults to the computer name).
        #[arg(long)]
        name: Option<String>,
    },
    /// Print this host's pairing id.
    Id,
    /// Trust a peer: `mousefinity add-peer laptop <id-from-their-init>`.
    AddPeer { name: String, id: String },
    /// Place a screen next to another: `mousefinity link desktop right laptop`
    /// means moving off desktop's right edge lands on laptop (and back).
    Link {
        a: String,
        /// left | right | up | down
        edge: String,
        b: String,
    },
    /// Interactive configuration UI (peers, layout, pairing).
    Tui,
    /// Diagnose connectivity: what this network blocks, relay health, and
    /// whether each peer is reachable directly or only via relay.
    Doctor,
    /// Mesh: one shared token that lets machines discover and trust each
    /// other automatically (tenant-isolated even on a shared relay).
    Mesh {
        #[command(subcommand)]
        cmd: MeshCmd,
    },
    /// Run the daemon (input sharing, clipboard sync, file receiving).
    Run,
    /// Send files to a peer via the running daemon.
    Send {
        peer: String,
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
}

fn main() -> Result<()> {
    // Without DPI awareness, Windows reports the scaled logical screen size
    // while low-level mouse hooks deliver physical pixels, which breaks edge
    // detection on any display with scaling enabled.
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let cli = Cli::parse();
    // Doctor prints a human report; keep library logging out of it unless
    // the user explicitly asks via RUST_LOG.
    let default_filter = if matches!(cli.cmd, Cmd::Doctor) {
        "error"
    } else {
        "info,iroh=warn"
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default_filter.into()),
        )
        .init();
    match cli.cmd {
        Cmd::Init { name } => cmd_init(name),
        Cmd::Id => cmd_id(),
        Cmd::AddPeer { name, id } => cmd_add_peer(name, id),
        Cmd::Link { a, edge, b } => cmd_link(a, edge, b),
        Cmd::Tui => tui::run(),
        Cmd::Doctor => doctor::run(),
        Cmd::Mesh { cmd } => cmd_mesh(cmd),
        Cmd::Run => cmd_run(),
        Cmd::Send { peer, files } => ipc::client_send(&peer, &files),
    }
}

fn host_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .map(|s| s.to_lowercase())
        .unwrap_or_else(|_| "my-host".into())
}

fn identity() -> Result<iroh::SecretKey> {
    let secret = config::load_or_create_secret()?;
    Ok(iroh::SecretKey::from_bytes(&secret))
}

fn cmd_init(name: Option<String>) -> Result<()> {
    let key = identity()?;
    if config::config_path()?.exists() {
        println!("config already exists at {}", config::config_path()?.display());
    } else {
        let cfg = config::Config {
            name: name.unwrap_or_else(host_name),
            screen: None,
            downloads: None,
            network: Default::default(),
            mesh_secret: None,
            peers: Default::default(),
            layout: Default::default(),
            layout_rev: 0,
        };
        config::save(&cfg)?;
        println!("wrote {}", config::config_path()?.display());
        println!("host name: {}", cfg.name);
    }
    println!("pairing id: {}", key.public());
    println!();
    println!("next steps:");
    println!("  1. run `mousefinity init` on each machine");
    println!("  2. exchange ids: `mousefinity add-peer <name> <id>` on both sides");
    println!("  3. arrange screens: `mousefinity link <this-host> right <name>`");
    println!("  4. start `mousefinity run` everywhere");
    Ok(())
}

fn cmd_id() -> Result<()> {
    println!("{}", identity()?.public());
    Ok(())
}

fn cmd_add_peer(name: String, id: String) -> Result<()> {
    let _: iroh::EndpointId = id.parse().context("that does not look like a pairing id")?;
    let mut cfg = config::load()?;
    if name == cfg.name {
        bail!("`{name}` is this host's own name");
    }
    cfg.peers
        .insert(name.clone(), config::Peer { id, addrs: vec![] });
    config::save(&cfg)?;
    println!("added peer `{name}`");
    Ok(())
}

fn cmd_link(a: String, edge: String, b: String) -> Result<()> {
    let mut cfg = config::load()?;
    for n in [&a, &b] {
        if *n != cfg.name && !cfg.peers.contains_key(n) {
            bail!("`{n}` is neither this host nor a known peer (add it with add-peer first)");
        }
    }
    let (fwd, back): (fn(&mut Neighbors) -> &mut Option<String>, fn(&mut Neighbors) -> &mut Option<String>) =
        match edge.as_str() {
            "right" => (|n| &mut n.right, |n| &mut n.left),
            "left" => (|n| &mut n.left, |n| &mut n.right),
            "up" => (|n| &mut n.up, |n| &mut n.down),
            "down" => (|n| &mut n.down, |n| &mut n.up),
            other => bail!("edge must be left|right|up|down, not `{other}`"),
        };
    *fwd(cfg.layout.entry(a.clone()).or_default()) = Some(b.clone());
    *back(cfg.layout.entry(b.clone()).or_default()) = Some(a.clone());
    cfg.layout_rev = config::now_ms();
    config::save(&cfg)?;
    println!("linked: leaving `{a}` {edge} lands on `{b}`");
    // If a daemon is running, it picks the change up and syncs it to every
    // connected peer; otherwise the sync happens on the next `run`.
    match ipc::client_reload() {
        Ok(_) => println!("daemon reloaded — layout is syncing to connected peers"),
        Err(_) => println!("no running daemon — layout will sync when the daemon starts"),
    }
    Ok(())
}

#[derive(Subcommand)]
enum MeshCmd {
    /// Create a mesh token on this host (making it the first member).
    Init,
    /// Print this mesh's join ticket to share with a new machine.
    Ticket,
    /// Join a mesh using a ticket from `mousefinity mesh ticket`.
    Join { ticket: String },
}

fn cmd_mesh(cmd: MeshCmd) -> Result<()> {
    match cmd {
        MeshCmd::Init => {
            let mut cfg = config::load()?;
            if cfg.mesh_secret.is_none() {
                let mut secret = [0u8; 32];
                use rand::Rng;
                rand::rng().fill_bytes(&mut secret);
                cfg.mesh_secret = Some(data_encoding::HEXLOWER.encode(&secret));
                config::save(&cfg)?;
                println!("mesh created.");
            } else {
                println!("this host already has a mesh token.");
            }
            let _ = ipc::client_reload();
            print_ticket(&cfg.name)
        }
        MeshCmd::Ticket => {
            let cfg = config::load()?;
            if cfg.mesh_secret.is_none() {
                bail!("no mesh on this host — run `mousefinity mesh init` first");
            }
            print_ticket(&cfg.name)
        }
        MeshCmd::Join { ticket } => cmd_mesh_join(&ticket),
    }
}

fn print_ticket(my_name: &str) -> Result<()> {
    let cfg = config::load()?;
    let secret = cfg
        .mesh_secret_bytes()?
        .context("no mesh token configured")?;
    let key = identity()?;
    let ticket = mesh::Ticket {
        secret,
        bootstrap_id: *key.public().as_bytes(),
        bootstrap_name: my_name.to_string(),
    };
    println!("share this ticket with a machine you want to add:");
    println!("  {}", mesh::encode_ticket(&ticket));
    println!("(anyone with the ticket can join — treat it like a Wi-Fi password)");
    Ok(())
}

fn cmd_mesh_join(ticket: &str) -> Result<()> {
    let t = mesh::decode_ticket(ticket)?;
    let bootstrap_id = iroh::EndpointId::from_bytes(&t.bootstrap_id)
        .map_err(|_| anyhow::anyhow!("ticket contains an invalid bootstrap id"))?;
    let mut cfg = config::load()?;
    if cfg.name == t.bootstrap_name {
        bail!(
            "this host is named `{}`, same as the ticket's bootstrap — rename one first",
            cfg.name
        );
    }
    cfg.mesh_secret = Some(data_encoding::HEXLOWER.encode(&t.secret));
    cfg.peers.insert(
        t.bootstrap_name.clone(),
        config::Peer {
            id: bootstrap_id.to_string(),
            addrs: vec![],
        },
    );
    config::save(&cfg)?;

    if ipc::daemon_reachable() {
        // The daemon holds our identity; let it do the handshake.
        ipc::client_reload().context("daemon reload failed")?;
        let msg = ipc::client_join(&t.bootstrap_name)?;
        println!("{msg}");
    } else {
        // No daemon: run the handshake with a temporary endpoint.
        let secret_key = config::load_or_create_secret()?;
        let rt = tokio::runtime::Runtime::new()?;
        let summary = rt.block_on(async {
            let (endpoint, custom_relay) = net::bind_endpoint(&cfg, secret_key).await?;
            let mut target = iroh::EndpointAddr::new(bootstrap_id);
            if let Some(url) = custom_relay {
                target = target.with_relay_url(url);
            }
            let me = mousefinity_proto::Member {
                name: cfg.name.clone(),
                id: endpoint.id().to_string(),
            };
            let members = net::join_handshake(&endpoint, &t.secret, target, me).await?;
            endpoint.close().await;
            let added = config::add_members(&members)?;
            anyhow::Ok(format!(
                "joined mesh: {} member(s) known, {} imported",
                members.len(),
                added.len()
            ))
        })?;
        println!("{summary}");
        println!("start `mousefinity run` and the mesh will connect.");
    }
    println!("tip: arrange the new screen with `mousefinity link` or the TUI — layout syncs everywhere.");
    Ok(())
}

fn cmd_run() -> Result<()> {
    let cfg = config::load()?;
    let secret = config::load_or_create_secret()?;
    let screen = cfg
        .screen
        .or_else(|| {
            rdev::display_size()
                .ok()
                .map(|(w, h)| (w as u32, h as u32))
        })
        .context("cannot detect screen size; set `screen = [width, height]` in the config")?;
    info!("host `{}`, screen {}x{}", cfg.name, screen.0, screen.1);
    // Synced layouts may legitimately mention screens this host has not
    // paired with (yet); such edges simply never trigger a hop here.
    for (screen_name, n) in &cfg.layout {
        for neighbor in [&n.left, &n.right, &n.up, &n.down].into_iter().flatten() {
            if neighbor != &cfg.name && !cfg.peers.contains_key(neighbor) {
                tracing::warn!(
                    "layout references `{neighbor}` (from `{screen_name}`) but it is not a \
                     paired peer on this host"
                );
            }
        }
    }

    let shared = Arc::new(capture::CaptureShared::default());
    let inject_tx = inject::spawn()?;
    let (engine_tx, engine_rx) = tokio::sync::mpsc::unbounded_channel();

    let engine = engine::Engine::new(
        cfg.name.clone(),
        screen,
        cfg.layout(),
        cfg.layout_rev,
        shared.clone(),
        inject_tx,
    );
    std::thread::Builder::new()
        .name("engine".into())
        .spawn(move || engine.run(engine_rx))?;

    let net_engine_tx = engine_tx.clone();
    std::thread::Builder::new().name("net".into()).spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("failed to start async runtime: {e}");
                std::process::exit(1);
            }
        };
        rt.block_on(async move {
            let net = match net::Net::bind(&cfg, secret, screen, net_engine_tx).await {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("network startup failed: {e:#}");
                    std::process::exit(1);
                }
            };
            info!("pairing id: {}", net.id());
            tokio::spawn(ipc::serve(net.clone()));
            net.run().await;
        });
    })?;

    // Blocks forever; must be the main thread (macOS event tap requirement).
    capture::run(shared, engine_tx);
    Ok(())
}
