//! Terminal emulator layer: VT parser (via vte crate), screen model handle,
//! and utility functions for output processing.

use tracing::debug;

// ─── Read mode ─────────────────────────────────────────────────────

/// Read mode for `terminal_read_output`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadMode {
    /// Raw bytes converted to lossy UTF-8 (preserves ANSI sequences).
    Raw,
    /// Control characters are escaped for visibility (e.g. `\x1b` shown as `<ESC>`).
    Escaped,
    /// Strip non-printable control sequences, returning only visible text.
    Text,
}

impl ReadMode {
    pub fn parse_mode(s: &str) -> Option<Self> {
        match s {
            "raw" => Some(Self::Raw),
            "escaped" => Some(Self::Escaped),
            "text" => Some(Self::Text),
            _ => None,
        }
    }
}

// ─── Terminal operations ───────────────────────────────────────────

/// Semantic terminal operation emitted by the VT parser.
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalOp {
    /// Printable text run.
    Print(String),
    /// Line feed (LF).
    Linefeed,
    /// Carriage return (CR).
    CarriageReturn,
    /// Backspace (BS).
    Backspace,
    /// Horizontal tab (HT).
    Tab,
    /// Bell (BEL).
    Bell,
    /// CUU — Cursor Up.
    CursorUp(u16),
    /// CUD — Cursor Down.
    CursorDown(u16),
    /// CUF — Cursor Forward.
    CursorForward(u16),
    /// CUB — Cursor Back.
    CursorBack(u16),
    /// CUP — Cursor Position (1-based; 0 means default-to-1).
    CursorPosition { row: u16, col: u16 },
    /// CHA — Cursor Horizontal Absolute.
    CursorHorizontalAbsolute(u16),
    /// VPA — Vertical Position Absolute.
    CursorVerticalAbsolute(u16),
    /// CNL — Cursor Next Line (CSI Ps E).
    CursorNextLine(u16),
    /// CPL — Cursor Previous Line (CSI Ps F).
    CursorPreviousLine(u16),
    /// ED — Erase in Display.
    EraseInDisplay(EraseMode),
    /// EL — Erase in Line.
    EraseInLine(EraseMode),
    /// SGR — Select Graphic Rendition (raw parameter codes).
    SetGraphicRendition(Vec<u16>),
    /// DECSC — Save Cursor.
    SaveCursor,
    /// DECRC — Restore Cursor.
    RestoreCursor,
    /// SU — Scroll Up.
    ScrollUp(u16),
    /// SD — Scroll Down.
    ScrollDown(u16),
    /// DECSTBM — Set Scrolling Region (1-based; 0 = default).
    SetScrollingRegion { top: u16, bottom: u16 },
    /// DECSET — DEC Private Mode Set (e.g. 1049 = alternate screen).
    DecPrivateModeSet(u16),
    /// DECRST — DEC Private Mode Reset.
    DecPrivateModeReset(u16),
    /// OSC 0/2 — Set Window Title.
    SetWindowTitle(String),
    /// IL — Insert Lines (CSI Ps L).
    InsertLine(u16),
    /// DL — Delete Lines (CSI Ps M).
    DeleteLine(u16),
    /// ICH — Insert Characters (CSI Ps @).
    InsertCharacter(u16),
    /// DCH — Delete Characters (CSI Ps P).
    DeleteCharacter(u16),
    /// DA — Device Attributes response (ignored, logged).
    DeviceAttributes(Vec<u16>),
    /// REP — Repeat last printed character (CSI Ps b).
    Repeat(u16),
}

/// Erase scope for ED/EL operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EraseMode {
    /// From cursor to end.
    ToEnd,
    /// From start to cursor.
    ToStart,
    /// Entire display/line.
    All,
}

fn erase_mode_from(n: u16) -> EraseMode {
    match n {
        0 => EraseMode::ToEnd,
        1 => EraseMode::ToStart,
        _ => EraseMode::All,
    }
}

// ─── Incremental UTF-8 Decoder ─────────────────────────────────────

/// Incremental UTF-8 decoder that preserves incomplete multibyte tails
/// across PTY reads.
pub struct IncrementalUtf8Decoder {
    pending: Vec<u8>,
}

impl IncrementalUtf8Decoder {
    pub fn new() -> Self {
        Self {
            pending: Vec::with_capacity(3),
        }
    }

    pub fn decode(&mut self, chunk: &[u8]) -> String {
        if chunk.is_empty() && self.pending.is_empty() {
            return String::new();
        }

        let mut combined: Vec<u8>;
        let input: &[u8] = if self.pending.is_empty() {
            chunk
        } else {
            combined = Vec::with_capacity(self.pending.len() + chunk.len());
            combined.extend_from_slice(&self.pending);
            combined.extend_from_slice(chunk);
            self.pending.clear();
            &combined
        };

        if input.is_empty() {
            return String::new();
        }

        let split_point = find_utf8_boundary(input);
        if split_point == input.len() {
            unsafe { String::from_utf8_unchecked(input.to_vec()) }
        } else {
            self.pending.extend_from_slice(&input[split_point..]);
            unsafe { String::from_utf8_unchecked(input[..split_point].to_vec()) }
        }
    }

    pub fn flush_lossy(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let s = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        s
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

impl Default for IncrementalUtf8Decoder {
    fn default() -> Self {
        Self::new()
    }
}

// ─── vte-based VT Parser ───────────────────────────────────────────

/// VT parser performer that collects [`TerminalOp`] values.
///
/// Implements the `vte::Perform` trait to translate low-level VT protocol
/// events into the semantic `TerminalOp` enum consumed by the screen model.
struct VtePerformer<'a> {
    ops: &'a mut Vec<TerminalOp>,
    print_buf: String,
}

impl<'a> VtePerformer<'a> {
    fn new(ops: &'a mut Vec<TerminalOp>) -> Self {
        Self {
            ops,
            print_buf: String::with_capacity(256),
        }
    }

    fn flush_print(&mut self) {
        if !self.print_buf.is_empty() {
            let text = std::mem::take(&mut self.print_buf);
            self.ops.push(TerminalOp::Print(text));
        }
    }
}

impl vte::Perform for VtePerformer<'_> {
    fn print(&mut self, c: char) {
        self.print_buf.push(c);
    }

    fn execute(&mut self, byte: u8) {
        self.flush_print();
        match byte {
            0x07 => self.ops.push(TerminalOp::Bell),
            0x08 => self.ops.push(TerminalOp::Backspace),
            0x09 => self.ops.push(TerminalOp::Tab),
            0x0A => self.ops.push(TerminalOp::Linefeed),
            0x0D => self.ops.push(TerminalOp::CarriageReturn),
            _ => {
                debug!(byte = format!("0x{:02X}", byte), "Unhandled C0 control");
            }
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        self.flush_print();

        // Helper: get param at index, defaulting to `default_val`.
        let param = |idx: usize, default_val: u16| -> u16 {
            params
                .iter()
                .nth(idx)
                .and_then(|sub| sub.first().copied())
                .unwrap_or(default_val)
        };
        // Helper: get param at index, treating 0 as `default_val` (1-based semantics).
        let param1 = |idx: usize, default_val: u16| -> u16 {
            let v = param(idx, 0);
            if v == 0 {
                default_val
            } else {
                v
            }
        };

        if intermediates.contains(&b'?') {
            // DEC Private Mode
            let mode = param(0, 0);
            match action {
                'h' => {
                    self.ops.push(TerminalOp::DecPrivateModeSet(mode));
                }
                'l' => {
                    self.ops.push(TerminalOp::DecPrivateModeReset(mode));
                }
                _ => {
                    debug!(
                        mode,
                        action = format!("{:?}", action),
                        "Unhandled DEC private CSI"
                    );
                }
            }
            return;
        }

        match action {
            // Cursor movement
            'A' => self.ops.push(TerminalOp::CursorUp(param1(0, 1))),
            'B' => self.ops.push(TerminalOp::CursorDown(param1(0, 1))),
            'C' => self.ops.push(TerminalOp::CursorForward(param1(0, 1))),
            'D' => self.ops.push(TerminalOp::CursorBack(param1(0, 1))),
            'H' | 'f' => {
                self.ops.push(TerminalOp::CursorPosition {
                    row: param1(0, 1),
                    col: param1(1, 1),
                });
            }
            'G' => self
                .ops
                .push(TerminalOp::CursorHorizontalAbsolute(param1(0, 1))),
            'd' => self
                .ops
                .push(TerminalOp::CursorVerticalAbsolute(param1(0, 1))),
            'E' => self.ops.push(TerminalOp::CursorNextLine(param1(0, 1))),
            'F' => self.ops.push(TerminalOp::CursorPreviousLine(param1(0, 1))),

            // Erase
            'J' => self
                .ops
                .push(TerminalOp::EraseInDisplay(erase_mode_from(param(0, 0)))),
            'K' => self
                .ops
                .push(TerminalOp::EraseInLine(erase_mode_from(param(0, 0)))),

            // SGR
            'm' => {
                let sgr_params: Vec<u16> = if params.is_empty() {
                    vec![0]
                } else {
                    params.iter().flat_map(|sub| sub.iter().copied()).collect()
                };
                self.ops.push(TerminalOp::SetGraphicRendition(sgr_params));
            }

            // Cursor save/restore
            's' => self.ops.push(TerminalOp::SaveCursor),
            'u' => self.ops.push(TerminalOp::RestoreCursor),

            // Scroll
            'S' => self.ops.push(TerminalOp::ScrollUp(param(0, 1))),
            'T' => self.ops.push(TerminalOp::ScrollDown(param(0, 1))),

            // Scrolling region
            'r' => self.ops.push(TerminalOp::SetScrollingRegion {
                top: param(0, 0),
                bottom: param(1, 0),
            }),

            // Insert/Delete line
            'L' => self.ops.push(TerminalOp::InsertLine(param1(0, 1))),
            'M' => self.ops.push(TerminalOp::DeleteLine(param1(0, 1))),

            // Insert/Delete character
            '@' => self.ops.push(TerminalOp::InsertCharacter(param1(0, 1))),
            'P' => self.ops.push(TerminalOp::DeleteCharacter(param1(0, 1))),

            // REP — Repeat last printed character
            'b' => self.ops.push(TerminalOp::Repeat(param1(0, 1))),

            // Device Attributes — log but don't act
            'c' => {
                let attrs: Vec<u16> = params.iter().flat_map(|s| s.iter().copied()).collect();
                debug!(?attrs, "Device Attributes response");
                self.ops.push(TerminalOp::DeviceAttributes(attrs));
            }

            _ => {
                let nums: Vec<u16> = params.iter().flat_map(|s| s.iter().copied()).collect();
                debug!(
                    action = format!("{:?}", action),
                    ?nums,
                    ?intermediates,
                    "Unhandled CSI sequence"
                );
            }
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        self.flush_print();
        match byte {
            b'7' => self.ops.push(TerminalOp::SaveCursor), // DECSC
            b'8' => self.ops.push(TerminalOp::RestoreCursor), // DECRC
            b'D' => self.ops.push(TerminalOp::Linefeed),   // IND
            b'M' => self.ops.push(TerminalOp::ScrollUp(1)), // RI
            b'E' => {
                // NEL
                self.ops.push(TerminalOp::CarriageReturn);
                self.ops.push(TerminalOp::Linefeed);
            }
            b'c' => {
                // RIS — Reset Initial State (we just log it)
                debug!("RIS (full reset) received — ignored by emulator");
            }
            _ => {
                debug!(
                    byte = format!("0x{:02X}", byte),
                    ?intermediates,
                    "Unhandled ESC sequence"
                );
            }
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        self.flush_print();
        if params.is_empty() {
            return;
        }
        // First parameter is the OSC code
        if let Ok(code_str) = std::str::from_utf8(params[0]) {
            if let Ok(code) = code_str.parse::<u16>() {
                match code {
                    0 | 2 => {
                        let title = params
                            .get(1)
                            .and_then(|b| std::str::from_utf8(b).ok())
                            .unwrap_or("")
                            .to_string();
                        self.ops.push(TerminalOp::SetWindowTitle(title));
                    }
                    _ => {
                        debug!(code, "Unhandled OSC code");
                    }
                }
            }
        }
    }

    fn hook(&mut self, params: &vte::Params, intermediates: &[u8], _ignore: bool, action: char) {
        self.flush_print();
        debug!(
            action = format!("{:?}", action),
            ?intermediates,
            param_count = params.len(),
            "DCS hook received — not supported"
        );
    }

    fn unhook(&mut self) {
        // DCS unhook — no-op
    }

    fn put(&mut self, _byte: u8) {
        // DCS data byte — no-op
    }
}

// ─── Terminal Emulator Handle ──────────────────────────────────────

/// Terminal emulator handle holding the VT parser, screen model, and parsed operations.
///
/// The parser consumes raw PTY bytes via the `vte` crate's state machine
/// (Paul Williams' ANSI parser) and produces semantic [`TerminalOp`] values.
/// The screen model consumes these ops to maintain a grid representation.
pub struct TerminalEmulatorHandle {
    cols: u16,
    rows: u16,
    parser: vte::Parser,
    pending_ops: Vec<TerminalOp>,
    deferred_text_ops: Vec<TerminalOp>,
    screen: super::screen::TerminalScreen,
}

impl TerminalEmulatorHandle {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            parser: vte::Parser::new(),
            pending_ops: Vec::new(),
            deferred_text_ops: Vec::new(),
            screen: super::screen::TerminalScreen::new(cols as usize, rows as usize),
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        self.screen.resize(cols as usize, rows as usize);
    }

    #[allow(dead_code)]
    pub fn cols(&self) -> u16 {
        self.cols
    }

    #[allow(dead_code)]
    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// Feed raw PTY bytes through the VT parser and update the screen model.
    pub fn feed_bytes(&mut self, bytes: &[u8]) {
        let mut ops = Vec::new();
        {
            let mut performer = VtePerformer::new(&mut ops);
            self.parser.advance(&mut performer, bytes);
            performer.flush_print();
        }
        for op in &ops {
            self.screen.process_op(op);
        }
        let pure_print_chunk = ops.iter().all(|op| matches!(op, TerminalOp::Print(_)))
            && bytes.iter().all(|b| !matches!(b, 0x00..=0x1F | 0x7F));
        if pure_print_chunk {
            self.deferred_text_ops.extend(ops);
        } else {
            self.pending_ops.extend(ops);
        }
    }

    #[allow(dead_code)]
    pub fn take_ops(&mut self) -> Vec<TerminalOp> {
        std::mem::take(&mut self.pending_ops)
    }

    pub fn flush_parser(&mut self) {
        if !self.deferred_text_ops.is_empty() {
            self.pending_ops.append(&mut self.deferred_text_ops);
        }
    }

    #[allow(dead_code)]
    pub fn screen(&self) -> &super::screen::TerminalScreen {
        &self.screen
    }

    #[allow(dead_code)]
    pub fn screen_mut(&mut self) -> &mut super::screen::TerminalScreen {
        &mut self.screen
    }

    pub fn state_hash(&self) -> u64 {
        self.screen.state_hash()
    }
}

// ─── UTF-8 boundary finder ─────────────────────────────────────────

fn find_utf8_boundary(input: &[u8]) -> usize {
    match std::str::from_utf8(input) {
        Ok(_) => input.len(),
        Err(e) => e.valid_up_to(),
    }
}

// ─── Utility functions ─────────────────────────────────────────────

/// Strip ANSI/VT control sequences from a string, returning only visible text.
pub fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'\x1b' => {
                if i + 1 >= bytes.len() {
                    break;
                }
                match bytes[i + 1] {
                    b'[' => {
                        i += 2;
                        while i < bytes.len() && !(bytes[i] >= 0x40 && bytes[i] <= 0x7E) {
                            i += 1;
                        }
                        if i < bytes.len() {
                            i += 1;
                        }
                    }
                    b']' => {
                        i += 2;
                        while i < bytes.len() {
                            if bytes[i] == 0x07 {
                                i += 1;
                                break;
                            }
                            if bytes[i] == b'\\' && i > 0 && bytes[i - 1] == b'\x1b' {
                                i += 1;
                                break;
                            }
                            i += 1;
                        }
                    }
                    _ => {
                        i += 2;
                    }
                }
            }
            b'\n' | b'\r' | b'\t' => {
                result.push(bytes[i] as char);
                i += 1;
            }
            0x00..=0x1F => {
                i += 1;
            }
            _ => {
                let char_len = utf8_char_len(bytes[i]);
                let end = (i + char_len).min(bytes.len());
                if end <= bytes.len() {
                    result.push_str(&s[i..end]);
                }
                i = end;
            }
        }
    }

    result
}

/// Escape ANSI/control bytes for debug visibility.
pub fn escape_control(s: &str) -> String {
    let mut result = String::with_capacity(s.len() * 2);
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'\x1b' => {
                result.push_str("<ESC>");
                i += 1;
            }
            b'\n' => {
                result.push_str("\\n");
                i += 1;
            }
            b'\r' => {
                result.push_str("\\r");
                i += 1;
            }
            b'\t' => {
                result.push_str("\\t");
                i += 1;
            }
            0x00..=0x1F => {
                result.push_str(&format!("<{:02X}>", bytes[i]));
                i += 1;
            }
            _ => {
                let char_len = utf8_char_len(bytes[i]);
                let end = (i + char_len).min(bytes.len());
                if end <= bytes.len() {
                    result.push_str(&s[i..end]);
                }
                i = end;
            }
        }
    }

    result
}

fn utf8_char_len(lead: u8) -> usize {
    if lead < 0x80 {
        1
    } else if lead < 0xE0 {
        2
    } else if lead < 0xF0 {
        3
    } else {
        4
    }
}

// ─── Helper for tests: parse bytes into ops ────────────────────────

/// Parse raw bytes into a `Vec<TerminalOp>` using the vte parser.
/// This is a convenience function used by tests in this module and `screen.rs`.
pub fn parse_bytes(bytes: &[u8]) -> Vec<TerminalOp> {
    let mut parser = vte::Parser::new();
    let mut ops = Vec::new();
    let mut performer = VtePerformer::new(&mut ops);
    parser.advance(&mut performer, bytes);
    performer.flush_print();
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_mode_parse_mode() {
        assert_eq!(ReadMode::parse_mode("raw"), Some(ReadMode::Raw));
        assert_eq!(ReadMode::parse_mode("escaped"), Some(ReadMode::Escaped));
        assert_eq!(ReadMode::parse_mode("text"), Some(ReadMode::Text));
        assert_eq!(ReadMode::parse_mode("other"), None);
    }

    #[test]
    fn test_incremental_utf8_ascii() {
        let mut dec = IncrementalUtf8Decoder::new();
        assert_eq!(dec.decode(b"hello"), "hello");
        assert!(!dec.has_pending());
    }

    #[test]
    fn test_incremental_utf8_split_multibyte() {
        let mut dec = IncrementalUtf8Decoder::new();
        assert_eq!(dec.decode(&[0xE4, 0xB8]), "");
        assert!(dec.has_pending());
        assert_eq!(dec.decode(&[0xAD]), "中");
        assert!(!dec.has_pending());
    }

    #[test]
    fn test_incremental_utf8_split_emoji() {
        let mut dec = IncrementalUtf8Decoder::new();
        assert_eq!(dec.decode(&[0xF0, 0x9F]), "");
        assert_eq!(dec.decode(&[0x98, 0x80]), "😀");
        assert!(!dec.has_pending());
    }

    #[test]
    fn test_incremental_utf8_mixed() {
        let mut dec = IncrementalUtf8Decoder::new();
        assert_eq!(dec.decode(b"hi\xE4"), "hi");
        assert_eq!(dec.decode(b"\xB8\xADbye"), "中bye");
    }

    #[test]
    fn test_incremental_utf8_no_split() {
        let mut dec = IncrementalUtf8Decoder::new();
        assert_eq!(dec.decode("你好世界".as_bytes()), "你好世界");
    }

    #[test]
    fn test_incremental_utf8_flush_lossy() {
        let mut dec = IncrementalUtf8Decoder::new();
        dec.decode(&[0xE4, 0xB8]);
        let flushed = dec.flush_lossy();
        assert!(!flushed.is_empty());
        assert!(!dec.has_pending());
    }

    #[test]
    fn test_incremental_utf8_empty_chunks() {
        let mut dec = IncrementalUtf8Decoder::new();
        assert_eq!(dec.decode(b""), "");
        assert_eq!(dec.decode(b"hello"), "hello");
        assert_eq!(dec.decode(b""), "");
    }

    #[test]
    fn test_strip_ansi_csi() {
        assert_eq!(strip_ansi("\x1b[31mHello\x1b[0m World"), "Hello World");
    }

    #[test]
    fn test_strip_ansi_cursor_move() {
        assert_eq!(strip_ansi("\x1b[2J\x1b[H\x1b[1;1HHello"), "Hello");
    }

    #[test]
    fn test_strip_ansi_osc() {
        assert_eq!(strip_ansi("\x1b]0;title\x07Content"), "Content");
    }

    #[test]
    fn test_strip_ansi_preserves_newlines() {
        assert_eq!(strip_ansi("line1\nline2\r\nline3"), "line1\nline2\r\nline3");
    }

    #[test]
    fn test_strip_ansi_no_sequences() {
        assert_eq!(strip_ansi("plain text"), "plain text");
    }

    #[test]
    fn test_strip_ansi_multibyte() {
        assert_eq!(strip_ansi("\x1b[32m你好\x1b[0m"), "你好");
    }

    #[test]
    fn test_escape_control_esc() {
        assert_eq!(escape_control("\x1b["), "<ESC>[");
    }

    #[test]
    fn test_escape_control_newlines() {
        assert_eq!(escape_control("a\nb\rc"), "a\\nb\\rc");
    }

    #[test]
    fn test_escape_control_other_c0() {
        assert_eq!(escape_control("\x00\x01\x1F"), "<00><01><1F>");
    }

    #[test]
    fn test_escape_control_mixed() {
        assert_eq!(
            escape_control("hi\x1b[31mred\x1b[0m"),
            "hi<ESC>[31mred<ESC>[0m"
        );
    }

    #[test]
    fn test_emulator_handle_dims() {
        let mut handle = TerminalEmulatorHandle::new(80, 24);
        assert_eq!(handle.cols(), 80);
        assert_eq!(handle.rows(), 24);
        handle.resize(120, 40);
        assert_eq!(handle.cols(), 120);
        assert_eq!(handle.rows(), 40);
    }

    // ─── VT Parser tests ───────────────────────────────────────────

    #[test]
    fn test_parser_plain_text() {
        let ops = parse_bytes(b"hello world");
        assert_eq!(ops, vec![TerminalOp::Print("hello world".to_string())]);
    }

    #[test]
    fn test_parser_c0_controls() {
        let ops = parse_bytes(b"line1\nline2\r\t");
        assert_eq!(
            ops,
            vec![
                TerminalOp::Print("line1".to_string()),
                TerminalOp::Linefeed,
                TerminalOp::Print("line2".to_string()),
                TerminalOp::CarriageReturn,
                TerminalOp::Tab,
            ]
        );
    }

    #[test]
    fn test_parser_cursor_movement() {
        let ops = parse_bytes(b"\x1b[5A\x1b[2B\x1b[10C\x1b[3D");
        assert_eq!(
            ops,
            vec![
                TerminalOp::CursorUp(5),
                TerminalOp::CursorDown(2),
                TerminalOp::CursorForward(10),
                TerminalOp::CursorBack(3),
            ]
        );
    }

    #[test]
    fn test_parser_cursor_position() {
        let ops = parse_bytes(b"\x1b[5;10H\x1b[H");
        assert_eq!(
            ops,
            vec![
                TerminalOp::CursorPosition { row: 5, col: 10 },
                TerminalOp::CursorPosition { row: 1, col: 1 },
            ]
        );
    }

    #[test]
    fn test_parser_erase_display() {
        let ops = parse_bytes(b"\x1b[J\x1b[1J\x1b[2J");
        assert_eq!(
            ops,
            vec![
                TerminalOp::EraseInDisplay(EraseMode::ToEnd),
                TerminalOp::EraseInDisplay(EraseMode::ToStart),
                TerminalOp::EraseInDisplay(EraseMode::All),
            ]
        );
    }

    #[test]
    fn test_parser_erase_line() {
        let ops = parse_bytes(b"\x1b[K\x1b[1K\x1b[2K");
        assert_eq!(
            ops,
            vec![
                TerminalOp::EraseInLine(EraseMode::ToEnd),
                TerminalOp::EraseInLine(EraseMode::ToStart),
                TerminalOp::EraseInLine(EraseMode::All),
            ]
        );
    }

    #[test]
    fn test_parser_sgr() {
        let ops = parse_bytes(b"\x1b[31m\x1b[1;32m\x1b[0m");
        assert_eq!(
            ops,
            vec![
                TerminalOp::SetGraphicRendition(vec![31]),
                TerminalOp::SetGraphicRendition(vec![1, 32]),
                TerminalOp::SetGraphicRendition(vec![0]),
            ]
        );
    }

    #[test]
    fn test_parser_save_restore_cursor() {
        let ops = parse_bytes(b"\x1b[s\x1b[u\x1b7\x1b8");
        assert_eq!(
            ops,
            vec![
                TerminalOp::SaveCursor,
                TerminalOp::RestoreCursor,
                TerminalOp::SaveCursor,
                TerminalOp::RestoreCursor,
            ]
        );
    }

    #[test]
    fn test_parser_alternate_screen() {
        let ops = parse_bytes(b"\x1b[?1049h\x1b[?1049l");
        assert_eq!(
            ops,
            vec![
                TerminalOp::DecPrivateModeSet(1049),
                TerminalOp::DecPrivateModeReset(1049),
            ]
        );
    }

    #[test]
    fn test_parser_osc_title() {
        let ops = parse_bytes(b"\x1b]0;My Title\x07data");
        assert_eq!(
            ops,
            vec![
                TerminalOp::SetWindowTitle("My Title".to_string()),
                TerminalOp::Print("data".to_string()),
            ]
        );
    }

    #[test]
    fn test_parser_osc_title_st() {
        let ops = parse_bytes(b"\x1b]2;title\x1b\\data");
        assert_eq!(
            ops,
            vec![
                TerminalOp::SetWindowTitle("title".to_string()),
                TerminalOp::Print("data".to_string()),
            ]
        );
    }

    #[test]
    fn test_parser_scroll() {
        let ops = parse_bytes(b"\x1b[3S\x1b[2T");
        assert_eq!(
            ops,
            vec![TerminalOp::ScrollUp(3), TerminalOp::ScrollDown(2)]
        );
    }

    #[test]
    fn test_parser_scrolling_region() {
        let ops = parse_bytes(b"\x1b[5;20r");
        assert_eq!(
            ops,
            vec![TerminalOp::SetScrollingRegion { top: 5, bottom: 20 }]
        );
    }

    #[test]
    fn test_parser_insert_delete_line() {
        let ops = parse_bytes(b"\x1b[3L\x1b[2M");
        assert_eq!(
            ops,
            vec![TerminalOp::InsertLine(3), TerminalOp::DeleteLine(2)]
        );
    }

    #[test]
    fn test_parser_insert_delete_char() {
        let ops = parse_bytes(b"\x1b[5@\x1b[4P");
        assert_eq!(
            ops,
            vec![
                TerminalOp::InsertCharacter(5),
                TerminalOp::DeleteCharacter(4)
            ]
        );
    }

    #[test]
    fn test_parser_mixed_sequence() {
        let ops = parse_bytes(b"\x1b[2J\x1b[H\x1b[?1049hHello World\x1b[31mRed\x1b[0m");
        assert_eq!(
            ops,
            vec![
                TerminalOp::EraseInDisplay(EraseMode::All),
                TerminalOp::CursorPosition { row: 1, col: 1 },
                TerminalOp::DecPrivateModeSet(1049),
                TerminalOp::Print("Hello World".to_string()),
                TerminalOp::SetGraphicRendition(vec![31]),
                TerminalOp::Print("Red".to_string()),
                TerminalOp::SetGraphicRendition(vec![0]),
            ]
        );
    }

    #[test]
    fn test_parser_split_sequence() {
        let mut emu = TerminalEmulatorHandle::new(80, 24);
        emu.feed_bytes(b"\x1b[3");
        assert!(emu.take_ops().is_empty());
        emu.feed_bytes(b"1m");
        let ops = emu.take_ops();
        assert_eq!(ops, vec![TerminalOp::SetGraphicRendition(vec![31])]);
    }

    #[test]
    fn test_parser_split_text() {
        let mut emu = TerminalEmulatorHandle::new(80, 24);
        emu.feed_bytes(b"hel");
        // Pure text is deferred
        assert!(emu.take_ops().is_empty());
        emu.flush_parser();
        let ops = emu.take_ops();
        assert_eq!(ops, vec![TerminalOp::Print("hel".to_string())]);
    }

    #[test]
    fn test_parser_default_params() {
        let ops = parse_bytes(b"\x1b[A\x1b[H");
        assert_eq!(
            ops,
            vec![
                TerminalOp::CursorUp(1),
                TerminalOp::CursorPosition { row: 1, col: 1 },
            ]
        );
    }

    #[test]
    fn test_parser_cha_vpa() {
        let ops = parse_bytes(b"\x1b[20G\x1b[5d");
        assert_eq!(
            ops,
            vec![
                TerminalOp::CursorHorizontalAbsolute(20),
                TerminalOp::CursorVerticalAbsolute(5),
            ]
        );
    }

    #[test]
    fn test_parser_esc_d_m_e() {
        let ops = parse_bytes(b"\x1bD\x1bM\x1bE");
        assert_eq!(
            ops,
            vec![
                TerminalOp::Linefeed,
                TerminalOp::ScrollUp(1),
                TerminalOp::CarriageReturn,
                TerminalOp::Linefeed,
            ]
        );
    }

    #[test]
    fn test_parser_bell_backspace() {
        let ops = parse_bytes(b"\x07\x08");
        assert_eq!(ops, vec![TerminalOp::Bell, TerminalOp::Backspace]);
    }

    #[test]
    fn test_parser_emulator_feed() {
        let mut emu = TerminalEmulatorHandle::new(80, 24);
        emu.feed_bytes(b"\x1b[2J\x1b[1;1HHello");
        let ops = emu.take_ops();
        assert_eq!(
            ops,
            vec![
                TerminalOp::EraseInDisplay(EraseMode::All),
                TerminalOp::CursorPosition { row: 1, col: 1 },
                TerminalOp::Print("Hello".to_string()),
            ]
        );
    }

    #[test]
    fn test_parser_emulator_flush() {
        let mut emu = TerminalEmulatorHandle::new(80, 24);
        emu.feed_bytes(b"pending text");
        assert!(emu.take_ops().is_empty());
        emu.flush_parser();
        let ops = emu.take_ops();
        assert_eq!(ops, vec![TerminalOp::Print("pending text".to_string())]);
    }

    // ─── CNL / CPL tests ──────────────────────────────────────────

    #[test]
    fn test_parser_cnl() {
        let ops = parse_bytes(b"\x1b[3E");
        assert_eq!(ops, vec![TerminalOp::CursorNextLine(3)]);
    }

    #[test]
    fn test_parser_cnl_default() {
        let ops = parse_bytes(b"\x1b[E");
        assert_eq!(ops, vec![TerminalOp::CursorNextLine(1)]);
    }

    #[test]
    fn test_parser_cpl() {
        let ops = parse_bytes(b"\x1b[5F");
        assert_eq!(ops, vec![TerminalOp::CursorPreviousLine(5)]);
    }

    #[test]
    fn test_parser_cpl_default() {
        let ops = parse_bytes(b"\x1b[F");
        assert_eq!(ops, vec![TerminalOp::CursorPreviousLine(1)]);
    }

    #[test]
    fn test_parser_dectcem() {
        let ops = parse_bytes(b"\x1b[?25l\x1b[?25h");
        assert_eq!(
            ops,
            vec![
                TerminalOp::DecPrivateModeReset(25), // ?25l = DECTCEM reset (hide cursor)
                TerminalOp::DecPrivateModeSet(25),   // ?25h = DECTCEM set (show cursor)
            ]
        );
    }
}
