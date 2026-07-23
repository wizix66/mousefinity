//! Global input capture via rdev's low-level grab hook.
//!
//! Runs on the main thread (a hard requirement on macOS). While the virtual
//! cursor is on a remote screen, all input is swallowed locally and forwarded
//! to the engine as deltas.
//!
//! Whether swallowing a move event leaves the pointer where it was is an OS
//! decision, and the two answers need opposite arithmetic:
//!
//! - **The pointer drifts** (macOS, X11). Consecutive hook events accumulate,
//!   so motion is the difference between them. Measuring from a fixed centre
//!   would re-report the whole accumulated offset on every event and send the
//!   remote cursor flying.
//! - **The pointer is pinned** (Windows: a low-level hook that swallows the
//!   event leaves the cursor in place, and the next event reports that same
//!   fixed origin plus its own movement). Differencing consecutive events then
//!   yields the *change* in speed — near zero for a steady drag — so the remote
//!   cursor jitters a few pixels around where it entered and can only leave by
//!   the edge it came in through. Motion here is the offset from the anchor.
//!
//! Which one this machine does is settled by measurement, not by `cfg`: once a
//! swallowed event reports a position away from the centre the hop warped the
//! pointer to, the injector is asked where the pointer actually is. Still on
//! that centre means suppression pins it; anywhere else means it drifts. The
//! answer holds for the process lifetime, and until it arrives the drifting
//! rule applies — the two agree on the first event after a hop, so the cost of
//! guessing wrong for a millisecond is a couple of jittered events.
//!
//! That measurement only works if the pointer really is at the centre, which
//! means the hop's warp must reach the OS. Our own warp is an injected move,
//! and injected moves re-enter this very hook; swallowing it the way every
//! other remote move is swallowed *blocks* it where suppression pins (Windows),
//! so the pointer never leaves the edge it entered by and everything measured
//! from the centre is measured from the wrong place. `warp_should_pass_through`
//! lets a pending warp through instead — until the pointer is known to drift,
//! where the warp lands regardless and swallowing resumes.
//!
//! While drifting, the pointer is warped back to the centre once it strays past
//! a quarter of the way to an edge — so it cannot reach one and go silent, and
//! so it does not visibly wander while parked. While pinned it never moves at
//! all, so none of that applies.
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

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU8, Ordering};
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
    /// What suppressing a move event does to the pointer here, as a
    /// [`Suppression`] discriminant. Measured once, then reused: it is a
    /// property of the OS, not of this hop.
    suppression: AtomicU8,
    /// A location probe has been asked for and not answered yet.
    probe_pending: AtomicBool,
    /// Where the pointer was known to be when that probe was requested. The
    /// probe answers the question by landing on this or not.
    probe_ax: AtomicI32,
    probe_ay: AtomicI32,
}

/// What swallowing a move event does to the pointer on this machine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Suppression {
    /// Not measured yet. Treated as [`Suppression::Drifts`] meanwhile, which
    /// is right for the first event after a hop either way.
    Unknown = 0,
    /// The pointer moves anyway, so hook events accumulate.
    Drifts = 1,
    /// The pointer stays put, so every hook event reports the same origin.
    Pins = 2,
}

impl Suppression {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Suppression::Drifts,
            2 => Suppression::Pins,
            _ => Suppression::Unknown,
        }
    }
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
        // An answer arriving after the hop ended would be compared against an
        // anchor the pointer has since been warped away from, and would latch
        // the wrong rule for the rest of the process.
        self.probe_pending.store(false, Ordering::Relaxed);
    }

    fn suppression(&self) -> Suppression {
        Suppression::from_u8(self.suppression.load(Ordering::Relaxed))
    }

    /// Whether a move event should be passed to the OS rather than swallowed,
    /// because it is (or coincides with) a warp we asked for and that warp has
    /// to actually relocate the pointer.
    ///
    /// Every other remote move is swallowed. On a machine where swallowing
    /// blocks the move outright (Windows), swallowing our *own* injected warp
    /// freezes the pointer wherever it was — never reaching the centre the hop
    /// warps it to — after which motion measured from that centre is nonsense
    /// and the remote cursor is stuck by the edge it entered by. Letting the
    /// warp through lands it. Where swallowing does not block the move (macOS,
    /// X11) the warp lands regardless, so once that is known this stops and the
    /// warp is swallowed like anything else, keeping stray physical moves off
    /// the local screen. Until it is known, the warp is let through — harmless
    /// where it would have landed anyway, and the one thing that works where it
    /// would not.
    fn warp_should_pass_through(&self) -> bool {
        self.warp_pending.load(Ordering::Relaxed) && self.suppression() != Suppression::Drifts
    }

    /// Ask, once, where the pointer really is. `anchor` is the last position
    /// the pointer is *known* to have occupied — the centre it was warped to,
    /// never a position merely reported by a hook event. Returns whether a
    /// probe should actually be sent.
    fn want_probe(&self, anchor: (i32, i32)) -> bool {
        if self.suppression() != Suppression::Unknown {
            return false;
        }
        if self.probe_pending.swap(true, Ordering::Relaxed) {
            return false;
        }
        self.probe_ax.store(anchor.0, Ordering::Relaxed);
        self.probe_ay.store(anchor.1, Ordering::Relaxed);
        true
    }

    /// The injector's answer: where the pointer actually is.
    ///
    /// Still on the anchor means the swallowed event moved nothing. Anywhere
    /// else means it moved — including further along than the event reported,
    /// which is why this asks "did it stay?" rather than "did it land where
    /// the event said?".
    pub fn note_pointer_location(&self, x: i32, y: i32) {
        if !self.probe_pending.swap(false, Ordering::Relaxed) {
            return;
        }
        let anchor = (
            self.probe_ax.load(Ordering::Relaxed),
            self.probe_ay.load(Ordering::Relaxed),
        );
        let mode = if (x, y) == anchor {
            Suppression::Pins
        } else {
            Suppression::Drifts
        };
        debug!(
            "suppressed motion leaves the pointer at {:?}; it was at {anchor:?}, so it {}",
            (x, y),
            match mode {
                Suppression::Pins => "pins — measuring motion from the anchor",
                _ => "drifts — measuring motion between events",
            }
        );
        self.suppression.store(mode as u8, Ordering::Relaxed);
    }

    /// Nothing can answer a probe, so stop asking and keep the drifting rule,
    /// which is what every release before this one used everywhere.
    pub fn note_probe_unavailable(&self) {
        self.probe_pending.store(false, Ordering::Relaxed);
        self.suppression
            .store(Suppression::Drifts as u8, Ordering::Relaxed);
    }

    /// Called by the inject thread once a warp has actually been performed.
    /// It is the only place that knows where the pointer really is, which is
    /// why the reference is set here rather than inferred in the hook.
    pub fn note_pointer_warped_to(&self, x: i32, y: i32) {
        self.lx.store(x, Ordering::Relaxed);
        self.ly.store(y, Ordering::Relaxed);
        self.warp_pending.store(false, Ordering::Relaxed);
    }

    /// No injector, so no warps will ever land: stop waiting for them. The
    /// same injector is what would answer a location probe, so that question
    /// cannot be asked either.
    pub fn injection_unavailable(&self) {
        self.can_warp.store(false, Ordering::Relaxed);
        self.warp_pending.store(false, Ordering::Relaxed);
        self.note_probe_unavailable();
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

/// Movement is the difference between consecutive events — unless suppressing
/// an event pins the pointer, in which case it is the offset from the anchor
/// the pointer is pinned to.
///
/// Where the pointer drifts, measuring from the centre is only correct if it is
/// warped back after every single event. It is not, so the offset keeps growing
/// and each event re-reports the whole accumulated distance; travel then grows
/// quadratically. Where the pointer is pinned the opposite holds: nothing
/// accumulates, so differencing consecutive events reports `dᵢ - dᵢ₋₁`, which
/// sums to nothing over a steady drag.
///
/// Rather than trying to work out *which* event was the warp — undecidable
/// from coordinates, since a warp landing slightly off is indistinguishable
/// from a fast flick — any step too large to be a hand is discarded. That
/// makes correctness independent of when the warp lands, or whether it
/// generates a hook event at all. The cost of a discarded event is a few
/// pixels of travel, once per warp; the cost of a wrong guess was the pointer
/// leaping across the far screen.
fn remote_motion(
    pos: (i32, i32),
    last: (i32, i32),
    centre: (i32, i32),
    mode: Suppression,
) -> Motion {
    // Pinned, the pointer never left the centre, so that is where this event's
    // movement is measured from — and there is nothing to warp back.
    let pinned = mode == Suppression::Pins;
    let reference = if pinned { centre } else { last };
    let dx = pos.0 - reference.0;
    let dy = pos.1 - reference.1;
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
        recenter: !pinned
            && ((pos.0 - centre.0).abs() > stray_x || (pos.1 - centre.1).abs() > stray_y),
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
                // A warp we asked for must reach the OS or the pointer never
                // moves to the centre; let it through untouched rather than
                // swallowing it like every other remote move. The reference is
                // resynced by the injector's `note_pointer_warped_to`, not
                // here, so nothing to update. The window this is true for is a
                // single synchronous hook call on Windows, so it does not leak
                // physical moves onto the local screen.
                if shared.warp_should_pass_through() {
                    return Some(event);
                }
                let centre = (
                    shared.cx.load(Ordering::Relaxed),
                    shared.cy.load(Ordering::Relaxed),
                );
                let last = (
                    shared.lx.load(Ordering::Relaxed),
                    shared.ly.load(Ordering::Relaxed),
                );
                let motion = remote_motion((x, y), last, centre, shared.suppression());
                // Always resync the reference, including on a discarded step:
                // the pointer really is here, whatever put it here. (Pinned,
                // it is not, and this is ignored.)
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
                // This event is about to be swallowed and it reports a
                // position away from the centre, so motion has happened since
                // the hop warped the pointer there: ask where the pointer
                // actually ended up. The centre is the anchor because it is
                // the last place the pointer is *known* to have been — the
                // previous event's coordinates are only where it was reported,
                // which on a pinning machine is not where it is. Asked once
                // per process, and only until an answer arrives.
                if (x, y) != centre && shared.want_probe(centre) {
                    send(LocalEvent::WhereIsPointer);
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
            let m = remote_motion(pos, last, CENTRE, Suppression::Drifts);
            assert_eq!(m.dy, 0);
            assert_eq!(m.dx, 10, "each event should report only its own movement");
            travelled += m.dx;
            last = pos;
        }
        assert_eq!(travelled, 100, "100px of hand movement, not 550");
    }

    #[test]
    fn motion_is_reported_in_both_directions() {
        let m = remote_motion((950, 530), (960, 540), CENTRE, Suppression::Drifts);
        assert_eq!((m.dx, m.dy), (-10, -10));
        assert!(!m.implausible);
    }

    #[test]
    fn a_still_pointer_reports_nothing() {
        let m = remote_motion(CENTRE, CENTRE, CENTRE, Suppression::Drifts);
        assert_eq!((m.dx, m.dy), (0, 0));
        assert!(!m.recenter && !m.implausible);
    }

    #[test]
    fn drifting_away_from_centre_asks_for_a_recentre() {
        assert!(!remote_motion((1100, 540), (1090, 540), CENTRE, Suppression::Drifts).recenter);
        assert!(remote_motion((1250, 540), (1240, 540), CENTRE, Suppression::Drifts).recenter);
        assert!(remote_motion((960, 300), (960, 310), CENTRE, Suppression::Drifts).recenter);
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
            let m = remote_motion(CENTRE, from, CENTRE, Suppression::Drifts);
            assert!(m.implausible, "warp from {from:?} must be discarded");
            assert_eq!((m.dx, m.dy), (0, 0));
        }
    }

    /// A warp that overshoots, lands short, or arrives coalesced with motion
    /// is still discarded — none of which the old coordinate matching caught.
    #[test]
    fn a_warp_landing_anywhere_near_centre_is_still_discarded() {
        for landing in [(957, 543), (960, 540), (975, 520), (940, 560)] {
            let m = remote_motion(landing, (1250, 540), CENTRE, Suppression::Drifts);
            assert!(m.implausible, "landing at {landing:?} must be discarded");
        }
    }

    #[test]
    fn a_hard_flick_is_still_forwarded() {
        let m = remote_motion(
            (CENTRE.0 + BIGGEST_ALLOWED_STEP, CENTRE.1),
            CENTRE,
            CENTRE,
            Suppression::Drifts,
        );
        assert!(
            !m.implausible,
            "a fast hand must not be mistaken for a warp"
        );
        assert_eq!(m.dx, BIGGEST_ALLOWED_STEP);
    }

    #[test]
    fn a_step_past_the_limit_is_discarded() {
        let m = remote_motion(
            (CENTRE.0 + BIGGEST_ALLOWED_STEP + 1, CENTRE.1),
            CENTRE,
            CENTRE,
            Suppression::Drifts,
        );
        assert!(m.implausible);
    }

    #[test]
    fn a_small_screen_still_has_a_usable_margin() {
        let centre = (1, 1);
        assert!(!remote_motion((1, 1), (1, 1), centre, Suppression::Drifts).recenter);
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

    /// The Windows bug: a swallowed event leaves the pointer at the centre, so
    /// every event reports centre + its own movement. Differencing those
    /// reports the *change* in speed, which sums to nothing over a steady drag
    /// — the remote cursor sat in a few pixels near the edge it came in
    /// through and could only go back the way it arrived.
    #[test]
    fn a_pinned_pointer_reports_each_step_not_the_change_in_speed() {
        let mut travelled = 0;
        for _ in 0..10 {
            // Same hand movement every time, so the pinned pointer reports the
            // same position every time.
            let pos = (CENTRE.0 + 10, CENTRE.1);
            let m = remote_motion(pos, pos, CENTRE, Suppression::Pins);
            assert_eq!(m.dx, 10, "each event must report its own movement");
            travelled += m.dx;
        }
        assert_eq!(
            travelled, 100,
            "100px of hand movement, not 10 then nothing"
        );
    }

    /// Nothing moved it, so there is nothing to move back — and warping would
    /// shift the very anchor that motion is measured from, turning the next
    /// hand movement into the warp distance plus itself.
    #[test]
    fn a_pinned_pointer_is_never_recentred() {
        let far = (CENTRE.0 + CENTRE.0 / RECENTRE_AT + 1, CENTRE.1);
        // Crept out there a pixel at a time, so the step itself is plausible.
        let last = (far.0 - 1, far.1);
        assert!(remote_motion(far, last, CENTRE, Suppression::Drifts).recenter);
        assert!(!remote_motion(far, last, CENTRE, Suppression::Pins).recenter);
    }

    /// Both rules agree on the first event after a hop, which is what makes it
    /// safe to keep measuring while the probe is still in flight.
    #[test]
    fn the_two_rules_agree_on_the_first_event_after_a_hop() {
        let pos = (CENTRE.0 + 7, CENTRE.1 - 3);
        let drifts = remote_motion(pos, CENTRE, CENTRE, Suppression::Drifts);
        let pins = remote_motion(pos, CENTRE, CENTRE, Suppression::Pins);
        assert_eq!((drifts.dx, drifts.dy), (pins.dx, pins.dy));
        assert_eq!(
            remote_motion(pos, CENTRE, CENTRE, Suppression::Unknown).dx,
            drifts.dx,
            "an unmeasured machine must behave as every release before this one did"
        );
    }

    #[test]
    fn a_pointer_still_on_the_anchor_means_suppression_pins_it() {
        let shared = CaptureShared::new();
        shared.set_remote(CENTRE);
        assert!(shared.want_probe(CENTRE));
        shared.note_pointer_location(CENTRE.0, CENTRE.1);
        assert_eq!(shared.suppression(), Suppression::Pins);
    }

    /// It need not have landed where the event said — under a suppressed drift
    /// it keeps moving while the probe is in flight — only somewhere else.
    #[test]
    fn a_pointer_that_moved_at_all_means_suppression_drifts() {
        for landed in [(CENTRE.0 + 10, CENTRE.1), (CENTRE.0 + 90, CENTRE.1)] {
            let shared = CaptureShared::new();
            shared.set_remote(CENTRE);
            assert!(shared.want_probe(CENTRE));
            shared.note_pointer_location(landed.0, landed.1);
            assert_eq!(shared.suppression(), Suppression::Drifts, "{landed:?}");
        }
    }

    /// The anchor must be somewhere the pointer was *known* to be. Anchoring
    /// on the previous event's coordinates instead looks right — until the
    /// probe is not the first event of the hop, at which point a pinned
    /// pointer sitting on the centre no longer matches the anchor and the
    /// drifting rule gets latched on exactly the machines this exists for.
    #[test]
    fn the_anchor_is_the_centre_and_not_wherever_the_last_event_claimed() {
        let shared = CaptureShared::new();
        shared.set_remote(CENTRE);
        // A couple of events have already gone by reporting motion.
        shared.lx.store(CENTRE.0 + 30, Ordering::Relaxed);
        shared.ly.store(CENTRE.1, Ordering::Relaxed);
        assert!(shared.want_probe(CENTRE));
        shared.note_pointer_location(CENTRE.0, CENTRE.1);
        assert_eq!(shared.suppression(), Suppression::Pins);
    }

    #[test]
    fn the_question_is_asked_once_and_then_never_again() {
        let shared = CaptureShared::new();
        shared.set_remote(CENTRE);
        assert!(shared.want_probe(CENTRE));
        assert!(!shared.want_probe(CENTRE), "one probe in flight at a time");
        shared.note_pointer_location(CENTRE.0, CENTRE.1);
        assert!(!shared.want_probe(CENTRE), "the answer does not expire");
    }

    /// A probe answered after the hop ended would be compared against an
    /// anchor the pointer has since been warped away from, latching `Drifts`
    /// on a machine that pins.
    #[test]
    fn an_answer_arriving_after_the_hop_ended_is_ignored() {
        let shared = CaptureShared::new();
        shared.set_remote(CENTRE);
        assert!(shared.want_probe(CENTRE));
        shared.set_local();
        shared.note_pointer_location(1, 1);
        assert_eq!(shared.suppression(), Suppression::Unknown);
    }

    /// Nothing can answer, so the question must not be left open: an
    /// unanswered probe blocks every later one.
    #[test]
    fn without_an_injector_the_drifting_rule_is_settled_on() {
        let shared = CaptureShared::new();
        shared.injection_unavailable();
        shared.set_remote(CENTRE);
        assert_eq!(shared.suppression(), Suppression::Drifts);
        assert!(!shared.want_probe(CENTRE));
    }

    /// The missing half of the fix: our own warp has to reach the OS. On a
    /// machine that blocks a swallowed move it would otherwise be swallowed
    /// like any other remote move, the pointer would never leave the edge for
    /// the centre, and measuring from that centre would be measuring from a
    /// place the pointer is not.
    #[test]
    fn a_pending_warp_is_let_through_until_the_pointer_is_known_to_drift() {
        let shared = CaptureShared::new();
        shared.set_remote(CENTRE);
        // Hop just happened: a warp is in flight and nothing is measured yet.
        assert_eq!(shared.suppression(), Suppression::Unknown);
        assert!(
            shared.warp_should_pass_through(),
            "before the OS is known, the warp must be let through — harmless \
             where it would land anyway, essential where it would not"
        );
    }

    #[test]
    fn a_pending_warp_is_let_through_on_a_pinning_machine() {
        let shared = CaptureShared::new();
        shared
            .suppression
            .store(Suppression::Pins as u8, Ordering::Relaxed);
        shared.set_remote(CENTRE);
        assert!(shared.warp_should_pass_through());
    }

    /// Where swallowing does not block the move, the warp lands regardless, so
    /// it is swallowed like everything else — otherwise stray physical moves
    /// during the warp would leak onto the local screen.
    #[test]
    fn a_pending_warp_is_swallowed_once_the_pointer_is_known_to_drift() {
        let shared = CaptureShared::new();
        shared
            .suppression
            .store(Suppression::Drifts as u8, Ordering::Relaxed);
        shared.set_remote(CENTRE);
        assert!(!shared.warp_should_pass_through());
    }

    #[test]
    fn nothing_is_let_through_when_no_warp_is_pending() {
        let shared = CaptureShared::new();
        shared.set_remote(CENTRE);
        shared.note_pointer_warped_to(CENTRE.0, CENTRE.1);
        assert!(!shared.warp_pending.load(Ordering::Relaxed));
        assert!(!shared.warp_should_pass_through());
    }
}
