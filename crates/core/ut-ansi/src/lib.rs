//! `ut-ansi`: a pure ANSI/VT escape-sequence parser.
//!
//! This crate is deliberately "dumb": it knows the *shape* of the control
//! sequences a terminal stream can contain (`vte`'s state machine does the
//! heavy lifting), but it has no opinion about what a cursor, a grid, or a
//! color is. [`AnsiParser::feed`] takes raw PTY bytes and returns a `Vec` of
//! owned [`AnsiEvent`]s; a separate crate (`ut-grid`) consumes those events
//! to maintain actual terminal state. This split exists so the parser can
//! be reused by anything that just wants to *observe* a byte stream (a
//! logger, a recorder, a protocol translator) without pulling in grid/cell
//! machinery, and so the grid state machine can be unit-tested against
//! hand-built event sequences without going through `vte` at all.

use vte::{Params, Parser, Perform};

/// One CSI (`ESC [ ... <action>`) sequence, with its parameters copied out
/// of `vte`'s borrowed [`Params`] into owned data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CsiEvent {
    /// Each element is one `;`-separated parameter, itself `:`-separated
    /// (sub-parameters) as `vte` reports it.
    pub params: Vec<Vec<u16>>,
    pub intermediates: Vec<u8>,
    pub ignore: bool,
    pub action: char,
}

impl CsiEvent {
    /// True if this is a DEC-private sequence (`ESC [ ? ... <action>`).
    pub fn is_private(&self) -> bool {
        self.intermediates.first() == Some(&b'?')
    }

    /// True if the first intermediate byte is `space` (used by e.g. `DECSCUSR`, `ESC[ q`).
    pub fn has_space_intermediate(&self) -> bool {
        self.intermediates.first() == Some(&b' ')
    }

    /// The first sub-parameter of the Nth parameter, if present and non-default (non-zero).
    pub fn param(&self, index: usize) -> Option<u16> {
        self.params.get(index).and_then(|p| p.first().copied()).filter(|&v| v != 0)
    }

    /// [`Self::param`] with a fallback default, matching common ANSI "0 means 1" semantics.
    pub fn param_or(&self, index: usize, default: u16) -> u16 {
        self.param(index).unwrap_or(default)
    }

    pub fn iter(&self) -> impl Iterator<Item = &[u16]> {
        self.params.iter().map(|p| p.as_slice())
    }
}

/// One DCS (Device Control String) hook, e.g. as used by Sixel graphics or
/// terminfo queries. Recognized but not interpreted by this crate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DcsHookEvent {
    pub params: Vec<Vec<u16>>,
    pub intermediates: Vec<u8>,
    pub ignore: bool,
    pub action: char,
}

/// One OSC (Operating System Command, `ESC ] ... ST/BEL`) sequence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OscEvent {
    pub params: Vec<Vec<u8>>,
    pub bell_terminated: bool,
}

impl OscEvent {
    /// The OSC number (the first `;`-separated field), if it parses as UTF-8.
    pub fn kind(&self) -> Option<&str> {
        self.params.first().and_then(|k| std::str::from_utf8(k).ok())
    }

    /// The Nth field after the kind, decoded lossily as UTF-8 (OSC text
    /// payloads, e.g. a window title, are not guaranteed valid UTF-8).
    pub fn field(&self, index: usize) -> Option<String> {
        self.params.get(index + 1).map(|f| String::from_utf8_lossy(f).into_owned())
    }
}

/// A single decoded event from the byte stream. This mirrors `vte::Perform`'s
/// callbacks one-to-one, just with owned data instead of borrows tied to the
/// parser's internal buffers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnsiEvent {
    /// A printable character (already UTF-8 decoded by `vte`).
    Print(char),
    /// A C0/C1 control byte executed outside of any escape sequence (BEL, BS, TAB, LF, CR, ...).
    Execute(u8),
    /// A CSI-introduced sequence, e.g. cursor movement, SGR, erase, mode set.
    Csi(CsiEvent),
    /// A two-character (or intermediate-prefixed) ESC sequence, e.g. `ESC D` (IND), `ESC M` (RI).
    Esc { intermediates: Vec<u8>, ignore: bool, byte: u8 },
    /// An OSC sequence, e.g. window title or OSC 7 cwd reporting.
    Osc(OscEvent),
    /// DCS hook/put/unhook, grouped as they always arrive as a sequence for one string.
    DcsHook(DcsHookEvent),
    DcsPut(u8),
    DcsUnhook,
}

/// Parses raw bytes into a stream of [`AnsiEvent`]s.
pub struct AnsiParser {
    inner: Parser,
    sink: EventSink,
}

#[derive(Default)]
struct EventSink {
    events: Vec<AnsiEvent>,
}

impl Default for AnsiParser {
    fn default() -> Self {
        Self::new()
    }
}

impl AnsiParser {
    pub fn new() -> Self {
        AnsiParser { inner: Parser::new(), sink: EventSink::default() }
    }

    /// Feed a chunk of bytes (as read from a PTY) and get back the events it produced.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<AnsiEvent> {
        for &b in bytes {
            self.inner.advance(&mut self.sink, b);
        }
        std::mem::take(&mut self.sink.events)
    }
}

fn owned_params(params: &Params) -> Vec<Vec<u16>> {
    params.iter().map(|p| p.to_vec()).collect()
}

impl Perform for EventSink {
    fn print(&mut self, c: char) {
        self.events.push(AnsiEvent::Print(c));
    }

    fn execute(&mut self, byte: u8) {
        self.events.push(AnsiEvent::Execute(byte));
    }

    fn hook(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        self.events.push(AnsiEvent::DcsHook(DcsHookEvent {
            params: owned_params(params),
            intermediates: intermediates.to_vec(),
            ignore,
            action,
        }));
    }

    fn put(&mut self, byte: u8) {
        self.events.push(AnsiEvent::DcsPut(byte));
    }

    fn unhook(&mut self) {
        self.events.push(AnsiEvent::DcsUnhook);
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        self.events.push(AnsiEvent::Osc(OscEvent {
            params: params.iter().map(|p| p.to_vec()).collect(),
            bell_terminated,
        }));
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        self.events.push(AnsiEvent::Csi(CsiEvent {
            params: owned_params(params),
            intermediates: intermediates.to_vec(),
            ignore,
            action,
        }));
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        self.events.push(AnsiEvent::Esc { intermediates: intermediates.to_vec(), ignore, byte });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_text_as_print_events() {
        let mut p = AnsiParser::new();
        let events = p.feed(b"hi");
        assert_eq!(events, vec![AnsiEvent::Print('h'), AnsiEvent::Print('i')]);
    }

    #[test]
    fn parses_control_bytes_as_execute() {
        let mut p = AnsiParser::new();
        let events = p.feed(b"\r\n");
        assert_eq!(events, vec![AnsiEvent::Execute(0x0D), AnsiEvent::Execute(0x0A)]);
    }

    #[test]
    fn parses_csi_cursor_move() {
        let mut p = AnsiParser::new();
        let events = p.feed(b"\x1b[3;5H");
        assert_eq!(events.len(), 1);
        match &events[0] {
            AnsiEvent::Csi(csi) => {
                assert_eq!(csi.action, 'H');
                assert_eq!(csi.param_or(0, 1), 3);
                assert_eq!(csi.param_or(1, 1), 5);
            }
            other => panic!("expected Csi, got {other:?}"),
        }
    }

    #[test]
    fn parses_private_mode_csi() {
        let mut p = AnsiParser::new();
        let events = p.feed(b"\x1b[?2004h");
        match &events[0] {
            AnsiEvent::Csi(csi) => {
                assert!(csi.is_private());
                assert_eq!(csi.action, 'h');
                assert_eq!(csi.param_or(0, 0), 2004);
            }
            other => panic!("expected Csi, got {other:?}"),
        }
    }

    #[test]
    fn parses_osc_title() {
        let mut p = AnsiParser::new();
        let events = p.feed(b"\x1b]0;hello\x07");
        match &events[0] {
            AnsiEvent::Osc(osc) => {
                assert_eq!(osc.kind(), Some("0"));
                assert_eq!(osc.field(0).as_deref(), Some("hello"));
                assert!(osc.bell_terminated);
            }
            other => panic!("expected Osc, got {other:?}"),
        }
    }

    #[test]
    fn parses_esc_dispatch() {
        let mut p = AnsiParser::new();
        let events = p.feed(b"\x1bM");
        assert_eq!(events, vec![AnsiEvent::Esc { intermediates: vec![], ignore: false, byte: b'M' }]);
    }

    #[test]
    fn feed_drains_between_calls() {
        let mut p = AnsiParser::new();
        let first = p.feed(b"a");
        assert_eq!(first.len(), 1);
        let second = p.feed(b"b");
        assert_eq!(second, vec![AnsiEvent::Print('b')]);
    }

    #[test]
    fn sgr_true_color_params_preserved() {
        let mut p = AnsiParser::new();
        let events = p.feed(b"\x1b[38;2;10;20;30m");
        match &events[0] {
            AnsiEvent::Csi(csi) => {
                assert_eq!(csi.action, 'm');
                let nums: Vec<u16> = csi.iter().map(|p| p[0]).collect();
                assert_eq!(nums, vec![38, 2, 10, 20, 30]);
            }
            other => panic!("expected Csi, got {other:?}"),
        }
    }
}
