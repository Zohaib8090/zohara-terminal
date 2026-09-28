//! `ut-input`: turns a keyboard event into the byte sequence a PTY-side
//! shell/program expects (Section 6 "Input and editing", the toolbar-keys
//! part of "On-screen terminal keyboard toolbar").
//!
//! This is where the framework-specific event loop (`ut-ui`, eventually a
//! real GUI toolkit) hands off to something toolkit-agnostic and testable:
//! [`encode`] takes a [`Key`] plus [`Modifiers`] and the terminal's current
//! `application_cursor_keys`/`application_keypad` mode flags (from
//! `ut_grid::Terminal`) and returns the exact bytes to write to the PTY.
//! No GUI toolkit, no PTY, no allocation beyond the returned `Vec<u8>`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Delete,
    Tab,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Function(u8), // F1..F12
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Modifiers {
    pub fn none() -> Self {
        Modifiers::default()
    }

    pub fn ctrl() -> Self {
        Modifiers { ctrl: true, ..Default::default() }
    }

    pub fn is_none(&self) -> bool {
        !self.ctrl && !self.alt && !self.shift
    }
}

/// Terminal mode flags that change how some keys encode (mirrors the
/// relevant subset of `ut_grid::Terminal`'s public mode fields, so this
/// crate doesn't need to depend on `ut-grid` just for two bools).
#[derive(Debug, Clone, Copy, Default)]
pub struct EncodeModes {
    pub application_cursor_keys: bool,
    pub application_keypad: bool,
}

/// Encode a key press into the bytes to send to the PTY.
pub fn encode(key: Key, mods: Modifiers, modes: EncodeModes) -> Vec<u8> {
    if let Key::Char(c) = key {
        if mods.ctrl {
            if let Some(byte) = ctrl_byte(c) {
                let mut out = vec![byte];
                if mods.alt {
                    out.insert(0, 0x1b);
                }
                return out;
            }
        }
        let mut buf = [0u8; 4];
        let s = c.encode_utf8(&mut buf);
        let mut out = Vec::with_capacity(s.len() + 1);
        if mods.alt {
            out.push(0x1b);
        }
        out.extend_from_slice(s.as_bytes());
        return out;
    }

    let arrow = |letter: u8| -> Vec<u8> {
        if modes.application_cursor_keys {
            vec![0x1b, b'O', letter]
        } else {
            vec![0x1b, b'[', letter]
        }
    };

    match key {
        Key::Char(_) => unreachable!("handled above"),
        Key::Enter => vec![b'\r'],
        Key::Backspace => vec![0x7f],
        Key::Delete => b"\x1b[3~".to_vec(),
        Key::Tab => {
            if mods.shift {
                b"\x1b[Z".to_vec()
            } else {
                vec![b'\t']
            }
        }
        Key::Escape => vec![0x1b],
        Key::Up => arrow(b'A'),
        Key::Down => arrow(b'B'),
        Key::Right => arrow(b'C'),
        Key::Left => arrow(b'D'),
        Key::Home => {
            if modes.application_cursor_keys {
                vec![0x1b, b'O', b'H']
            } else {
                b"\x1b[H".to_vec()
            }
        }
        Key::End => {
            if modes.application_cursor_keys {
                vec![0x1b, b'O', b'F']
            } else {
                b"\x1b[F".to_vec()
            }
        }
        Key::PageUp => b"\x1b[5~".to_vec(),
        Key::PageDown => b"\x1b[6~".to_vec(),
        Key::Insert => b"\x1b[2~".to_vec(),
        Key::Function(n) => function_key(n),
    }
}

fn ctrl_byte(c: char) -> Option<u8> {
    let upper = c.to_ascii_uppercase();
    if upper.is_ascii_alphabetic() {
        Some((upper as u8) - b'A' + 1)
    } else {
        match c {
            '[' => Some(0x1b),
            '\\' => Some(0x1c),
            ']' => Some(0x1d),
            '^' | '6' => Some(0x1e),
            '_' | '-' => Some(0x1f),
            '?' => Some(0x7f),
            ' ' | '@' | '2' => Some(0x00),
            _ => None,
        }
    }
}

fn function_key(n: u8) -> Vec<u8> {
    // xterm-style function key encodings.
    match n {
        1 => b"\x1bOP".to_vec(),
        2 => b"\x1bOQ".to_vec(),
        3 => b"\x1bOR".to_vec(),
        4 => b"\x1bOS".to_vec(),
        5 => b"\x1b[15~".to_vec(),
        6 => b"\x1b[17~".to_vec(),
        7 => b"\x1b[18~".to_vec(),
        8 => b"\x1b[19~".to_vec(),
        9 => b"\x1b[20~".to_vec(),
        10 => b"\x1b[21~".to_vec(),
        11 => b"\x1b[23~".to_vec(),
        12 => b"\x1b[24~".to_vec(),
        _ => Vec::new(),
    }
}

/// Bracket a paste per bracketed-paste mode (Section 3 "Bracketed paste
/// mode"): the shell can then tell "typed" input from "pasted" input and
/// (per Section 10 "Safe paste") avoid treating pasted newlines as
/// immediate command submission.
pub fn bracket_paste(text: &str, enabled: bool) -> Vec<u8> {
    if !enabled {
        return text.as_bytes().to_vec();
    }
    let mut out = Vec::with_capacity(text.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_char_encodes_as_utf8() {
        assert_eq!(encode(Key::Char('a'), Modifiers::none(), EncodeModes::default()), b"a".to_vec());
        assert_eq!(encode(Key::Char('中'), Modifiers::none(), EncodeModes::default()), "中".as_bytes().to_vec());
    }

    #[test]
    fn ctrl_c_produces_etx() {
        assert_eq!(encode(Key::Char('c'), Modifiers::ctrl(), EncodeModes::default()), vec![0x03]);
    }

    #[test]
    fn ctrl_alt_combo_prefixes_escape() {
        let mods = Modifiers { ctrl: true, alt: true, shift: false };
        assert_eq!(encode(Key::Char('c'), mods, EncodeModes::default()), vec![0x1b, 0x03]);
    }

    #[test]
    fn alt_char_prefixes_escape() {
        let mods = Modifiers { alt: true, ..Default::default() };
        assert_eq!(encode(Key::Char('x'), mods, EncodeModes::default()), vec![0x1b, b'x']);
    }

    #[test]
    fn enter_is_carriage_return() {
        assert_eq!(encode(Key::Enter, Modifiers::none(), EncodeModes::default()), vec![b'\r']);
    }

    #[test]
    fn backspace_is_del_byte() {
        assert_eq!(encode(Key::Backspace, Modifiers::none(), EncodeModes::default()), vec![0x7f]);
    }

    #[test]
    fn arrow_keys_normal_mode() {
        assert_eq!(encode(Key::Up, Modifiers::none(), EncodeModes::default()), b"\x1b[A".to_vec());
        assert_eq!(encode(Key::Down, Modifiers::none(), EncodeModes::default()), b"\x1b[B".to_vec());
    }

    #[test]
    fn arrow_keys_application_mode() {
        let modes = EncodeModes { application_cursor_keys: true, application_keypad: false };
        assert_eq!(encode(Key::Up, Modifiers::none(), modes), b"\x1bOA".to_vec());
        assert_eq!(encode(Key::Left, Modifiers::none(), modes), b"\x1bOD".to_vec());
    }

    #[test]
    fn shift_tab_sends_back_tab() {
        let mods = Modifiers { shift: true, ..Default::default() };
        assert_eq!(encode(Key::Tab, mods, EncodeModes::default()), b"\x1b[Z".to_vec());
    }

    #[test]
    fn function_keys_encode_xterm_style() {
        assert_eq!(encode(Key::Function(1), Modifiers::none(), EncodeModes::default()), b"\x1bOP".to_vec());
        assert_eq!(encode(Key::Function(5), Modifiers::none(), EncodeModes::default()), b"\x1b[15~".to_vec());
    }

    #[test]
    fn bracketed_paste_wraps_text() {
        let out = bracket_paste("hello", true);
        assert_eq!(out, b"\x1b[200~hello\x1b[201~".to_vec());
    }

    #[test]
    fn bracketed_paste_disabled_passes_through() {
        let out = bracket_paste("hello", false);
        assert_eq!(out, b"hello".to_vec());
    }

    #[test]
    fn page_and_delete_keys() {
        assert_eq!(encode(Key::Delete, Modifiers::none(), EncodeModes::default()), b"\x1b[3~".to_vec());
        assert_eq!(encode(Key::PageUp, Modifiers::none(), EncodeModes::default()), b"\x1b[5~".to_vec());
        assert_eq!(encode(Key::PageDown, Modifiers::none(), EncodeModes::default()), b"\x1b[6~".to_vec());
    }
}
