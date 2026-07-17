//! Thin wrapper over arboard. One long-lived handle: on X11 the clipboard
//! contents we set would vanish if the handle were dropped.

use mousefinity_proto::MAX_CLIPBOARD;
use tracing::debug;

pub struct Clip {
    inner: Option<arboard::Clipboard>,
}

impl Clip {
    pub fn new() -> Self {
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
        let c = self.inner.as_mut()?;
        match c.get_text() {
            Ok(t) if !t.is_empty() && t.len() <= MAX_CLIPBOARD => Some(t),
            _ => None,
        }
    }

    pub fn set_text(&mut self, text: &str) {
        if let Some(c) = self.inner.as_mut() {
            if let Err(e) = c.set_text(text.to_string()) {
                debug!("clipboard set failed: {e}");
            }
        }
    }
}
