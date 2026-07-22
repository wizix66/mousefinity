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
    /// The parked pointer has drifted towards a physical edge; put it back at
    /// the centre before it gets stuck there and stops reporting motion.
    Recenter,
    /// ScrollLock pressed: force control back to this host.
    EmergencyRelease,
    /// Capture needs to know where the pointer really is, to settle whether
    /// swallowing a move event on this OS moves it at all. Only the injector
    /// can answer, and it answers capture directly.
    WhereIsPointer,
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
    /// Wind down before the process exits: hand control back and let go of
    /// anything still held, here and on the peer being driven.
    Shutdown,
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
    /// Edge targets we have already complained about. A blocked hop retries on
    /// every mouse event at the edge, so the explanation has to be said once
    /// rather than thousands of times.
    warned_unreachable: std::collections::HashSet<String>,
    /// What this host is currently holding down on behalf of the peer driving
    /// it. Nothing else knows: a key press arrives as one message and its
    /// release as another, so a link that dies in between leaves the key down
    /// with no record that it ever went down. A stuck Ctrl is the usual
    /// result, after which Esc opens the start menu and the keyboard appears
    /// to have gone mad.
    held_keys: std::collections::HashSet<Key>,
    held_buttons: std::collections::HashSet<Button>,
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
            warned_unreachable: std::collections::HashSet::new(),
            held_keys: std::collections::HashSet::new(),
            held_buttons: std::collections::HashSet::new(),
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
                // It can be complained about again if it drops later.
                self.warned_unreachable.remove(&name);
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
                    // The peer driving this host vanished — killed, crashed or
                    // disconnected — so it can never send the releases itself.
                    self.release_held();
                }
            }
            EngineIn::PeerMsg { name, msg } => self.on_peer(name, msg),
            EngineIn::SetLayout { rev, layout } => self.adopt_layout(rev, layout, None),
            EngineIn::Shutdown => self.shutdown(),
        }
    }

    /// Leave both ends in a usable state.
    ///
    /// The peer being driven is holding whatever this host last sent down, and
    /// once this process is gone nothing will ever tell it otherwise — so send
    /// the releases explicitly rather than relying on the disconnect being
    /// noticed. Then hand control back, so the cursor is not parked on a
    /// screen that no longer has a driver.
    fn shutdown(&mut self) {
        if let Focus::Remote { name, .. } = &self.focus {
            let name = name.clone();
            info!("handing control back to {} before exit", self.my_name);
            if let Some(peer) = self.peers.get(&name) {
                for key in [
                    Key::ControlLeft,
                    Key::ControlRight,
                    Key::ShiftLeft,
                    Key::ShiftRight,
                    Key::Alt,
                    Key::AltGr,
                    Key::MetaLeft,
                    Key::MetaRight,
                ] {
                    let _ = peer.tx.send(Msg::Key { key, down: false });
                }
                for button in [Button::Left, Button::Right, Button::Middle] {
                    let _ = peer.tx.send(Msg::Button {
                        button,
                        down: false,
                    });
                }
                let _ = peer.tx.send(Msg::Leave);
            }
            self.return_local_center();
        }
        // And anything a peer was holding down on this host.
        self.controlled_by = None;
        self.release_held();
    }

    /// Adopt a layout if its revision is strictly newer than ours, persist it,
    /// and gossip it to every peer except where it came from. Echoes carry an
    /// equal revision and stop here, so gossip converges.
    fn adopt_layout(&mut self, rev: u64, layout: Layout, source: Option<&str>) {
        if rev <= self.layout_rev {
            return;
        }
        // A newer revision that says "there is no arrangement" is far more
        // likely to be a misconfigured or half-translated peer than a genuine
        // intent to unlink every screen, and adopting it silently deletes a
        // working setup on every machine it reaches. Clearing the layout
        // locally still works; it just does not travel.
        if layout.0.is_empty() && !self.layout.0.is_empty() {
            if let Some(from) = source {
                warn!(
                    "ignoring empty layout rev {rev} from `{from}` — keeping the {} screen(s) \
                     configured here. `{from}` probably references machines it has not paired \
                     with; run `mousefinity doctor` there",
                    self.layout.0.len()
                );
                return;
            }
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

    /// Let go of everything held on behalf of a controlling peer.
    ///
    /// Injecting a release for a key that is not actually down is harmless, so
    /// this errs towards releasing rather than tracking perfectly.
    fn release_held(&mut self) {
        if self.held_keys.is_empty() && self.held_buttons.is_empty() {
            return;
        }
        warn!(
            "releasing {} key(s) and {} button(s) still held by the peer that \
             was driving this host",
            self.held_keys.len(),
            self.held_buttons.len()
        );
        for key in self.held_keys.drain() {
            let _ = self.inject.send(InjectCmd::Key { key, down: false });
        }
        for button in self.held_buttons.drain() {
            let _ = self.inject.send(InjectCmd::Button {
                button,
                down: false,
            });
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
            LocalEvent::Recenter => {
                // Only meaningful while parked; capture sets warp_pending so
                // the resulting event is recognised as ours and not as motion.
                if !matches!(self.focus, Focus::Local) {
                    let _ = self.inject.send(InjectCmd::MoveAbs {
                        x: self.my_screen.0 as i32 / 2,
                        y: self.my_screen.1 as i32 / 2,
                    });
                }
            }
            LocalEvent::WhereIsPointer => {
                let _ = self.inject.send(InjectCmd::WhereIsPointer);
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
            // Staying put is right — but silence here is why "connected, yet
            // the cursor will not cross" is so hard to diagnose. Say it once.
            if self.warned_unreachable.insert(target.to_string()) {
                warn!(
                    "cursor hit the {} edge of `{}` but `{target}` is not connected, \
                     so it stays put; connected now: {}. run `mousefinity doctor` \
                     if `{target}` is not the name you expect",
                    edge.name(),
                    self.my_name,
                    if self.peers.is_empty() {
                        "nothing".to_string()
                    } else {
                        self.peers.keys().cloned().collect::<Vec<_>>().join(", ")
                    }
                );
            }
            return;
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
                    // Hopping away mid-chord is ordinary: a held modifier must
                    // not be left down on a screen nobody is driving.
                    self.release_held();
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
                    if down {
                        self.held_buttons.insert(button);
                    } else {
                        self.held_buttons.remove(&button);
                    }
                    let _ = self.inject.send(InjectCmd::Button { button, down });
                }
            }
            Msg::Key { key, down } => {
                if self.controlled_by.as_deref() == Some(name.as_str()) {
                    if down {
                        self.held_keys.insert(key);
                    } else {
                        self.held_keys.remove(&key);
                    }
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

    /// A peer whose own config is broken translates its layout to nothing and
    /// gossips that at a winning revision. Adopting it would silently unlink
    /// every screen here, which looks exactly like "it connects but the cursor
    /// will not cross".
    /// Drain whatever the injector has been told to do so far.
    fn drain_inject(rig: &mut Rig) -> Vec<InjectCmd> {
        std::thread::sleep(std::time::Duration::from_millis(120));
        let mut out = Vec::new();
        while let Ok(cmd) = rig.inject_rx.try_recv() {
            out.push(cmd);
        }
        out
    }

    fn control_us_holding_ctrl(rig: &Rig) {
        rig.tx
            .send(EngineIn::PeerMsg {
                name: "b".into(),
                msg: Msg::Enter { x: 10, y: 10 },
            })
            .unwrap();
        rig.tx
            .send(EngineIn::PeerMsg {
                name: "b".into(),
                msg: Msg::Key {
                    key: Key::ControlLeft,
                    down: true,
                },
            })
            .unwrap();
    }

    /// The reported bug: the controlling host dies mid-chord, so the release
    /// never arrives and Ctrl stays down here forever. On Windows that makes
    /// Esc open the start menu, which reads as a scrambled keyboard.
    #[test]
    fn a_peer_that_vanishes_does_not_leave_its_keys_held() {
        let mut r = rig();
        control_us_holding_ctrl(&r);
        drain_inject(&mut r);

        r.tx.send(EngineIn::PeerDown {
            name: "b".into(),
            epoch: 1,
        })
        .unwrap();

        let released: Vec<_> = drain_inject(&mut r)
            .into_iter()
            .filter(|c| matches!(c, InjectCmd::Key { key: Key::ControlLeft, down: false }))
            .collect();
        assert_eq!(released.len(), 1, "ctrl must be released when the peer drops");
    }

    /// Hopping away with a modifier held is ordinary use, and must not strand
    /// it either.
    #[test]
    fn leaving_releases_what_was_held() {
        let mut r = rig();
        control_us_holding_ctrl(&r);
        drain_inject(&mut r);

        r.tx.send(EngineIn::PeerMsg {
            name: "b".into(),
            msg: Msg::Leave,
        })
        .unwrap();

        assert!(
            drain_inject(&mut r).iter().any(|c| matches!(
                c,
                InjectCmd::Key { key: Key::ControlLeft, down: false }
            )),
            "ctrl must be released when the cursor leaves"
        );
    }

    /// A key released normally must not be released twice on the way out.
    #[test]
    fn a_key_released_normally_is_forgotten() {
        let mut r = rig();
        control_us_holding_ctrl(&r);
        r.tx.send(EngineIn::PeerMsg {
            name: "b".into(),
            msg: Msg::Key {
                key: Key::ControlLeft,
                down: false,
            },
        })
        .unwrap();
        drain_inject(&mut r);

        r.tx.send(EngineIn::PeerDown {
            name: "b".into(),
            epoch: 1,
        })
        .unwrap();
        assert!(
            drain_inject(&mut r).is_empty(),
            "nothing was still held, so nothing should be released"
        );
    }

    /// Shutting down while driving a peer must tell it to let go, since after
    /// this process exits nothing ever will.
    #[test]
    fn shutdown_releases_modifiers_on_the_peer_being_driven() {
        let mut r = rig();
        r.tx.send(EngineIn::Local(LocalEvent::Move { x: 999, y: 500 }))
            .unwrap();
        match recv(&mut r.peer_rx) {
            Msg::Enter { .. } => {}
            other => panic!("expected to hop first, got {other:?}"),
        }

        r.tx.send(EngineIn::Shutdown).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(150));

        let mut released_ctrl = false;
        let mut left = false;
        while let Ok(msg) = r.peer_rx.try_recv() {
            match msg {
                Msg::Key { key: Key::ControlLeft, down: false } => released_ctrl = true,
                Msg::Leave => left = true,
                _ => {}
            }
        }
        assert!(released_ctrl, "modifiers must be released on the driven peer");
        assert!(left, "the peer must be told the cursor is gone");
    }

    #[test]
    fn an_empty_layout_from_a_peer_does_not_wipe_a_working_one() {
        let mut r = rig();
        r.tx.send(EngineIn::PeerMsg {
            name: "b".into(),
            msg: Msg::Layout {
                rev: u64::MAX,
                layout: Layout(std::collections::BTreeMap::new()),
            },
        })
        .unwrap();
        // The edge still hops, so the arrangement survived.
        r.tx.send(EngineIn::Local(LocalEvent::Move { x: 999, y: 500 }))
            .unwrap();
        match recv(&mut r.peer_rx) {
            Msg::Enter { .. } => {}
            other => panic!("layout was wiped by the empty gossip: got {other:?}"),
        }
    }

    /// Clearing the layout locally is a real intent and must still apply.
    #[test]
    fn an_empty_layout_from_disk_is_applied() {
        let mut r = rig();
        r.tx.send(EngineIn::SetLayout {
            rev: u64::MAX,
            layout: Layout(std::collections::BTreeMap::new()),
        })
        .unwrap();
        // A local edit is authoritative, so it is adopted and gossiped on.
        match recv(&mut r.peer_rx) {
            Msg::Layout { layout, .. } => assert!(layout.0.is_empty()),
            other => panic!("expected the cleared layout to gossip, got {other:?}"),
        }
        r.tx.send(EngineIn::Local(LocalEvent::Move { x: 999, y: 500 }))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(
            r.peer_rx.try_recv().is_err(),
            "a local edit clearing the layout should stop hops"
        );
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
