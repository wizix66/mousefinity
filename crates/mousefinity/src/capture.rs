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
//! not visibly wander while parked.
//!
//! A warp breaks the difference between consecutive events, and which event
//! *was* the warp cannot be decided from coordinates: one landing slightly off
//! is indistinguishable from a fast flick. Two attempts at telling them apart
//! both leaked the teleport distance to the far screen as a jump, so the
//! question is no longer asked. Any step too large for a hand is discarded
//! instead, which is correct whenever the warp lands, whether or not it
//! produces a hook event of its own, and even if something outside this
//! program moves the pointer. The injector still reports where it put the
//! pointer, which keeps the number of discarded events near zero, but nothing
//! depends on that report arriving.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error, warn};

use crate::engine::{EngineIn, LocalEvent};
use crate::keymap;

#[derive(Default)]
pub struct CaptureShared {
    /// True while the virtual cursor is on a remote screen.
    remote: AtomicBool,
    /// A warp has been asked for and has not landed yet. Motion is held
    /// rather than guessed at until the injector confirms where the pointer
    /// ended up.
    warp_pending: AtomicBool,
    /// How many implausible steps have been discarded. Diagnostic only: a
    /// count far above the number of recentres means something other than our
    /// own warp is moving the pointer.
    discarded: AtomicU32,
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
        self.discarded.store(0, Ordering::Relaxed);
        // The hop warps the pointer to the centre; that warp is in flight.
        self.warp_pending
            .store(self.can_warp.load(Ordering::Relaxed), Ordering::Relaxed);
        self.remote.store(true, Ordering::SeqCst);
    }

    pub fn set_local(&self) {
        self.remote.store(false, Ordering::SeqCst);
        self.warp_pending.store(false, Ordering::Relaxed);
    }

    /// Called by the inject thread once a warp has actually been performed.
    /// It is the only place that knows where the pointer really is, which is
    /// why the reference is set here rather than inferred in the hook.
    pub fn note_pointer_warped_to(&self, x: i32, y: i32) {
        self.lx.store(x, Ordering::Relaxed);
        self.ly.store(y, Ordering::Relaxed);
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
    /// The step was too large to have come from a hand. Nothing is forwarded;
    /// the reference is simply resynced to here.
    implausible: bool,
}

/// Largest step, as a fraction of the distance from centre to edge, that a
/// single hook event is allowed to describe.
///
/// A warp to the centre always crosses more than [`RECENTRE_AT`] of that
/// distance, so it lands outside this bound; a hand does not. Between two
/// events even a hard flick moves tens of pixels, while this permits ~160 on a
/// 1920-wide screen — around 20,000 px/s at a 125Hz report rate.
const MAX_STEP: i32 = 6;
/// How far the pointer may stray from centre before it is warped back, as the
/// same kind of fraction. Must be a *smaller* divisor than [`MAX_STEP`] — and
/// so a longer distance — otherwise a warp would be short enough to pass as
/// real movement and would reach the far screen as a jump.
const RECENTRE_AT: i32 = 4;

/// Movement is the difference between consecutive events, never the offset
/// from the centre.
///
/// Measuring from the centre is only correct if the pointer is warped back
/// after every single event. It is not, so the offset keeps growing and each
/// event re-reports the whole accumulated distance; travel then grows
/// quadratically.
///
/// Rather than trying to work out *which* event was the warp — undecidable
/// from coordinates, since a warp landing slightly off is indistinguishable
/// from a fast flick — any step too large to be a hand is discarded. That
/// makes correctness independent of when the warp lands, or whether it
/// generates a hook event at all. The cost of a discarded event is a few
/// pixels of travel, once per warp; the cost of a wrong guess was the pointer
/// leaping across the far screen.
fn remote_motion(pos: (i32, i32), last: (i32, i32), centre: (i32, i32)) -> Motion {
    let dx = pos.0 - last.0;
    let dy = pos.1 - last.1;
    // Keep the pointer near the middle of the screen: it is visible the whole
    // time it is parked, and one wandering off on its own looks broken even
    // when the remote cursor is behaving.
    let stray_x = (centre.0 / RECENTRE_AT).max(1);
    let stray_y = (centre.1 / RECENTRE_AT).max(1);
    let max_x = (centre.0 / MAX_STEP).max(1);
    let max_y = (centre.1 / MAX_STEP).max(1);
    if dx.abs() > max_x || dy.abs() > max_y {
        return Motion {
            dx: 0,
            dy: 0,
            recenter: false,
            implausible: true,
        };
    }
    Motion {
        dx,
        dy,
        recenter: (pos.0 - centre.0).abs() > stray_x || (pos.1 - centre.1).abs() > stray_y,
        implausible: false,
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
                let motion = remote_motion((x, y), last, centre);
                // Always resync the reference, including on a discarded step:
                // the pointer really is here, whatever put it here.
                shared.lx.store(x, Ordering::Relaxed);
                shared.ly.store(y, Ordering::Relaxed);
                if motion.implausible {
                    // Expected once per warp. Anything else means the pointer
                    // is being moved by something we did not do, and if jumps
                    // are still visible this counter says whether they came
                    // from here at all.
                    let n = shared.discarded.fetch_add(1, Ordering::Relaxed) + 1;
                    if n % 64 == 1 {
                        debug!(
                            "discarded an implausible pointer step to ({x},{y}) from {last:?} \
                             (that is {n} so far; expected roughly one per recentre)"
                        );
                    }
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
    /// Matches (centre.0 / MAX_STEP) for CENTRE above.
    const BIGGEST_ALLOWED_STEP: i32 = 160;

    /// The first regression: with deltas measured from the centre, dragging
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
        assert!(!m.implausible);
    }

    #[test]
    fn a_still_pointer_reports_nothing() {
        let m = remote_motion(CENTRE, CENTRE, CENTRE);
        assert_eq!((m.dx, m.dy), (0, 0));
        assert!(!m.recenter && !m.implausible);
    }

    #[test]
    fn drifting_away_from_centre_asks_for_a_recentre() {
        assert!(!remote_motion((1100, 540), (1090, 540), CENTRE).recenter);
        assert!(remote_motion((1250, 540), (1240, 540), CENTRE).recenter);
        assert!(remote_motion((960, 300), (960, 310), CENTRE).recenter);
    }

    /// The jump, whichever way round it happens: a warp is always longer than
    /// any step a hand is allowed, so it can never reach the far screen.
    #[test]
    fn a_warp_is_always_longer_than_the_largest_allowed_step() {
        let stray = CENTRE.0 / RECENTRE_AT;
        let biggest_step = CENTRE.0 / MAX_STEP;
        assert!(
            stray > biggest_step,
            "a warp of {stray}px must exceed the {biggest_step}px step limit"
        );
        // Warping in from the recentre threshold, in either direction.
        for from in [(CENTRE.0 + stray, CENTRE.1), (CENTRE.0 - stray, CENTRE.1)] {
            let m = remote_motion(CENTRE, from, CENTRE);
            assert!(m.implausible, "warp from {from:?} must be discarded");
            assert_eq!((m.dx, m.dy), (0, 0));
        }
    }

    /// A warp that overshoots, lands short, or arrives coalesced with motion
    /// is still discarded — none of which the old coordinate matching caught.
    #[test]
    fn a_warp_landing_anywhere_near_centre_is_still_discarded() {
        for landing in [(957, 543), (960, 540), (975, 520), (940, 560)] {
            let m = remote_motion(landing, (1250, 540), CENTRE);
            assert!(m.implausible, "landing at {landing:?} must be discarded");
        }
    }

    #[test]
    fn a_hard_flick_is_still_forwarded() {
        let m = remote_motion(
            (CENTRE.0 + BIGGEST_ALLOWED_STEP, CENTRE.1),
            CENTRE,
            CENTRE,
        );
        assert!(!m.implausible, "a fast hand must not be mistaken for a warp");
        assert_eq!(m.dx, BIGGEST_ALLOWED_STEP);
    }

    #[test]
    fn a_step_past_the_limit_is_discarded() {
        let m = remote_motion(
            (CENTRE.0 + BIGGEST_ALLOWED_STEP + 1, CENTRE.1),
            CENTRE,
            CENTRE,
        );
        assert!(m.implausible);
    }

    #[test]
    fn a_small_screen_still_has_a_usable_margin() {
        let centre = (1, 1);
        assert!(!remote_motion((1, 1), (1, 1), centre).recenter);
    }

    /// With no injector there is nothing to warp, so hopping must not leave
    /// the host unable to move the remote cursor at all.
    #[test]
    fn without_an_injector_no_warp_is_awaited() {
        let shared = CaptureShared::new();
        shared.injection_unavailable();
        shared.set_remote(CENTRE);
        assert!(!shared.warp_pending.load(Ordering::Relaxed));
    }

    #[test]
    fn the_injector_resyncs_the_reference_where_it_landed() {
        let shared = CaptureShared::new();
        shared.set_remote(CENTRE);
        shared.note_pointer_warped_to(CENTRE.0, CENTRE.1);
        assert!(!shared.warp_pending.load(Ordering::Relaxed));
        assert_eq!(shared.lx.load(Ordering::Relaxed), CENTRE.0);
        assert_eq!(shared.ly.load(Ordering::Relaxed), CENTRE.1);
    }
}
