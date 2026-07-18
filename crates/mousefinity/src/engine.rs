//! The focus state machine: tracks the virtual cursor across screens, decides
//! hops, forwards input to the focused peer, and applies input from whichever
//! peer currently controls this host.

use std::collections::HashMap;
use std::sync::Arc;

use mousefinity_proto::{crossing, entry_pos, Button, Edge, Key, Layout, Msg};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, info, warn};

use crate::capture::CaptureShared;
use crate::clipboard::Clip;
use crate::inject::InjectCmd;

/// Cursor is placed this many pixels inside the destination edge after a hop
/// so the arrival itself cannot immediately re-trigger a hop.
const EDGE_INSET: i32 = 2;

#[derive(Debug)]
pub enum LocalEvent {
    /// Absolute cursor position while we are in local mode.
    Move { x: i32, y: i32 },
    /// Relative motion while we are in remote mode.
    Delta { dx: i32, dy: i32 },
    Button { button: Button, down: bool },
    Key { key: Key, down: bool },
    Wheel { dx: i32, dy: i32 },
    /// ScrollLock pressed: force control back to this host.
    EmergencyRelease,
}

pub enum EngineIn {
    Local(LocalEvent),
    PeerUp {
        name: String,
        screen: (u32, u32),
        epoch: u64,
        tx: UnboundedSender<Msg>,
    },
    PeerMsg { name: String, msg: Msg },
    PeerDown { name: String, epoch: u64 },
    /// The config on disk changed (IPC reload): adopt if newer and gossip.
    SetLayout { rev: u64, layout: Layout },
}

struct Peer {
    screen: (u32, u32),
    epoch: u64,
    tx: UnboundedSender<Msg>,
}

enum Focus {
    Local,
    Remote { name: String, vx: i32, vy: i32 },
}

pub struct Engine {
    my_name: String,
    my_screen: (u32, u32),
    layout: Layout,
    layout_rev: u64,
    peers: HashMap<String, Peer>,
    focus: Focus,
    controlled_by: Option<String>,
    shared: Arc<CaptureShared>,
    inject: UnboundedSender<InjectCmd>,
    clip: Clip,
}

impl Engine {
    pub fn new(
        my_name: String,
        my_screen: (u32, u32),
        layout: Layout,
        layout_rev: u64,
        shared: Arc<CaptureShared>,
        inject: UnboundedSender<InjectCmd>,
    ) -> Self {
        Self {
            my_name,
            my_screen,
            layout,
            layout_rev,
            peers: HashMap::new(),
            focus: Focus::Local,
            controlled_by: None,
            shared,
            inject,
            clip: Clip::new(),
        }
    }

    /// Blocking loop; run on a dedicated thread.
    pub fn run(mut self, mut rx: UnboundedReceiver<EngineIn>) {
        while let Some(msg) = rx.blocking_recv() {
            self.handle(msg);
        }
    }

    fn handle(&mut self, input: EngineIn) {
        match input {
            EngineIn::Local(ev) => self.on_local(ev),
            EngineIn::PeerUp {
                name,
                screen,
                epoch,
                tx,
            } => {
                info!("peer up: {name} ({}x{})", screen.0, screen.1);
                // Offer our layout; whichever side has the newer revision wins.
                let _ = tx.send(Msg::Layout {
                    rev: self.layout_rev,
                    layout: self.layout.clone(),
                });
                self.peers.insert(name, Peer { screen, epoch, tx });
            }
            EngineIn::PeerDown { name, epoch } => {
                let stale = self
                    .peers
                    .get(&name)
                    .map(|p| p.epoch != epoch)
                    .unwrap_or(true);
                if stale {
                    return;
                }
                info!("peer down: {name}");
                self.peers.remove(&name);
                if matches!(&self.focus, Focus::Remote { name: n, .. } if *n == name) {
                    warn!("focused peer disconnected; returning control home");
                    self.return_local_center();
                }
                if self.controlled_by.as_deref() == Some(name.as_str()) {
                    self.controlled_by = None;
                }
            }
            EngineIn::PeerMsg { name, msg } => self.on_peer(name, msg),
            EngineIn::SetLayout { rev, layout } => self.adopt_layout(rev, layout, None),
        }
    }

    /// Adopt a layout if its revision is strictly newer than ours, persist it,
    /// and gossip it to every peer except where it came from. Echoes carry an
    /// equal revision and stop here, so gossip converges.
    fn adopt_layout(&mut self, rev: u64, layout: Layout, source: Option<&str>) {
        if rev <= self.layout_rev {
            return;
        }
        info!("adopting layout rev {rev} from {}", source.unwrap_or("disk"));
        self.layout_rev = rev;
        self.layout = layout.clone();
        if source.is_some() {
            if let Err(e) = crate::config::save_synced_layout(rev, &layout) {
                warn!("could not persist synced layout: {e:#}");
            }
        }
        for (name, peer) in &self.peers {
            if Some(name.as_str()) != source {
                let _ = peer.tx.send(Msg::Layout {
                    rev,
                    layout: layout.clone(),
                });
            }
        }
    }

    // ---- local input ----

    fn on_local(&mut self, ev: LocalEvent) {
        match ev {
            LocalEvent::Move { x, y } => {
                // While another host drives our cursor, its injected motion
                // must not trigger our own edge hops.
                if self.controlled_by.is_some() {
                    return;
                }
                if !matches!(self.focus, Focus::Local) {
                    return;
                }
                if let Some(edge) = self.touched_edge(x, y) {
                    if let Some(target) = self
                        .layout
                        .neighbor(&self.my_name, edge)
                        .map(str::to_string)
                    {
                        self.hop_from_local(edge, x, y, &target);
                    }
                }
            }
            LocalEvent::Delta { dx, dy } => self.on_remote_delta(dx, dy),
            LocalEvent::Button { button, down } => {
                self.send_focused(Msg::Button { button, down });
            }
            LocalEvent::Key { key, down } => {
                self.send_focused(Msg::Key { key, down });
            }
            LocalEvent::Wheel { dx, dy } => {
                self.send_focused(Msg::Wheel { dx, dy });
            }
            LocalEvent::EmergencyRelease => {
                if !matches!(self.focus, Focus::Local) {
                    info!("emergency release");
                    self.return_local_center();
                }
            }
        }
    }

    fn touched_edge(&self, x: i32, y: i32) -> Option<Edge> {
        let (w, h) = (self.my_screen.0 as i32, self.my_screen.1 as i32);
        if x <= 0 {
            Some(Edge::Left)
        } else if x >= w - 1 {
            Some(Edge::Right)
        } else if y <= 0 {
            Some(Edge::Up)
        } else if y >= h - 1 {
            Some(Edge::Down)
        } else {
            None
        }
    }

    fn hop_from_local(&mut self, edge: Edge, x: i32, y: i32, target: &str) {
        let Some(peer) = self.peers.get(target) else {
            return; // neighbour configured but offline: stay put
        };
        let (ex, ey) = inset(edge, entry_pos(edge, x, y, self.my_screen, peer.screen), peer.screen);
        debug!("hop {} -> {target} at ({ex},{ey})", self.my_name);
        let _ = peer.tx.send(Msg::Enter { x: ex, y: ey });
        let clip_msg = self.clip.get_text().map(|text| Msg::Clipboard { text });
        if let Some(m) = clip_msg {
            let _ = self.peers.get(target).unwrap().tx.send(m);
        }
        self.focus = Focus::Remote {
            name: target.to_string(),
            vx: ex,
            vy: ey,
        };
        let center = (
            self.my_screen.0 as i32 / 2,
            self.my_screen.1 as i32 / 2,
        );
        self.shared.set_remote(center);
        let _ = self.inject.send(InjectCmd::MoveAbs {
            x: center.0,
            y: center.1,
        });
    }

    fn on_remote_delta(&mut self, dx: i32, dy: i32) {
        let Focus::Remote { name, vx, vy } = &mut self.focus else {
            return;
        };
        *vx += dx;
        *vy += dy;
        let name = name.clone();
        let (nvx, nvy) = (*vx, *vy);
        let screen = match self.peers.get(&name) {
            Some(p) => p.screen,
            None => {
                self.return_local_center();
                return;
            }
        };
        match crossing(nvx, nvy, screen) {
            None => {
                self.send_focused(Msg::MouseMove { x: nvx, y: nvy });
            }
            Some(edge) => {
                let target = self.layout.neighbor(&name, edge).map(str::to_string);
                match target {
                    Some(t) if t == self.my_name => {
                        // Coming home.
                        let (ex, ey) =
                            inset(edge, entry_pos(edge, nvx, nvy, screen, self.my_screen), self.my_screen);
                        self.leave_focused();
                        self.focus = Focus::Local;
                        self.shared.set_local();
                        let _ = self.inject.send(InjectCmd::MoveAbs { x: ex, y: ey });
                        debug!("returned local at ({ex},{ey})");
                    }
                    Some(t) if self.peers.contains_key(&t) => {
                        // Chained hop to a third screen.
                        let to_screen = self.peers.get(&t).unwrap().screen;
                        let (ex, ey) = inset(edge, entry_pos(edge, nvx, nvy, screen, to_screen), to_screen);
                        self.leave_focused();
                        let peer = self.peers.get(&t).unwrap();
                        let _ = peer.tx.send(Msg::Enter { x: ex, y: ey });
                        debug!("chained hop -> {t} at ({ex},{ey})");
                        self.focus = Focus::Remote {
                            name: t,
                            vx: ex,
                            vy: ey,
                        };
                    }
                    _ => {
                        // No neighbour (or offline): clamp to the focused screen.
                        let Focus::Remote { vx, vy, .. } = &mut self.focus else {
                            return;
                        };
                        *vx = nvx.clamp(0, screen.0 as i32 - 1);
                        *vy = nvy.clamp(0, screen.1 as i32 - 1);
                        let (cx, cy) = (*vx, *vy);
                        self.send_focused(Msg::MouseMove { x: cx, y: cy });
                    }
                }
            }
        }
    }

    fn send_focused(&mut self, msg: Msg) {
        if let Focus::Remote { name, .. } = &self.focus {
            if let Some(peer) = self.peers.get(name) {
                let _ = peer.tx.send(msg);
            }
        }
    }

    fn leave_focused(&mut self) {
        self.send_focused(Msg::Leave);
    }

    fn return_local_center(&mut self) {
        self.leave_focused();
        self.focus = Focus::Local;
        self.shared.set_local();
        let _ = self.inject.send(InjectCmd::MoveAbs {
            x: self.my_screen.0 as i32 / 2,
            y: self.my_screen.1 as i32 / 2,
        });
    }

    // ---- peer input ----

    fn on_peer(&mut self, name: String, msg: Msg) {
        match msg {
            Msg::Enter { x, y } => {
                debug!("controlled by {name}, cursor at ({x},{y})");
                self.controlled_by = Some(name);
                let _ = self.inject.send(InjectCmd::MoveAbs { x, y });
            }
            Msg::Leave => {
                if self.controlled_by.as_deref() == Some(name.as_str()) {
                    self.controlled_by = None;
                    // The user may have copied something here; hand it back.
                    let clip_msg = self.clip.get_text().map(|text| Msg::Clipboard { text });
                    if let (Some(m), Some(peer)) = (clip_msg, self.peers.get(&name)) {
                        let _ = peer.tx.send(m);
                    }
                }
            }
            Msg::MouseMove { x, y } => {
                if self.controlled_by.as_deref() == Some(name.as_str()) {
                    let _ = self.inject.send(InjectCmd::MoveAbs { x, y });
                }
            }
            Msg::Button { button, down } => {
                if self.controlled_by.as_deref() == Some(name.as_str()) {
                    let _ = self.inject.send(InjectCmd::Button { button, down });
                }
            }
            Msg::Key { key, down } => {
                if self.controlled_by.as_deref() == Some(name.as_str()) {
                    let _ = self.inject.send(InjectCmd::Key { key, down });
                }
            }
            Msg::Wheel { dx, dy } => {
                if self.controlled_by.as_deref() == Some(name.as_str()) {
                    let _ = self.inject.send(InjectCmd::Wheel { dx, dy });
                }
            }
            Msg::Clipboard { text } => {
                debug!("clipboard from {name} ({} bytes)", text.len());
                self.clip.set_text(&text);
            }
            Msg::Screen { screen } => {
                if let Some(p) = self.peers.get_mut(&name) {
                    p.screen = screen;
                }
            }
            Msg::Layout { rev, layout } => self.adopt_layout(rev, layout, Some(&name)),
            // Rosters are consumed by the net layer; nothing to do here.
            Msg::Roster { .. } => {}
            Msg::Hello { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    struct Rig {
        tx: UnboundedSender<EngineIn>,
        inject_rx: UnboundedReceiver<InjectCmd>,
        peer_rx: UnboundedReceiver<Msg>,
        shared: Arc<CaptureShared>,
    }

    /// Engine on its own thread: this host is 1000x1000 "a", with a 2000x2000
    /// peer "b" connected on the right edge.
    fn rig() -> Rig {
        let mut layout_map = std::collections::BTreeMap::new();
        layout_map.insert(
            "a".to_string(),
            mousefinity_proto::Neighbors {
                right: Some("b".into()),
                ..Default::default()
            },
        );
        layout_map.insert(
            "b".to_string(),
            mousefinity_proto::Neighbors {
                left: Some("a".into()),
                ..Default::default()
            },
        );
        let shared = Arc::new(CaptureShared::default());
        let (inject_tx, inject_rx) = unbounded_channel();
        let (tx, rx) = unbounded_channel();
        let engine = Engine::new(
            "a".into(),
            (1000, 1000),
            Layout(layout_map),
            1,
            shared.clone(),
            inject_tx,
        );
        std::thread::spawn(move || engine.run(rx));
        let (peer_tx, peer_rx) = unbounded_channel();
        tx.send(EngineIn::PeerUp {
            name: "b".into(),
            screen: (2000, 2000),
            epoch: 1,
            tx: peer_tx,
        })
        .unwrap();
        let mut rig = Rig {
            tx,
            inject_rx,
            peer_rx,
            shared,
        };
        // Drain the layout offer that PeerUp always sends.
        match recv(&mut rig.peer_rx) {
            Msg::Layout { .. } => {}
            other => panic!("expected initial Layout offer, got {other:?}"),
        }
        rig
    }

    fn recv<T>(rx: &mut UnboundedReceiver<T>) -> T {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Ok(v) = rx.try_recv() {
                return v;
            }
            assert!(std::time::Instant::now() < deadline, "timed out waiting");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn hop_out_forward_and_return() {
        let mut r = rig();
        // Touch the right edge -> hop to b, scaled entry (y 500/1000 -> 1000/2000).
        r.tx.send(EngineIn::Local(LocalEvent::Move { x: 999, y: 500 }))
            .unwrap();
        match recv(&mut r.peer_rx) {
            Msg::Enter { x, y } => {
                assert_eq!(x, EDGE_INSET);
                assert_eq!(y, 1000);
            }
            other => panic!("expected Enter, got {other:?}"),
        }
        // Local cursor parks at centre while remote.
        match recv(&mut r.inject_rx) {
            InjectCmd::MoveAbs { x: 500, y: 500 } => {}
            other => panic!("expected warp to centre, got {other:?}"),
        }
        assert!(r.shared.is_remote());

        // Motion is forwarded as absolute positions on b.
        r.tx.send(EngineIn::Local(LocalEvent::Delta { dx: 10, dy: -3 }))
            .unwrap();
        // First message may be the clipboard hand-off, skip non-moves.
        let m = loop {
            match recv(&mut r.peer_rx) {
                Msg::Clipboard { .. } => continue,
                m => break m,
            }
        };
        match m {
            Msg::MouseMove { x, y } => {
                assert_eq!(x, EDGE_INSET + 10);
                assert_eq!(y, 997);
            }
            other => panic!("expected MouseMove, got {other:?}"),
        }

        // Push far left -> crosses b's left edge -> comes home.
        r.tx.send(EngineIn::Local(LocalEvent::Delta { dx: -50, dy: 0 }))
            .unwrap();
        let m = loop {
            match recv(&mut r.peer_rx) {
                Msg::Clipboard { .. } => continue,
                m => break m,
            }
        };
        assert!(matches!(m, Msg::Leave), "expected Leave, got {m:?}");
        match recv(&mut r.inject_rx) {
            InjectCmd::MoveAbs { x, y } => {
                assert_eq!(x, 1000 - 1 - EDGE_INSET);
                // y scales back 997/2000 -> 498 on the 1000-high screen
                assert_eq!(y, 498);
            }
            other => panic!("expected warp home, got {other:?}"),
        }
        assert!(!r.shared.is_remote());
    }

    #[test]
    fn emergency_release_comes_home() {
        let mut r = rig();
        r.tx.send(EngineIn::Local(LocalEvent::Move { x: 999, y: 500 }))
            .unwrap();
        recv(&mut r.peer_rx); // Enter
        recv(&mut r.inject_rx); // warp to centre
        assert!(r.shared.is_remote());
        r.tx.send(EngineIn::Local(LocalEvent::EmergencyRelease))
            .unwrap();
        let m = loop {
            match recv(&mut r.peer_rx) {
                Msg::Clipboard { .. } => continue,
                m => break m,
            }
        };
        assert!(matches!(m, Msg::Leave));
        match recv(&mut r.inject_rx) {
            InjectCmd::MoveAbs { x: 500, y: 500 } => {}
            other => panic!("expected warp to centre, got {other:?}"),
        }
        assert!(!r.shared.is_remote());
    }

    #[test]
    fn peer_loss_releases_focus() {
        let mut r = rig();
        r.tx.send(EngineIn::Local(LocalEvent::Move { x: 999, y: 500 }))
            .unwrap();
        recv(&mut r.peer_rx);
        recv(&mut r.inject_rx);
        assert!(r.shared.is_remote());
        r.tx.send(EngineIn::PeerDown {
            name: "b".into(),
            epoch: 1,
        })
        .unwrap();
        match recv(&mut r.inject_rx) {
            InjectCmd::MoveAbs { x: 500, y: 500 } => {}
            other => panic!("expected warp to centre, got {other:?}"),
        }
        assert!(!r.shared.is_remote());
    }

    #[test]
    fn injected_input_ignored_from_non_controller() {
        let mut r = rig();
        // No Enter received: input messages from b must not inject.
        r.tx.send(EngineIn::PeerMsg {
            name: "b".into(),
            msg: Msg::MouseMove { x: 5, y: 5 },
        })
        .unwrap();
        // Then a proper Enter does inject.
        r.tx.send(EngineIn::PeerMsg {
            name: "b".into(),
            msg: Msg::Enter { x: 7, y: 8 },
        })
        .unwrap();
        match recv(&mut r.inject_rx) {
            InjectCmd::MoveAbs { x: 7, y: 8 } => {}
            other => panic!("uncontrolled move must be dropped, got {other:?}"),
        }
    }
}

fn inset(edge: Edge, pos: (i32, i32), screen: (u32, u32)) -> (i32, i32) {
    let (w, h) = (screen.0 as i32, screen.1 as i32);
    let (mut x, mut y) = pos;
    match edge {
        Edge::Left => x = (w - 1 - EDGE_INSET).max(0), // entering from its right side
        Edge::Right => x = EDGE_INSET.min(w - 1),
        Edge::Up => y = (h - 1 - EDGE_INSET).max(0),
        Edge::Down => y = EDGE_INSET.min(h - 1),
    }
    (x, y)
}
