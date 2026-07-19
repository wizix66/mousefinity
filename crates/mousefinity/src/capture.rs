//! Global input capture via rdev's low-level grab hook.
//!
//! Runs on the main thread (a hard requirement on macOS). While the virtual
//! cursor is on a remote screen, all input is swallowed locally and forwarded
//! to the engine; the physical cursor stays parked at the screen centre so
//! every subsequent hook event yields a clean per-event delta.

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
}

impl CaptureShared {
    pub fn set_remote(&self, center: (i32, i32)) {
        self.cx.store(center.0, Ordering::Relaxed);
        self.cy.store(center.1, Ordering::Relaxed);
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
                let cx = shared.cx.load(Ordering::Relaxed);
                let cy = shared.cy.load(Ordering::Relaxed);
                if shared.warp_pending.load(Ordering::Relaxed) && x == cx && y == cy {
                    // Our own warp-to-centre: let it through so the OS cursor
                    // actually moves, and do not treat it as user motion.
                    shared.warp_pending.store(false, Ordering::Relaxed);
                    return Some(event);
                }
                let (dx, dy) = (x - cx, y - cy);
                if dx != 0 || dy != 0 {
                    send(LocalEvent::Delta { dx, dy });
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
