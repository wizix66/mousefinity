mod capture;
mod clipboard;
mod config;
mod engine;
mod inject;
mod ipc;
mod keymap;
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,iroh=warn".into()),
        )
        .init();
    match Cli::parse().cmd {
        Cmd::Init { name } => cmd_init(name),
        Cmd::Id => cmd_id(),
        Cmd::AddPeer { name, id } => cmd_add_peer(name, id),
        Cmd::Link { a, edge, b } => cmd_link(a, edge, b),
        Cmd::Tui => tui::run(),
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
