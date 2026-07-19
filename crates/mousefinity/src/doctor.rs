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
    let mut report = Report::new(true);
    let result = rt.block_on(collect(&mut report));
    // A failure part-way through still leaves useful findings on screen.
    result
}

/// Collects the findings so they can be printed live (`doctor`) or captured
/// into a file (`report`) without running the probes twice.
pub struct Report {
    lines: Vec<String>,
    echo: bool,
    failures: usize,
}

impl Report {
    pub fn new(echo: bool) -> Self {
        Self {
            lines: Vec::new(),
            echo,
            failures: 0,
        }
    }

    fn push(&mut self, line: String) {
        if self.echo {
            println!("{line}");
        }
        self.lines.push(line);
    }

    fn line(&mut self, text: impl Into<String>) {
        self.push(text.into());
    }

    fn ok(&mut self, label: &str, detail: impl AsRef<str>) {
        self.push(format!("  [ ok ] {label}: {}", detail.as_ref()));
    }

    fn bad(&mut self, label: &str, detail: impl AsRef<str>) {
        self.failures += 1;
        self.push(format!("  [FAIL] {label}: {}", detail.as_ref()));
    }

    fn note(&mut self, label: &str, detail: impl AsRef<str>) {
        self.push(format!("  [note] {label}: {}", detail.as_ref()));
    }

    /// How many checks came back failing; the bundle leads with this.
    pub fn failures(&self) -> usize {
        self.failures
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }
}

/// Run every probe, recording findings into `rep`.
pub async fn collect(rep: &mut Report) -> Result<()> {
    let mut cfg = config::load()?;
    let secret = config::load_or_create_secret()?;
    rep.line(format!("mousefinity doctor — host `{}`", cfg.name));
    rep.line(format!(
        "  pairing id: {}",
        iroh::SecretKey::from_bytes(&secret).public()
    ));

    if crate::ipc::daemon_reachable() {
        rep.note(
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
            rep.note(
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
    rep.ok("bind", "endpoint up");
    if let Some(url) = &custom_relay {
        rep.note("relay", format!("using self-hosted relay {url}"));
    }
    if cfg.network.relay.as_deref() == Some("off") {
        rep.note("relay", "disabled by config — direct connections only");
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
        None => rep.bad(
            "net probe",
            "no result in 12s — outbound UDP and relay HTTPS both look blocked",
        ),
        Some(r) => {
            if r.udp_v4 || r.udp_v6 {
                rep.ok(
                    "udp egress",
                    format!(
                        "works (ipv4: {}, ipv6: {}) — direct paths & hole-punching possible",
                        r.udp_v4, r.udp_v6
                    ),
                );
            } else {
                rep.bad(
                    "udp egress",
                    "blocked — no direct paths; traffic must ride the relay over TCP 443",
                );
            }
            if let Some(addr) = r.global_v4 {
                rep.ok("public address", format!("{addr} (ipv4)"));
            }
            if r.mapping_varies_by_dest_ipv4 == Some(true) {
                rep.bad(
                    "nat type",
                    "symmetric-style NAT (mapping varies by destination) — \
                     hole-punching often fails here; expect relayed traffic",
                );
            }
            if r.captive_portal == Some(true) {
                rep.bad("captive portal", "detected — sign into the network first");
            }
            match &r.preferred_relay {
                Some(url) => rep.ok("relay", format!("reachable, closest: {url}")),
                None if cfg.network.relay.as_deref() != Some("off") => rep.bad(
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
            rep.ok("home relay", format!("connected to {}", s.url()));
        } else if let Some(err) = s.last_error() {
            rep.bad("home relay", format!("{}: {err}", s.url()));
        }
    }

    // 4. Per-peer reachability, path types, and a real protocol handshake.
    if cfg.peers.is_empty() {
        rep.note("peers", "none configured — add one and re-run to test reachability");
    }
    let screen = rdev::display_size().map(|(w, h)| (w as u32, h as u32)).unwrap_or((0, 0));
    for (name, peer) in &cfg.peers {
        let id: iroh::EndpointId = match peer.id.parse() {
            Ok(id) => id,
            Err(_) => {
                rep.bad(&format!("peer {name}"), "invalid pairing id in config");
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
            Err(_) => rep.bad(
                &format!("peer {name}"),
                "connect timed out after 10s — peer offline, or all paths blocked",
            ),
            Ok(Err(e)) => rep.bad(
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
                    Ok(Ok(true)) => rep.ok(
                        &format!("peer {name}"),
                        format!("connected & mutually paired — {path_info}"),
                    ),
                    _ => rep.note(
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

    // 5. Layout sanity. Connectivity and hopping are separate failures: a
    //    perfectly connected pair still will not move the cursor if this
    //    host's screen has no edge pointing at the other, or if the edge
    //    names something that is not a paired peer. That combination looks
    //    exactly like "it says connected but nothing happens", so check it
    //    here rather than leaving it to be inferred.
    check_layout(&cfg, rep);

    rep.line("done.");
    Ok(())
}

fn check_layout(cfg: &config::Config, rep: &mut Report) {
    let mine = cfg.layout.get(&cfg.name);
    let edges: Vec<(mousefinity_proto::Edge, &str)> = mine
        .map(|n| {
            [
                mousefinity_proto::Edge::Left,
                mousefinity_proto::Edge::Right,
                mousefinity_proto::Edge::Up,
                mousefinity_proto::Edge::Down,
            ]
            .into_iter()
            .filter_map(|e| n.get(e).map(|t| (e, t)))
            .collect()
        })
        .unwrap_or_default();

    if edges.is_empty() {
        rep.bad(
            "layout",
            format!(
                "no screen is placed next to `{}`, so the cursor has nowhere to \
                 go — run `mousefinity link {} right <peer>` (or use the tui)",
                cfg.name, cfg.name
            ),
        );
        return;
    }
    for (edge, target) in edges {
        let label = format!("layout {}", edge.name());
        if target == cfg.name {
            rep.bad(&label, format!("points at this host itself (`{target}`)"));
        } else if cfg.peers.contains_key(target) {
            rep.ok(&label, format!("`{target}` — hop possible when it is up"));
        } else {
            // The usual cause of "connects fine, cursor will not cross".
            rep.bad(
                &label,
                format!(
                    "`{target}` is not a peer on this host, so this edge can never \
                     fire — add it with `mousefinity add-peer {target} <id>`, or fix \
                     the name to match one of: {}",
                    if cfg.peers.is_empty() {
                        "(no peers configured)".to_string()
                    } else {
                        cfg.peers.keys().cloned().collect::<Vec<_>>().join(", ")
                    }
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Peer};
    use mousefinity_proto::Neighbors;

    fn cfg_with(layout: &[(&str, Neighbors)], peers: &[&str]) -> Config {
        Config {
            name: "mac-mini".into(),
            screen: None,
            downloads: None,
            network: Default::default(),
            mesh_secret: None,
            peers: peers
                .iter()
                .map(|n| {
                    (
                        n.to_string(),
                        Peer {
                            id: "1".repeat(64),
                            addrs: vec![],
                        },
                    )
                })
                .collect(),
            layout: layout.iter().map(|(n, v)| (n.to_string(), v.clone())).collect(),
            layout_rev: 0,
        }
    }

    fn run(cfg: &Config) -> (String, usize) {
        let mut rep = Report::new(false);
        check_layout(cfg, &mut rep);
        (rep.text(), rep.failures())
    }

    #[test]
    fn an_edge_to_a_paired_peer_passes() {
        let cfg = cfg_with(
            &[(
                "mac-mini",
                Neighbors {
                    right: Some("p53".into()),
                    ..Default::default()
                },
            )],
            &["p53"],
        );
        let (text, failures) = run(&cfg);
        assert_eq!(failures, 0, "{text}");
        assert!(text.contains("layout right"));
    }

    /// The case that looks like "connected but the cursor will not cross".
    #[test]
    fn an_edge_naming_a_non_peer_fails_and_lists_the_real_names() {
        let cfg = cfg_with(
            &[(
                "mac-mini",
                Neighbors {
                    right: Some("windows-pc".into()),
                    ..Default::default()
                },
            )],
            &["p53"],
        );
        let (text, failures) = run(&cfg);
        assert_eq!(failures, 1);
        assert!(text.contains("can never fire"));
        assert!(text.contains("p53"), "must name what is actually paired: {text}");
    }

    #[test]
    fn no_edge_on_this_host_is_reported_as_the_reason_nothing_hops() {
        // Peers connect fine; there is simply nowhere to go.
        let cfg = cfg_with(&[], &["p53"]);
        let (text, failures) = run(&cfg);
        assert_eq!(failures, 1);
        assert!(text.contains("nowhere to go"), "{text}");
    }

    /// A layout that only describes *other* machines' edges is the shape you
    /// get from a half-finished `link`, and hops from here still cannot fire.
    #[test]
    fn edges_belonging_only_to_other_screens_do_not_count() {
        let cfg = cfg_with(
            &[(
                "p53",
                Neighbors {
                    left: Some("mac-mini".into()),
                    ..Default::default()
                },
            )],
            &["p53"],
        );
        let (_, failures) = run(&cfg);
        assert_eq!(failures, 1);
    }

    #[test]
    fn an_edge_pointing_at_this_host_is_flagged() {
        let cfg = cfg_with(
            &[(
                "mac-mini",
                Neighbors {
                    right: Some("mac-mini".into()),
                    ..Default::default()
                },
            )],
            &["p53"],
        );
        let (text, failures) = run(&cfg);
        assert_eq!(failures, 1);
        assert!(text.contains("itself"), "{text}");
    }
}
