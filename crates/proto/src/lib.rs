//! Wire protocol and screen-layout model for mousefinity.
//!
//! This crate is intentionally free of any platform input/GUI dependencies so
//! it can be built for every supported target, including Android and iOS.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Protocol version. Bump on incompatible changes; peers refuse mismatches.
///
/// v3: `Msg::Layout` is keyed by endpoint id rather than host name.
pub const PROTO_VERSION: u16 = 3;

/// ALPN for the long-lived control/input connection between paired hosts.
pub const ALPN_CONTROL: &[u8] = b"mousefinity/ctl/1";
/// ALPN for one-shot file transfer connections.
pub const ALPN_FILE: &[u8] = b"mousefinity/file/1";
/// ALPN for one-shot mesh join handshakes (token-authenticated pairing).
pub const ALPN_JOIN: &[u8] = b"mousefinity/join/1";

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
    /// Layout sync: the sender's screen arrangement and its revision
    /// (milliseconds since the Unix epoch of the last local edit). Receivers
    /// adopt strictly newer revisions, persist them, and gossip them onward,
    /// so a `link`/TUI edit on any machine reaches every connected peer.
    ///
    /// On the wire the [`Layout`] is keyed by **endpoint id**, not host name:
    /// two hosts may call the same machine different things locally, and a
    /// name-keyed layout would make those aliases collide into phantom extra
    /// screens. The daemon translates to and from local names at the network
    /// boundary, so each host keeps its own naming.
    Layout { rev: u64, layout: Layout },
    /// Mesh roster gossip: every member the sender knows about. Receivers
    /// take the union, persist newcomers as trusted peers, and re-gossip on
    /// change, so a joining machine becomes reachable mesh-wide. Only sent
    /// between hosts that share a mesh token.
    Roster { members: Vec<Member> },
}

/// One mesh member: layout/config name plus its pairing id (hex).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub name: String,
    pub id: String,
}

/// First frame on an `ALPN_JOIN` connection.
///
/// `proof` must be the keyed hash of both connection endpoints' ids under
/// the mesh secret (see the `mesh` module in the daemon), which shows the
/// dialer holds the mesh token without revealing it; the underlying QUIC
/// connection already authenticates both ids, so the proof cannot be
/// replayed for any other pair of machines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinRequest {
    /// Public identifier of the mesh (hash of the secret).
    pub mesh_id: [u8; 32],
    /// Keyed hash binding the token to this connection's two endpoint ids.
    pub proof: [u8; 32],
    /// Who is joining.
    pub member: Member,
}

/// Reply to a [`JoinRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum JoinResponse {
    /// Wrong mesh, bad proof, or a name conflict; the reason is human-readable.
    Denied { reason: String },
    /// Joined: the full roster as the accepting member knows it.
    Welcome { members: Vec<Member> },
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

impl Edge {
    pub fn opposite(self) -> Edge {
        match self {
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
            Edge::Up => Edge::Down,
            Edge::Down => Edge::Up,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Edge::Left => "left",
            Edge::Right => "right",
            Edge::Up => "up",
            Edge::Down => "down",
        }
    }
}

/// Neighbours of one screen in the virtual arrangement.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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

    pub fn get_mut(&mut self, edge: Edge) -> &mut Option<String> {
        match edge {
            Edge::Left => &mut self.left,
            Edge::Right => &mut self.right,
            Edge::Up => &mut self.up,
            Edge::Down => &mut self.down,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.left.is_none() && self.right.is_none() && self.up.is_none() && self.down.is_none()
    }
}

/// The virtual arrangement of screens.
///
/// The key is a *screen reference*, whose meaning depends on where the layout
/// lives: on disk and inside the daemon it is a local host name, on the wire
/// it is an endpoint id (see [`Msg::Layout`]). A reference that a host has no
/// name for stays a raw id locally, so layouts survive gossip through hosts
/// that have not paired with every machine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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

    /// Entry is proportional along the shared edge, so the cursor comes out
    /// where it went in regardless of how differently the screens are shaped.
    #[test]
    fn landscape_to_portrait_keeps_the_relative_position() {
        // Leaving a 1920x1080 screen 25% down its right edge...
        let (x, y) = entry_pos(Edge::Right, 1920, 270, (1920, 1080), (1080, 1920));
        // ...arrives 25% down a portrait 1080x1920 screen.
        assert_eq!(x, 0);
        assert_eq!(y, 480);
    }

    #[test]
    fn portrait_to_landscape_keeps_the_relative_position() {
        let (x, y) = entry_pos(Edge::Left, 0, 1440, (1080, 1920), (1920, 1080));
        assert_eq!(x, 1919, "enters at the far edge of the destination");
        assert_eq!(y, 810, "75% down the portrait screen is 75% down the wide one");
    }

    /// A 5K screen next to a small laptop is the worst case for a naive
    /// mapping: without scaling, the bottom half of the tall screen would be
    /// unreachable from the short one.
    #[test]
    fn extreme_resolution_differences_still_span_the_whole_edge() {
        let small = (1280u32, 800u32);
        let huge = (5120u32, 2880u32);
        let top = entry_pos(Edge::Right, 1280, 0, small, huge);
        let bottom = entry_pos(Edge::Right, 1280, 799, small, huge);
        assert_eq!(top.1, 0);
        assert!(
            bottom.1 >= 2875,
            "the bottom of the small screen must reach the bottom of the big one, got {}",
            bottom.1
        );
    }

    /// Vertical edges scale along x, and the entry point is always inside the
    /// destination so arriving cannot immediately re-trigger a crossing.
    #[test]
    fn stacked_screens_scale_along_the_other_axis() {
        let (x, y) = entry_pos(Edge::Down, 960, 1080, (1920, 1080), (3840, 2160));
        assert_eq!(x, 1920);
        assert_eq!(y, 0);
        let (x, y) = entry_pos(Edge::Up, 960, 0, (1920, 1080), (3840, 2160));
        assert_eq!(x, 1920);
        assert_eq!(y, 2159);
    }

    /// Every entry point must land inside the destination, whatever the
    /// shapes involved — an out-of-bounds entry would hop straight back.
    #[test]
    fn entry_is_always_within_the_destination() {
        let shapes = [(1920u32, 1080u32), (1080, 1920), (1280, 800), (5120, 2880)];
        for from in shapes {
            for to in shapes {
                for edge in [Edge::Left, Edge::Right, Edge::Up, Edge::Down] {
                    for (x, y) in [(0, 0), (from.0 as i32 - 1, from.1 as i32 - 1)] {
                        let (ex, ey) = entry_pos(edge, x, y, from, to);
                        assert!(
                            ex >= 0 && ex < to.0 as i32 && ey >= 0 && ey < to.1 as i32,
                            "{edge:?} from {from:?} at ({x},{y}) into {to:?} gave ({ex},{ey})"
                        );
                    }
                }
            }
        }
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
