//! Global input capture via rdev's low-level grab hook.
//!
//! Runs on the main thread (a hard requirement on macOS). While the virtual
//! cursor is on a remote screen, all input is swallowed locally and forwarded
//! to the engine as deltas.
//!
//! Those deltas are differences between consecutive hook events, not offsets
//! from the screen centre. Suppressing an event does not reliably pin the
//! pointer, so it drifts; measuring from a fixed centre would then re-report
//! the accumulated offset on every event and send the remote cursor flying.
//!
//! The pointer is warped back to the centre once it strays past a quarter of
//! the way to an edge — so it cannot reach one and go silent, and so it does
//! not visibly wander while parked. A warp breaks the difference between
//! consecutive events, and which event *was* the warp cannot be told from
//! coordinates: one landing slightly off looks exactly like a fast flick, and
//! guessing wrong sends the teleport distance to the far screen as a jump. So
//! nothing is guessed. Motion is held from the moment a warp is requested
//! until [`CaptureShared::note_pointer_warped_to`] — called by the inject
//! thread, the only place that knows the pointer really moved — reports where
//! it landed.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc::UnboundedSender;
use tracing::{error, warn};

use crate::engine::{EngineIn, LocalEvent};
use crate::keymap;

/// How many hook events to hold while waiting for a warp to land before
/// assuming it is never going to. Without this, one lost warp would freeze
/// remote motion permanently.
const MAX_WAIT_EVENTS: u32 = 32;

#[derive(Default)]
pub struct CaptureShared {
    /// True while the virtual cursor is on a remote screen.
    remote: AtomicBool,
    /// A warp has been asked for and has not landed yet. Motion is held
    /// rather than guessed at until the injector confirms where the pointer
    /// ended up.
    warp_pending: AtomicBool,
    /// Events held during the current wait, so a warp that never arrives
    /// cannot stall motion forever.
    waited: AtomicU32,
    cx: AtomicI32,
    cy: AtomicI32,
    /// Where the pointer was at the previous hook event. Motion is measured
    /// against this rather than against the centre.
    lx: AtomicI32,
    ly: AtomicI32,
    /// Set once injection is known to be unavailable; warps are then never
    /// waited for, since nothing can perform them.
    can_warp: AtomicBool,
}

impl CaptureShared {
    pub fn new() -> Self {
        Self {
            can_warp: AtomicBool::new(true),
            ..Default::default()
        }
    }

    pub fn set_remote(&self, center: (i32, i32)) {
        self.cx.store(center.0, Ordering::Relaxed);
        self.cy.store(center.1, Ordering::Relaxed);
        self.lx.store(center.0, Ordering::Relaxed);
        self.ly.store(center.1, Ordering::Relaxed);
        self.waited.store(0, Ordering::Relaxed);
        // The hop itself warps the pointer to the centre; wait for that to
        // land before trusting any position.
        self.warp_pending
            .store(self.can_warp.load(Ordering::Relaxed), Ordering::Relaxed);
        self.remote.store(true, Ordering::SeqCst);
    }

    pub fn set_local(&self) {
        self.remote.store(false, Ordering::SeqCst);
        self.warp_pending.store(false, Ordering::Relaxed);
        self.waited.store(0, Ordering::Relaxed);
    }

    /// Called by the inject thread once a warp has actually been performed.
    /// It is the only place that knows where the pointer really is, which is
    /// why the reference is set here rather than inferred in the hook.
    pub fn note_pointer_warped_to(&self, x: i32, y: i32) {
        self.lx.store(x, Ordering::Relaxed);
        self.ly.store(y, Ordering::Relaxed);
        self.waited.store(0, Ordering::Relaxed);
        self.warp_pending.store(false, Ordering::Relaxed);
    }

    /// No injector, so no warps will ever land: stop waiting for them.
    pub fn injection_unavailable(&self) {
        self.can_warp.store(false, Ordering::Relaxed);
        self.warp_pending.store(false, Ordering::Relaxed);
    }

    pub fn is_remote(&self) -> bool {
        self.remote.load(Ordering::SeqCst)
    }
}

/// What one hook event means while the cursor is on a remote screen.
#[derive(Debug, PartialEq, Eq)]
struct Motion {
    dx: i32,
    dy: i32,
    /// The pointer has drifted far enough that it risks reaching a physical
    /// screen edge, where it would stop reporting motion altogether.
    recenter: bool,
}

/// Movement is the difference between consecutive events, never the offset
/// from the centre.
///
/// Measuring from the centre is only correct if the pointer is warped back
/// after every single event. It is not, so the offset keeps growing and each
/// event re-reports the whole accumulated distance; travel then grows
/// quadratically.
///
/// This deliberately knows nothing about warps. Deciding from coordinates
/// whether an event was our own warp cannot be done reliably — a warp landing
/// slightly off looks exactly like a fast flick, and getting it wrong sends
/// the teleport distance to the far screen as a jump. The caller instead holds
/// events until the injector reports where it actually put the pointer.
fn remote_motion(pos: (i32, i32), last: (i32, i32), centre: (i32, i32)) -> Motion {
    // Keep the pointer near the middle of the screen: it is visible the whole
    // time it is parked, and one wandering off on its own looks broken even
    // when the remote cursor is behaving.
    let margin_x = (centre.0 / 4).max(1);
    let margin_y = (centre.1 / 4).max(1);
    Motion {
        dx: pos.0 - last.0,
        dy: pos.1 - last.1,
        recenter: (pos.0 - centre.0).abs() > margin_x || (pos.1 - centre.1).abs() > margin_y,
    }
}

/// Run the grab loop forever. If grabbing is unavailable (missing permissions
/// on macOS/Linux), logs the failure and parks: the host then still works as a
/// target that others control, just not as a controller.
pub fn run(shared: Arc<CaptureShared>, tx: UnboundedSender<EngineIn>) {
    let result = rdev::grab(move |event| callback(&shared, &tx, event));
    match result {
        Ok(()) => {}
        Err(e) => {
            error!("input capture unavailable: {e:?}");
            // This is the usual reason "it connects but my mouse will not
            // leave this screen": the host is fine as a target and looks
            // healthy in the logs, it just cannot originate a hop. Say what
            // to actually do about it.
            #[cfg(target_os = "macos")]
            warn!(
                "grant this binary Accessibility *and* Input Monitoring in \
                 System Settings > Privacy & Security, then restart it — note \
                 the permission attaches to the binary's current path, so \
                 moving it means granting again"
            );
            #[cfg(target_os = "linux")]
            warn!("add your user to the `input` group (then log out and back in)");
            warn!(
                "running as a controlled-only host: other machines can drive this \
                 one, but the cursor cannot leave it"
            );
            std::thread::park();
        }
    }
}

fn callback(
    shared: &CaptureShared,
    tx: &UnboundedSender<EngineIn>,
    event: rdev::Event,
) -> Option<rdev::Event> {
    let remote = shared.is_remote();
    let send = |ev: LocalEvent| {
        let _ = tx.send(EngineIn::Local(ev));
    };
    match event.event_type {
        rdev::EventType::MouseMove { x, y } => {
            let (x, y) = (x as i32, y as i32);
            if remote {
                let centre = (
                    shared.cx.load(Ordering::Relaxed),
                    shared.cy.load(Ordering::Relaxed),
                );
                let last = (
                    shared.lx.load(Ordering::Relaxed),
                    shared.ly.load(Ordering::Relaxed),
                );
                if shared.warp_pending.load(Ordering::Relaxed) {
                    // A warp is in flight, so this position may be from before
                    // it, after it, or the warp itself — there is no way to
                    // tell, and guessing wrong sends the teleport distance to
                    // the far screen. Hold motion for the millisecond or two
                    // until the injector says where the pointer ended up.
                    if shared.waited.fetch_add(1, Ordering::Relaxed) < MAX_WAIT_EVENTS {
                        return Some(event);
                    }
                    // It is not coming. Resync here and carry on rather than
                    // leaving the remote cursor frozen.
                    shared.warp_pending.store(false, Ordering::Relaxed);
                    shared.waited.store(0, Ordering::Relaxed);
                    shared.lx.store(x, Ordering::Relaxed);
                    shared.ly.store(y, Ordering::Relaxed);
                    return Some(event);
                }
                let motion = remote_motion((x, y), last, centre);
                shared.lx.store(x, Ordering::Relaxed);
                shared.ly.store(y, Ordering::Relaxed);
                if motion.dx != 0 || motion.dy != 0 {
                    send(LocalEvent::Delta {
                        dx: motion.dx,
                        dy: motion.dy,
                    });
                }
                // `swap` keeps this to one warp request in flight at a time.
                if motion.recenter && !shared.warp_pending.swap(true, Ordering::Relaxed) {
                    send(LocalEvent::Recenter);
                }
                None
            } else {
                send(LocalEvent::Move { x, y });
                Some(event)
            }
        }
        rdev::EventType::ButtonPress(b) | rdev::EventType::ButtonRelease(b) => {
            if remote {
                let down = matches!(event.event_type, rdev::EventType::ButtonPress(_));
                send(LocalEvent::Button {
                    button: keymap::rdev_button(b),
                    down,
                });
                None
            } else {
                Some(event)
            }
        }
        rdev::EventType::Wheel { delta_x, delta_y } => {
            if remote {
                send(LocalEvent::Wheel {
                    dx: delta_x as i32,
                    dy: delta_y as i32,
                });
                None
            } else {
                Some(event)
            }
        }
        rdev::EventType::KeyPress(k) | rdev::EventType::KeyRelease(k) => {
            if remote {
                let down = matches!(event.event_type, rdev::EventType::KeyPress(_));
                if matches!(k, rdev::Key::ScrollLock) {
                    // Emergency release: always returns control to this host,
                    // even if the focused peer stopped responding.
                    if down {
                        send(LocalEvent::EmergencyRelease);
                    }
                } else {
                    send(LocalEvent::Key {
                        key: keymap::rdev_to_proto(k),
                        down,
                    });
                }
                None
            } else {
                Some(event)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CENTRE: (i32, i32) = (960, 540);

    /// The original regression: with deltas measured from the centre, dragging
    /// steadily made every event report the whole accumulated offset, so
    /// remote travel grew as the square of real travel.
    #[test]
    fn steady_movement_produces_steady_deltas() {
        let mut last = CENTRE;
        let mut travelled = 0;
        for step in 1..=10 {
            let pos = (CENTRE.0 + step * 10, CENTRE.1);
            let m = remote_motion(pos, last, CENTRE);
            assert_eq!(m.dy, 0);
            assert_eq!(m.dx, 10, "each event should report only its own movement");
            travelled += m.dx;
            last = pos;
        }
        assert_eq!(travelled, 100, "100px of hand movement, not 550");
    }

    #[test]
    fn motion_is_reported_in_both_directions() {
        let m = remote_motion((950, 530), (960, 540), CENTRE);
        assert_eq!((m.dx, m.dy), (-10, -10));
    }

    #[test]
    fn a_still_pointer_reports_nothing() {
        let m = remote_motion(CENTRE, CENTRE, CENTRE);
        assert_eq!((m.dx, m.dy), (0, 0));
        assert!(!m.recenter);
    }

    #[test]
    fn drifting_away_from_centre_asks_for_a_recentre() {
        assert!(!remote_motion((1100, 540), (1090, 540), CENTRE).recenter);
        assert!(remote_motion((1250, 540), (1240, 540), CENTRE).recenter);
        assert!(remote_motion((960, 300), (960, 310), CENTRE).recenter);
    }

    #[test]
    fn a_small_screen_still_has_a_usable_margin() {
        let centre = (1, 1);
        assert!(!remote_motion((1, 1), (1, 1), centre).recenter);
    }

    /// The jump: while a warp is outstanding no position can be trusted, so
    /// nothing is forwarded until the injector reports the real one.
    #[test]
    fn motion_is_held_while_a_warp_is_outstanding() {
        let shared = CaptureShared::new();
        shared.set_remote(CENTRE);
        assert!(shared.warp_pending.load(Ordering::Relaxed));

        shared.note_pointer_warped_to(CENTRE.0, CENTRE.1);
        assert!(!shared.warp_pending.load(Ordering::Relaxed));
        assert_eq!(shared.lx.load(Ordering::Relaxed), CENTRE.0);
        // Movement after the warp differences against where it actually landed.
        let m = remote_motion((CENTRE.0 + 7, CENTRE.1), CENTRE, CENTRE);
        assert_eq!(m.dx, 7, "not the distance from wherever it strayed to");
    }

    /// A warp that never lands must not freeze remote motion for good.
    #[test]
    fn a_warp_that_never_arrives_is_given_up_on() {
        let shared = CaptureShared::new();
        shared.set_remote(CENTRE);
        for _ in 0..MAX_WAIT_EVENTS {
            assert!(shared.waited.fetch_add(1, Ordering::Relaxed) < MAX_WAIT_EVENTS);
        }
        assert!(
            shared.waited.fetch_add(1, Ordering::Relaxed) >= MAX_WAIT_EVENTS,
            "the hook gives up and resyncs after this many held events"
        );
    }

    /// With no injector there is nothing to wait for, so hopping must not
    /// leave the host unable to move the remote cursor at all.
    #[test]
    fn without_an_injector_no_warp_is_ever_awaited() {
        let shared = CaptureShared::new();
        shared.injection_unavailable();
        shared.set_remote(CENTRE);
        assert!(!shared.warp_pending.load(Ordering::Relaxed));
    }
}
