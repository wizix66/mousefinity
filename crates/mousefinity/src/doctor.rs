//! `mousefinity doctor`: network diagnostics that answer "why won't these
//! two hosts connect?" — what this network blocks, which relay we reached,
//! and whether each configured peer is reachable directly or only relayed.

use std::time::{Duration, Instant};

use anyhow::Result;
use iroh::Watcher;
use mousefinity_proto::{read_frame, write_frame, Msg, ALPN_CONTROL, PROTO_VERSION};
use tokio::time::timeout;

use crate::config;
use crate::net;

pub fn run() -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async())
}

fn ok(label: &str, detail: impl AsRef<str>) {
    println!("  [ ok ] {label}: {}", detail.as_ref());
}

fn bad(label: &str, detail: impl AsRef<str>) {
    println!("  [FAIL] {label}: {}", detail.as_ref());
}

fn note(label: &str, detail: impl AsRef<str>) {
    println!("  [note] {label}: {}", detail.as_ref());
}

async fn run_async() -> Result<()> {
    let mut cfg = config::load()?;
    let secret = config::load_or_create_secret()?;
    println!("mousefinity doctor — host `{}`", cfg.name);
    println!(
        "  pairing id: {}",
        iroh::SecretKey::from_bytes(&secret).public()
    );

    if crate::ipc::daemon_reachable() {
        note(
            "daemon",
            "a daemon is running on this machine; doctor shares its identity, so \
             expect a brief reconnect blip on peers while diagnostics run",
        );
    }

    // 1. Bind (fall back to an ephemeral port if the daemon holds the fixed one).
    let bind_res = net::bind_endpoint(&cfg, secret).await;
    let (endpoint, custom_relay) = match bind_res {
        Ok(v) => v,
        Err(e) if cfg.network.port.is_some() => {
            note(
                "bind",
                format!(
                    "fixed port {} unavailable ({e:#}); retrying on an ephemeral port",
                    cfg.network.port.unwrap_or(0)
                ),
            );
            cfg.network.port = None;
            net::bind_endpoint(&cfg, secret).await?
        }
        Err(e) => return Err(e),
    };
    ok("bind", "endpoint up");
    if let Some(url) = &custom_relay {
        note("relay", format!("using self-hosted relay {url}"));
    }
    if cfg.network.relay.as_deref() == Some("off") {
        note("relay", "disabled by config — direct connections only");
    }

    // 2. Net report: UDP reachability, NAT behaviour, relay latencies.
    let mut report_watch = endpoint.net_report();
    let deadline = Instant::now() + Duration::from_secs(12);
    let report = loop {
        if let Some(r) = report_watch.get() {
            break Some(r);
        }
        if Instant::now() > deadline {
            break None;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    match report {
        None => bad(
            "net probe",
            "no result in 12s — outbound UDP and relay HTTPS both look blocked",
        ),
        Some(r) => {
            if r.udp_v4 || r.udp_v6 {
                ok(
                    "udp egress",
                    format!(
                        "works (ipv4: {}, ipv6: {}) — direct paths & hole-punching possible",
                        r.udp_v4, r.udp_v6
                    ),
                );
            } else {
                bad(
                    "udp egress",
                    "blocked — no direct paths; traffic must ride the relay over TCP 443",
                );
            }
            if let Some(addr) = r.global_v4 {
                ok("public address", format!("{addr} (ipv4)"));
            }
            if r.mapping_varies_by_dest_ipv4 == Some(true) {
                bad(
                    "nat type",
                    "symmetric-style NAT (mapping varies by destination) — \
                     hole-punching often fails here; expect relayed traffic",
                );
            }
            if r.captive_portal == Some(true) {
                bad("captive portal", "detected — sign into the network first");
            }
            match &r.preferred_relay {
                Some(url) => ok("relay", format!("reachable, closest: {url}")),
                None if cfg.network.relay.as_deref() != Some("off") => bad(
                    "relay",
                    "no relay reachable — check outbound TCP 443 / TLS interception",
                ),
                None => {}
            }
        }
    }

    // 3. Home relay connection state (shows TLS errors explicitly).
    let mut relay_watch = endpoint.home_relay_status();
    let statuses = relay_watch.get();
    for s in &statuses {
        if s.is_connected() {
            ok("home relay", format!("connected to {}", s.url()));
        } else if let Some(err) = s.last_error() {
            bad("home relay", format!("{}: {err}", s.url()));
        }
    }

    // 4. Per-peer reachability, path types, and a real protocol handshake.
    if cfg.peers.is_empty() {
        note("peers", "none configured — add one and re-run to test reachability");
    }
    let screen = rdev::display_size().map(|(w, h)| (w as u32, h as u32)).unwrap_or((0, 0));
    for (name, peer) in &cfg.peers {
        let id: iroh::EndpointId = match peer.id.parse() {
            Ok(id) => id,
            Err(_) => {
                bad(&format!("peer {name}"), "invalid pairing id in config");
                continue;
            }
        };
        let mut target = iroh::EndpointAddr::new(id);
        for a in &peer.addrs {
            if let Ok(sock) = a.parse() {
                target = target.with_ip_addr(sock);
            }
        }
        if let Some(url) = &custom_relay {
            target = target.with_relay_url(url.clone());
        }
        match timeout(Duration::from_secs(10), endpoint.connect(target, ALPN_CONTROL)).await {
            Err(_) => bad(
                &format!("peer {name}"),
                "connect timed out after 10s — peer offline, or all paths blocked",
            ),
            Ok(Err(e)) => bad(
                &format!("peer {name}"),
                format!(
                    "cannot connect: {e:#}\n         (\"no addressing information\" means \
                     discovery failed: peer offline or DNS/HTTPS/mDNS lookups all blocked)"
                ),
            ),
            Ok(Ok(conn)) => {
                // Exchange a real Hello so we know the peer trusts us.
                let handshake = async {
                    let (mut send, mut recv) = conn.open_bi().await?;
                    write_frame(
                        &mut send,
                        &Msg::Hello {
                            version: PROTO_VERSION,
                            name: cfg.name.clone(),
                            screen,
                        },
                    )
                    .await?;
                    let hello: Msg = read_frame(&mut recv).await?;
                    anyhow::Ok(matches!(hello, Msg::Hello { .. }))
                };
                let trusted = timeout(Duration::from_secs(5), handshake).await;
                // Give hole-punching a moment to upgrade the path.
                tokio::time::sleep(Duration::from_secs(2)).await;
                let paths = conn.paths();
                let mut summary: Vec<String> = Vec::new();
                for p in paths.iter() {
                    let kind = if p.is_relay() { "relay" } else { "direct" };
                    let sel = if p.is_selected() { " *active*" } else { "" };
                    summary.push(format!(
                        "{kind} {} rtt {:?}{sel}",
                        p.remote_addr(),
                        p.rtt()
                    ));
                }
                let path_info = if summary.is_empty() {
                    "no path info".to_string()
                } else {
                    summary.join("; ")
                };
                match trusted {
                    Ok(Ok(true)) => ok(
                        &format!("peer {name}"),
                        format!("connected & mutually paired — {path_info}"),
                    ),
                    _ => note(
                        &format!("peer {name}"),
                        format!(
                            "connected ({path_info}) but no Hello back — \
                             peer daemon busy, or it has not added this host"
                        ),
                    ),
                }
                conn.close(0u32.into(), b"doctor done");
            }
        }
    }

    endpoint.close().await;
    println!("done.");
    Ok(())
}
