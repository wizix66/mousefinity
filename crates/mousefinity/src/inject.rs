//! Input injection thread. Owns the enigo handle; other threads send commands.

use std::sync::Arc;

use anyhow::Result;
use enigo::{Axis, Coordinate, Direction, Keyboard, Mouse};
use mousefinity_proto::{Button, Key};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::capture::CaptureShared;
use crate::keymap;

#[derive(Debug)]
pub enum InjectCmd {
    MoveAbs { x: i32, y: i32 },
    Button { button: Button, down: bool },
    Key { key: Key, down: bool },
    Wheel { dx: i32, dy: i32 },
    /// Report the pointer's real position back to capture. This thread is the
    /// only one holding something that can be asked.
    WhereIsPointer,
}

pub fn spawn(shared: Arc<CaptureShared>) -> Result<mpsc::UnboundedSender<InjectCmd>> {
    let (tx, mut rx) = mpsc::unbounded_channel::<InjectCmd>();
    std::thread::Builder::new()
        .name("inject".into())
        .spawn(move || {
            let mut enigo = match enigo::Enigo::new(&enigo::Settings::default()) {
                Ok(e) => e,
                Err(e) => {
                    warn!("input injection unavailable: {e}");
                    // Capture must not wait for warps that will never happen,
                    // or remote motion would stall the first time it asks.
                    shared.injection_unavailable();
                    return;
                }
            };
            while let Some(cmd) = rx.blocking_recv() {
                if matches!(cmd, InjectCmd::WhereIsPointer) {
                    match enigo.location() {
                        Ok((x, y)) => shared.note_pointer_location(x, y),
                        Err(e) => {
                            // Unanswerable, so capture must stop waiting for
                            // an answer rather than measure motion by a rule
                            // it never got to choose.
                            debug!("cannot read pointer location: {e}");
                            shared.note_probe_unavailable();
                        }
                    }
                    continue;
                }
                let warped_to = match cmd {
                    InjectCmd::MoveAbs { x, y } => Some((x, y)),
                    _ => None,
                };
                let r = apply(&mut enigo, cmd);
                if let Err(e) = r {
                    debug!("inject failed: {e}");
                }
                // Only this thread knows when the pointer actually moved.
                // Telling capture where it now is beats letting capture infer
                // it from coordinates, which cannot reliably tell our own warp
                // from a fast flick and turns the difference into a jump.
                if let Some((x, y)) = warped_to {
                    shared.note_pointer_warped_to(x, y);
                }
            }
        })?;
    Ok(tx)
}

fn apply(enigo: &mut enigo::Enigo, cmd: InjectCmd) -> Result<(), enigo::InputError> {
    match cmd {
        // Answered in the loop above, which is the only place that can reach
        // capture with the result.
        InjectCmd::WhereIsPointer => Ok(()),
        InjectCmd::MoveAbs { x, y } => enigo.move_mouse(x, y, Coordinate::Abs),
        InjectCmd::Button { button, down } => {
            let Some(b) = keymap::enigo_button(button) else {
                return Ok(());
            };
            let dir = if down {
                Direction::Press
            } else {
                Direction::Release
            };
            enigo.button(b, dir)
        }
        InjectCmd::Key { key, down } => {
            let Some(k) = keymap::enigo_key(key) else {
                return Ok(());
            };
            let dir = if down {
                Direction::Press
            } else {
                Direction::Release
            };
            enigo.key(k, dir)
        }
        InjectCmd::Wheel { dx, dy } => {
            // rdev reports wheel-up as positive; enigo scrolls down for
            // positive values, so flip the vertical axis.
            if dy != 0 {
                enigo.scroll(-dy, Axis::Vertical)?;
            }
            if dx != 0 {
                enigo.scroll(dx, Axis::Horizontal)?;
            }
            Ok(())
        }
    }
}
