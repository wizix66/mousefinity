//! Conversions between rdev (capture), the wire protocol, and enigo (inject).

use mousefinity_proto::{Button, Key};

macro_rules! mirror {
    ($($v:ident),* $(,)?) => {
        pub fn rdev_to_proto(k: rdev::Key) -> Key {
            match k {
                $(rdev::Key::$v => Key::$v,)*
                rdev::Key::Unknown(c) => Key::Unknown(c),
            }
        }
    };
}

// rdev 0.5.3's Key set, mirrored 1:1 into the wire enum.
mirror!(
    Alt, AltGr, Backspace, CapsLock, ControlLeft, ControlRight, Delete, DownArrow, End, Escape,
    F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12, Home, LeftArrow, MetaLeft, MetaRight,
    PageDown, PageUp, Return, RightArrow, ShiftLeft, ShiftRight, Space, Tab, UpArrow, PrintScreen,
    ScrollLock, Pause, NumLock, Insert, KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI,
    KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO, KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU, KeyV, KeyW, KeyX,
    KeyY, KeyZ, Num0, Num1, Num2, Num3, Num4, Num5, Num6, Num7, Num8, Num9, BackQuote, Minus,
    Equal, LeftBracket, RightBracket, BackSlash, SemiColon, Quote, Comma, Dot, Slash,
    IntlBackslash, Kp0, Kp1, Kp2, Kp3, Kp4, Kp5, Kp6, Kp7, Kp8, Kp9, KpMinus, KpPlus, KpMultiply,
    KpDivide, KpReturn, KpDelete, Function,
);

pub fn rdev_button(b: rdev::Button) -> Button {
    match b {
        rdev::Button::Left => Button::Left,
        rdev::Button::Right => Button::Right,
        rdev::Button::Middle => Button::Middle,
        rdev::Button::Unknown(4) => Button::Back,
        rdev::Button::Unknown(5) => Button::Forward,
        rdev::Button::Unknown(o) => Button::Other(o),
    }
}

pub fn enigo_button(b: Button) -> Option<enigo::Button> {
    Some(match b {
        Button::Left => enigo::Button::Left,
        Button::Right => enigo::Button::Right,
        Button::Middle => enigo::Button::Middle,
        Button::Back => enigo::Button::Back,
        Button::Forward => enigo::Button::Forward,
        Button::Other(_) => return None,
    })
}

/// Map a wire key to an enigo key for injection. `None` means the key has no
/// sensible cross-platform injection and is dropped.
///
/// Letters, digits and punctuation are injected as characters (US layout on
/// the sending side); the OS on the receiving side applies its own modifier
/// handling, so Shift+a arrives as 'A'.
pub fn enigo_key(k: Key) -> Option<enigo::Key> {
    use enigo::Key as E;
    let ch = |c: char| Some(E::Unicode(c));
    Some(match k {
        Key::Alt => E::Alt,
        Key::AltGr => E::Alt,
        Key::Backspace => E::Backspace,
        Key::CapsLock => E::CapsLock,
        Key::ControlLeft => E::LControl,
        Key::ControlRight => E::RControl,
        Key::Delete => E::Delete,
        Key::DownArrow => E::DownArrow,
        Key::End => E::End,
        Key::Escape => E::Escape,
        Key::F1 => E::F1,
        Key::F2 => E::F2,
        Key::F3 => E::F3,
        Key::F4 => E::F4,
        Key::F5 => E::F5,
        Key::F6 => E::F6,
        Key::F7 => E::F7,
        Key::F8 => E::F8,
        Key::F9 => E::F9,
        Key::F10 => E::F10,
        Key::F11 => E::F11,
        Key::F12 => E::F12,
        Key::Home => E::Home,
        Key::LeftArrow => E::LeftArrow,
        Key::MetaLeft | Key::MetaRight => E::Meta,
        Key::PageDown => E::PageDown,
        Key::PageUp => E::PageUp,
        Key::Return | Key::KpReturn => E::Return,
        Key::RightArrow => E::RightArrow,
        Key::ShiftLeft => E::LShift,
        Key::ShiftRight => E::RShift,
        Key::Space => E::Space,
        Key::Tab => E::Tab,
        Key::UpArrow => E::UpArrow,
        Key::KpDelete => E::Delete,
        #[cfg(any(target_os = "windows", all(unix, not(target_os = "macos"))))]
        Key::PrintScreen => E::PrintScr,
        #[cfg(any(target_os = "windows", all(unix, not(target_os = "macos"))))]
        Key::Insert => E::Insert,
        #[cfg(any(target_os = "windows", all(unix, not(target_os = "macos"))))]
        Key::NumLock => E::Numlock,
        #[cfg(any(target_os = "windows", all(unix, not(target_os = "macos"))))]
        Key::Pause => E::Pause,
        // These keys don't exist in enigo's macOS backend.
        #[cfg(target_os = "macos")]
        Key::NumLock | Key::Pause | Key::PrintScreen | Key::Insert => return None,
        // ScrollLock is the local emergency-release hotkey; never injected.
        Key::ScrollLock | Key::Function | Key::Unknown(_) => return None,
        Key::KeyA => return ch('a'),
        Key::KeyB => return ch('b'),
        Key::KeyC => return ch('c'),
        Key::KeyD => return ch('d'),
        Key::KeyE => return ch('e'),
        Key::KeyF => return ch('f'),
        Key::KeyG => return ch('g'),
        Key::KeyH => return ch('h'),
        Key::KeyI => return ch('i'),
        Key::KeyJ => return ch('j'),
        Key::KeyK => return ch('k'),
        Key::KeyL => return ch('l'),
        Key::KeyM => return ch('m'),
        Key::KeyN => return ch('n'),
        Key::KeyO => return ch('o'),
        Key::KeyP => return ch('p'),
        Key::KeyQ => return ch('q'),
        Key::KeyR => return ch('r'),
        Key::KeyS => return ch('s'),
        Key::KeyT => return ch('t'),
        Key::KeyU => return ch('u'),
        Key::KeyV => return ch('v'),
        Key::KeyW => return ch('w'),
        Key::KeyX => return ch('x'),
        Key::KeyY => return ch('y'),
        Key::KeyZ => return ch('z'),
        Key::Num0 | Key::Kp0 => return ch('0'),
        Key::Num1 | Key::Kp1 => return ch('1'),
        Key::Num2 | Key::Kp2 => return ch('2'),
        Key::Num3 | Key::Kp3 => return ch('3'),
        Key::Num4 | Key::Kp4 => return ch('4'),
        Key::Num5 | Key::Kp5 => return ch('5'),
        Key::Num6 | Key::Kp6 => return ch('6'),
        Key::Num7 | Key::Kp7 => return ch('7'),
        Key::Num8 | Key::Kp8 => return ch('8'),
        Key::Num9 | Key::Kp9 => return ch('9'),
        Key::BackQuote => return ch('`'),
        Key::Minus | Key::KpMinus => return ch('-'),
        Key::Equal => return ch('='),
        Key::LeftBracket => return ch('['),
        Key::RightBracket => return ch(']'),
        Key::BackSlash | Key::IntlBackslash => return ch('\\'),
        Key::SemiColon => return ch(';'),
        Key::Quote => return ch('\''),
        Key::Comma => return ch(','),
        Key::Dot => return ch('.'),
        Key::Slash | Key::KpDivide => return ch('/'),
        Key::KpPlus => return ch('+'),
        Key::KpMultiply => return ch('*'),
    })
}
