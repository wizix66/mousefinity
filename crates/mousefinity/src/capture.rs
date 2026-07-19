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
//! The pointer is warped back to the centre once it strays past a quarter of
//! the way to an edge — both so it cannot reach one and go silent, and because
//! it stays visible while parked, and a pointer wandering off by itself looks
//! broken even when the remote cursor is behaving.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc::UnboundedSender;
use tracing::{error, warn};

use crate::engine::{EngineIn, LocalEvent};
use crate::keymap;

#[derive(Default)]
pub struct CaptureShared {
    /// True while the virtual cursor is on a remote screen.
    remote: AtomicBool,
    /// True right after we injected a warp-to-centre that must pass through.
    warp_pending: AtomicBool,
    cx: AtomicI32,
    cy: AtomicI32,
    /// Where the pointer was at the previous hook event. Motion is measured
    /// against this rather than against the centre.
    lx: AtomicI32,
    ly: AtomicI32,
}

impl CaptureShared {
    pub fn set_remote(&self, center: (i32, i32)) {
        self.cx.store(center.0, Ordering::Relaxed);
        self.cy.store(center.1, Ordering::Relaxed);
        self.lx.store(center.0, Ordering::Relaxed);
        self.ly.store(center.1, Ordering::Relaxed);
        self.warp_pending.store(true, Ordering::Relaxed);
        self.remote.store(true, Ordering::SeqCst);
    }

    pub fn set_local(&self) {
        self.remote.store(false, Ordering::SeqCst);
        self.warp_pending.store(false, Ordering::Relaxed);
    }

    pub fn is_remote(&self) -> bool {
        self.remote.load(Ordering::SeqCst)
    }
}

/// A warp is treated as landed if it arrives within this many pixels of the
/// centre. Exact equality is too strict: the pointer can settle a pixel off
/// under display scaling, and a missed match means the entire teleport
/// distance is forwarded as real movement — a visible jump on the far screen.
const WARP_TOLERANCE: i32 = 8;

/// What one hook event means while the cursor is on a remote screen.
#[derive(Debug, PartialEq, Eq)]
struct Motion {
    dx: i32,
    dy: i32,
    /// The pointer has drifted far enough that it risks reaching a physical
    /// screen edge, where it would stop reporting motion altogether.
    recenter: bool,
    /// This event was our own warp arriving. Adopt the position as the new
    /// reference and forward nothing.
    absorbed: bool,
}

/// Movement is measured against the *previous* event, not against the centre.
///
/// Measuring from the centre is only correct if the pointer is warped back
/// after every single event. It is not, so the offset from centre keeps
/// growing and each event re-reports the entire accumulated distance; travel
/// then grows quadratically. Differencing against the last position gives the
/// true per-event movement whether or not the pointer happens to be pinned.
///
/// `warp_pending` says we asked for a warp and are waiting for it to land.
/// Only then is an event allowed to be discarded, so ordinary motion can never
/// be swallowed by mistake.
fn remote_motion(
    pos: (i32, i32),
    last: (i32, i32),
    centre: (i32, i32),
    warp_pending: bool,
) -> Motion {
    let dx = pos.0 - last.0;
    let dy = pos.1 - last.1;
    // Keep the pointer near the middle of the screen: it is visible the whole
    // time it is parked, and a pointer wandering off on its own looks broken
    // even when the remote cursor is behaving.
    let margin_x = (centre.0 / 4).max(1);
    let margin_y = (centre.1 / 4).max(1);
    let strayed = (pos.0 - centre.0).abs() > margin_x || (pos.1 - centre.1).abs() > margin_y;

    if warp_pending {
        let at_centre = (pos.0 - centre.0).abs() <= WARP_TOLERANCE
            && (pos.1 - centre.1).abs() <= WARP_TOLERANCE;
        // Either the warp landed where it was aimed, or it landed coalesced
        // with motion — a single event no hand could have produced. Both mean
        // the apparent movement is teleport distance, not travel.
        if at_centre || dx.abs() > margin_x || dy.abs() > margin_y {
            return Motion {
                dx: 0,
                dy: 0,
                recenter: false,
                absorbed: true,
            };
        }
    }
    Motion {
        dx,
        dy,
        recenter: strayed,
        absorbed: false,
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
                let warp_pending = shared.warp_pending.load(Ordering::Relaxed);
                let motion = remote_motion((x, y), last, centre, warp_pending);
                // Track the real position even while a warp is in flight, so
                // events arriving before it lands still difference correctly.
                shared.lx.store(x, Ordering::Relaxed);
                shared.ly.store(y, Ordering::Relaxed);
                if motion.absorbed {
                    // Let it through so the OS cursor really moves to centre.
                    shared.warp_pending.store(false, Ordering::Relaxed);
                    return Some(event);
                }
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

    fn moved(pos: (i32, i32), last: (i32, i32)) -> Motion {
        remote_motion(pos, last, CENTRE, false)
    }

    /// The original regression: with deltas measured from the centre, dragging
    /// steadily made every event report the whole accumulated offset, so
    /// remote travel grew as the square of real travel.
    #[test]
    fn steady_movement_produces_steady_deltas() {
        let mut last = CENTRE;
        let mut travelled = 0;
        for step in 1..=10 {
            let pos = (CENTRE.0 + step * 10, CENTRE.1);
            let m = moved(pos, last);
            assert_eq!(m.dy, 0);
            assert_eq!(m.dx, 10, "each event should report only its own movement");
            travelled += m.dx;
            last = pos;
        }
        // 100px of hand movement is 100px on the remote screen, not 550.
        assert_eq!(travelled, 100);
    }

    #[test]
    fn motion_is_reported_in_both_directions() {
        let m = moved((950, 530), (960, 540));
        assert_eq!((m.dx, m.dy), (-10, -10));
    }

    #[test]
    fn a_still_pointer_reports_nothing() {
        let m = moved(CENTRE, CENTRE);
        assert_eq!((m.dx, m.dy), (0, 0));
        assert!(!m.recenter && !m.absorbed);
    }

    #[test]
    fn drifting_away_from_centre_asks_for_a_recentre() {
        assert!(!moved((1100, 540), (1090, 540)).recenter);
        assert!(moved((1250, 540), (1240, 540)).recenter);
        assert!(moved((960, 300), (960, 310)).recenter);
    }

    /// The jump: a warp landing a pixel off the centre used to miss the exact
    /// match and forward the whole teleport distance as real movement.
    #[test]
    fn a_warp_landing_slightly_off_centre_is_still_absorbed() {
        let m = remote_motion((CENTRE.0 + 3, CENTRE.1 - 2), (1250, 540), CENTRE, true);
        assert!(m.absorbed, "must be recognised as our warp, not a 290px flick");
        assert_eq!((m.dx, m.dy), (0, 0));
    }

    /// If the warp arrives coalesced with motion it will not be near the
    /// centre, but no hand crosses that distance in one event either.
    #[test]
    fn a_warp_coalesced_with_motion_is_absorbed_by_size() {
        let m = remote_motion((700, 540), (1400, 540), CENTRE, true);
        assert!(m.absorbed);
    }

    /// A pending warp must not swallow ordinary movement that happens while it
    /// is still in flight.
    #[test]
    fn small_movement_while_a_warp_is_in_flight_is_kept() {
        let m = remote_motion((1255, 540), (1250, 540), CENTRE, true);
        assert!(!m.absorbed);
        assert_eq!(m.dx, 5, "not the 295px distance from centre");
    }

    /// Without a warp outstanding nothing may be discarded, however far the
    /// pointer appears to have travelled.
    #[test]
    fn a_large_move_is_never_absorbed_unless_a_warp_is_pending() {
        let m = moved(CENTRE, (1500, 540));
        assert!(!m.absorbed);
        assert_eq!(m.dx, -540);
    }

    #[test]
    fn a_small_screen_still_has_a_usable_margin() {
        let centre = (1, 1);
        assert!(!remote_motion((1, 1), (1, 1), centre, false).recenter);
    }
}
