//! Thin wrapper over arboard. One long-lived handle: on X11 the clipboard
//! contents we set would vanish if the handle were dropped.

use std::sync::Mutex;

use mousefinity_proto::MAX_CLIPBOARD;
use tracing::debug;

/// The platform clipboard is one process-wide resource, and the macOS backend
/// aborts the process outright when several threads touch NSPasteboard at
/// once. The daemon only ever holds one `Clip`, so this never contends in
/// production — but tests build an engine per thread, which is enough to hit
/// it.
static CLIPBOARD: Mutex<()> = Mutex::new(());

pub struct Clip {
    inner: Option<arboard::Clipboard>,
}

impl Clip {
    pub fn new() -> Self {
        let _guard = CLIPBOARD.lock().unwrap_or_else(|e| e.into_inner());
        let inner = match arboard::Clipboard::new() {
            Ok(c) => Some(c),
            Err(e) => {
                debug!("clipboard unavailable: {e}");
                None
            }
        };
        Self { inner }
    }

    pub fn get_text(&mut self) -> Option<String> {
        let _guard = CLIPBOARD.lock().unwrap_or_else(|e| e.into_inner());
        let c = self.inner.as_mut()?;
        match c.get_text() {
            Ok(t) if !t.is_empty() && t.len() <= MAX_CLIPBOARD => Some(t),
            _ => None,
        }
    }

    pub fn set_text(&mut self, text: &str) {
        let _guard = CLIPBOARD.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = self.inner.as_mut() {
            if let Err(e) = c.set_text(text.to_string()) {
                debug!("clipboard set failed: {e}");
            }
        }
    }
}
