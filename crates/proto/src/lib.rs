//! Wire protocol and screen-layout model for mousefinity.
//!
//! This crate is intentionally free of any platform input/GUI dependencies so
//! it can be built for every supported target, including Android and iOS.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Protocol version. Bump on incompatible changes; peers refuse mismatches.
pub const PROTO_VERSION: u16 = 1;

/// ALPN for the long-lived control/input connection between paired hosts.
pub const ALPN_CONTROL: &[u8] = b"mousefinity/ctl/1";
/// ALPN for one-shot file transfer connections.
pub const ALPN_FILE: &[u8] = b"mousefinity/file/1";

/// Hard cap for a single framed message (clipboard is chunk-limited below this).
pub const MAX_FRAME: u32 = 8 * 1024 * 1024;

/// Maximum clipboard payload we are willing to sync (4 MiB).
pub const MAX_CLIPBOARD: usize = 4 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame of {0} bytes exceeds limit")]
    FrameTooLarge(u32),
    #[error("encode/decode error: {0}")]
    Codec(#[from] postcard::Error),
}

/// Mouse buttons, platform-neutral.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
    Other(u8),
}

/// Platform-neutral key identifiers (superset of the usual desktop sets).
///
/// Letters and digits refer to *physical* keys in US-QWERTY positions; the
/// receiving side decides how to inject them. `Unicode` carries a resolved
/// character when the sender knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Key {
    Alt,
    AltGr,
    Backspace,
    CapsLock,
    ControlLeft,
    ControlRight,
    Delete,
    DownArrow,
    End,
    Escape,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    Home,
    LeftArrow,
    MetaLeft,
    MetaRight,
    PageDown,
    PageUp,
    Return,
    RightArrow,
    ShiftLeft,
    ShiftRight,
    Space,
    Tab,
    UpArrow,
    PrintScreen,
    ScrollLock,
    Pause,
    NumLock,
    Insert,
    KeyA,
    KeyB,
    KeyC,
    KeyD,
    KeyE,
    KeyF,
    KeyG,
    KeyH,
    KeyI,
    KeyJ,
    KeyK,
    KeyL,
    KeyM,
    KeyN,
    KeyO,
    KeyP,
    KeyQ,
    KeyR,
    KeyS,
    KeyT,
    KeyU,
    KeyV,
    KeyW,
    KeyX,
    KeyY,
    KeyZ,
    Num0,
    Num1,
    Num2,
    Num3,
    Num4,
    Num5,
    Num6,
    Num7,
    Num8,
    Num9,
    BackQuote,
    Minus,
    Equal,
    LeftBracket,
    RightBracket,
    BackSlash,
    SemiColon,
    Quote,
    Comma,
    Dot,
    Slash,
    IntlBackslash,
    Kp0,
    Kp1,
    Kp2,
    Kp3,
    Kp4,
    Kp5,
    Kp6,
    Kp7,
    Kp8,
    Kp9,
    KpMinus,
    KpPlus,
    KpMultiply,
    KpDivide,
    KpReturn,
    KpDelete,
    Function,
    /// A key we could not classify; raw platform scancode for diagnostics.
    Unknown(u32),
}

/// Messages on the control connection. Framed as `u32-le length ++ postcard`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Msg {
    /// First message in both directions after the connection opens.
    Hello {
        version: u16,
        name: String,
        /// Width/height of the sender's screen in pixels.
        screen: (u32, u32),
    },
    /// The virtual cursor entered your screen at (x, y). You are now focused.
    Enter { x: i32, y: i32 },
    /// The virtual cursor left your screen. Reply with `Clipboard` if you
    /// have content newer than what you last received.
    Leave,
    /// Absolute cursor position on your screen.
    MouseMove { x: i32, y: i32 },
    Button { button: Button, down: bool },
    Wheel { dx: i32, dy: i32 },
    Key { key: Key, down: bool },
    /// Clipboard sync. UTF-8 text only for now.
    Clipboard { text: String },
    /// Screen size changed (e.g. resolution switch).
    Screen { screen: (u32, u32) },
}

/// Header for a file transfer connection (`ALPN_FILE`). Sent framed, then the
/// raw file bytes follow on the same stream until FIN.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileOffer {
    pub name: String,
    pub size: u64,
}

/// Write one framed message.
pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> Result<(), ProtoError>
where
    W: AsyncWriteExt + Unpin,
    T: Serialize,
{
    let bytes = postcard::to_stdvec(msg)?;
    let len = bytes.len() as u32;
    if len > MAX_FRAME {
        return Err(ProtoError::FrameTooLarge(len));
    }
    w.write_all(&len.to_le_bytes()).await?;
    w.write_all(&bytes).await?;
    Ok(())
}

/// Read one framed message.
pub async fn read_frame<R, T>(r: &mut R) -> Result<T, ProtoError>
where
    R: AsyncReadExt + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME {
        return Err(ProtoError::FrameTooLarge(len));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    Ok(postcard::from_bytes(&buf)?)
}

/// Which edge of a screen was crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Edge {
    Left,
    Right,
    Up,
    Down,
}

/// Neighbours of one screen in the virtual arrangement.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Neighbors {
    pub left: Option<String>,
    pub right: Option<String>,
    pub up: Option<String>,
    pub down: Option<String>,
}

impl Neighbors {
    pub fn get(&self, edge: Edge) -> Option<&str> {
        match edge {
            Edge::Left => self.left.as_deref(),
            Edge::Right => self.right.as_deref(),
            Edge::Up => self.up.as_deref(),
            Edge::Down => self.down.as_deref(),
        }
    }
}

/// The virtual arrangement of screens, keyed by host name.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Layout(pub std::collections::BTreeMap<String, Neighbors>);

impl Layout {
    pub fn neighbor(&self, screen: &str, edge: Edge) -> Option<&str> {
        self.0.get(screen).and_then(|n| n.get(edge))
    }
}

/// Given a cursor position on `from` (size `from_size`), determine whether it
/// crossed an edge, and if so where it enters the neighbour screen of size
/// `to_size`. The perpendicular coordinate is scaled proportionally so hops
/// feel natural between screens of different resolutions.
pub fn crossing(x: i32, y: i32, from_size: (u32, u32)) -> Option<Edge> {
    let (w, h) = (from_size.0 as i32, from_size.1 as i32);
    if x < 0 {
        Some(Edge::Left)
    } else if x >= w {
        Some(Edge::Right)
    } else if y < 0 {
        Some(Edge::Up)
    } else if y >= h {
        Some(Edge::Down)
    } else {
        None
    }
}

/// Entry position on the destination screen after crossing `edge`.
pub fn entry_pos(edge: Edge, x: i32, y: i32, from: (u32, u32), to: (u32, u32)) -> (i32, i32) {
    let scale = |v: i32, from_dim: u32, to_dim: u32| -> i32 {
        if from_dim == 0 {
            return 0;
        }
        let f = v.clamp(0, from_dim as i32 - 1) as f64 / from_dim as f64;
        ((f * to_dim as f64) as i32).clamp(0, to_dim as i32 - 1)
    };
    match edge {
        Edge::Left => (to.0 as i32 - 1, scale(y, from.1, to.1)),
        Edge::Right => (0, scale(y, from.1, to.1)),
        Edge::Up => (scale(x, from.0, to.0), to.1 as i32 - 1),
        Edge::Down => (scale(x, from.0, to.0), 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut buf = Vec::new();
            let msg = Msg::MouseMove { x: -5, y: 900 };
            write_frame(&mut buf, &msg).await.unwrap();
            let mut cursor = std::io::Cursor::new(buf);
            let got: Msg = read_frame(&mut cursor).await.unwrap();
            match got {
                Msg::MouseMove { x: -5, y: 900 } => {}
                other => panic!("bad roundtrip: {other:?}"),
            }
        });
    }

    #[test]
    fn edge_crossing() {
        assert_eq!(crossing(-1, 10, (1920, 1080)), Some(Edge::Left));
        assert_eq!(crossing(1920, 10, (1920, 1080)), Some(Edge::Right));
        assert_eq!(crossing(5, -3, (1920, 1080)), Some(Edge::Up));
        assert_eq!(crossing(5, 1080, (1920, 1080)), Some(Edge::Down));
        assert_eq!(crossing(5, 5, (1920, 1080)), None);
    }

    #[test]
    fn entry_scaling() {
        // Crossing right edge of a 1920x1080 screen into a 3840x2160 screen
        // at half height lands at half height of the destination.
        let (x, y) = entry_pos(Edge::Right, 1920, 540, (1920, 1080), (3840, 2160));
        assert_eq!(x, 0);
        assert_eq!(y, 1080);
    }
}
