//! Terminal screen/grid model for the terminal emulator.
//!
//! Implements a cell-based terminal screen with primary and alternate buffers,
//! cursor tracking, cell attributes, scroll regions, and scrollback history.
//! Consumes [`TerminalOp`] values produced by the ANSI parser and maintains
//! a structured representation of the visible terminal state.

use super::emulator::{EraseMode, TerminalOp};
use serde::Serialize;
use std::collections::VecDeque;
use std::hash::Hasher;
use tracing::{debug, warn};

// ─── Screen Diff ──────────────────────────────────────────────────

/// A single changed line within a screen diff.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ChangedLine {
    /// 0-based line index.
    pub row: usize,
    /// Content before the change.
    pub old: String,
    /// Content after the change.
    pub new: String,
}

/// Structured difference between two [`ScreenSnapshot`] instances.
///
/// Produced by [`ScreenSnapshot::diff`], this type reports only the lines,
/// cursor position, title, and mode that actually changed, making it suitable
/// for LLM consumption without transmitting the full screen on every frame.
#[derive(Debug, Clone, Serialize)]
pub struct ScreenDiff {
    /// Lines that differ between the two snapshots.
    pub changed_lines: Vec<ChangedLine>,
    /// `true` when the cursor moved.
    pub cursor_changed: bool,
    /// (old_row, old_col, new_row, new_col) when the cursor moved.
    pub cursor_delta: Option<(usize, usize, usize, usize)>,
    /// `true` when the active buffer (primary/alternate) switched.
    pub mode_changed: bool,
    /// New window title when it changed, `None` otherwise.
    pub title_changed: Option<String>,
    /// `true` when the screen dimensions changed.
    pub dims_changed: bool,
}

impl ScreenDiff {
    /// Returns `true` if nothing changed between the two snapshots.
    pub fn is_empty(&self) -> bool {
        self.changed_lines.is_empty()
            && !self.cursor_changed
            && !self.mode_changed
            && self.title_changed.is_none()
            && !self.dims_changed
    }
}

// ─── FNV-1a Hash (inline, no external dep) ────────────────────────

/// Minimal FNV-1a hasher for screen state hashing. Avoids pulling in a
/// separate crate for a simple change-detection use case.
struct FnvHasher(u64);

impl FnvHasher {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
}

impl Hasher for FnvHasher {
    fn write(&mut self, bytes: &[u8]) {
        const FNV_PRIME: u64 = 0x100000001b3;
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
    }
    fn write_u8(&mut self, i: u8) {
        self.write(&[i]);
    }
    fn write_usize(&mut self, i: usize) {
        self.write(&i.to_ne_bytes());
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

// ─── Cell & Attributes ─────────────────────────────────────────────

/// ANSI terminal color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    /// Default terminal color (inherited from terminal theme).
    #[default]
    Default,
    /// ANSI 256-color palette index (0–255).
    Index(u8),
}

/// Per-cell style attributes tracked by the emulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellAttributes {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

impl Default for CellAttributes {
    fn default() -> Self {
        Self {
            fg: Color::Default,
            bg: Color::Default,
            bold: false,
            italic: false,
            underline: false,
            reverse: false,
        }
    }
}

impl CellAttributes {
    /// Apply an SGR parameter list to the attributes, returning the updated state.
    pub fn apply_sgr(&mut self, params: &[u16]) {
        for &p in params {
            match p {
                0 => *self = CellAttributes::default(),
                1 => self.bold = true,
                3 => self.italic = true,
                4 => self.underline = true,
                7 => self.reverse = true,
                22 => self.bold = false,
                23 => self.italic = false,
                24 => self.underline = false,
                27 => self.reverse = false,
                30..=37 => self.fg = Color::Index((p - 30) as u8),
                39 => self.fg = Color::Default,
                40..=47 => self.bg = Color::Index((p - 40) as u8),
                49 => self.bg = Color::Default,
                // High-intensity FG (90–97)
                90..=97 => self.fg = Color::Index((p - 90 + 8) as u8),
                // High-intensity BG (100–107)
                100..=107 => self.bg = Color::Index((p - 100 + 8) as u8),
                _ => {}
            }
        }
    }
}

/// A single cell in the terminal grid.
#[derive(Debug, Clone)]
pub struct Cell {
    /// Character displayed in this cell.
    pub char: char,
    /// True if this cell is the second column of a wide character.
    pub wide: bool,
    /// Cell visual attributes.
    pub attrs: CellAttributes,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            char: ' ',
            wide: false,
            attrs: CellAttributes::default(),
        }
    }
}

impl Cell {
    #[allow(dead_code)]
    fn is_empty(&self) -> bool {
        self.char == ' ' && !self.wide && self.attrs == CellAttributes::default()
    }
}

// ─── Screen Buffer ─────────────────────────────────────────────────

/// A 2D grid of cells representing one screen buffer (primary or alternate).
pub struct ScreenBuffer {
    cells: Vec<Cell>,
    cols: usize,
    rows: usize,
}

impl ScreenBuffer {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            cells: vec![Cell::default(); cols * rows],
            cols,
            rows,
        }
    }

    pub fn resize(&mut self, new_cols: usize, new_rows: usize) {
        if new_cols == self.cols && new_rows == self.rows {
            return;
        }
        let mut new_cells = vec![Cell::default(); new_cols * new_rows];
        let copy_rows = self.rows.min(new_rows);
        let copy_cols = self.cols.min(new_cols);
        for row in 0..copy_rows {
            for col in 0..copy_cols {
                let src_idx = row * self.cols + col;
                let dst_idx = row * new_cols + col;
                new_cells[dst_idx] = self.cells[src_idx].clone();
            }
        }
        self.cells = new_cells;
        self.cols = new_cols;
        self.rows = new_rows;
    }

    #[inline]
    fn idx(&self, row: usize, col: usize) -> usize {
        row * self.cols + col
    }

    fn cell(&self, row: usize, col: usize) -> &Cell {
        &self.cells[self.idx(row, col)]
    }

    #[allow(dead_code)]
    fn cell_mut(&mut self, row: usize, col: usize) -> &mut Cell {
        let idx = self.idx(row, col);
        &mut self.cells[idx]
    }

    fn clear(&mut self) {
        for cell in &mut self.cells {
            *cell = Cell::default();
        }
    }

    fn clear_row(&mut self, row: usize) {
        let start = row * self.cols;
        for i in 0..self.cols {
            self.cells[start + i] = Cell::default();
        }
    }

    /// Erase cells in a row according to the erase mode.
    fn erase_line(&mut self, row: usize, col: usize, mode: EraseMode) {
        let start = row * self.cols;
        match mode {
            EraseMode::ToEnd => {
                for c in col..self.cols {
                    self.cells[start + c] = Cell::default();
                }
            }
            EraseMode::ToStart => {
                let end = col.min(self.cols - 1);
                for c in 0..=end {
                    self.cells[start + c] = Cell::default();
                }
            }
            EraseMode::All => {
                self.clear_row(row);
            }
        }
    }

    /// Erase cells in the display according to the erase mode.
    fn erase_display(&mut self, row: usize, col: usize, mode: EraseMode) {
        match mode {
            EraseMode::ToEnd => {
                // Cursor to end of current line
                self.erase_line(row, col, EraseMode::ToEnd);
                // All lines below
                for r in (row + 1)..self.rows {
                    self.clear_row(r);
                }
            }
            EraseMode::ToStart => {
                // All lines above
                for r in 0..row {
                    self.clear_row(r);
                }
                // Start of current line to cursor
                self.erase_line(row, col, EraseMode::ToStart);
            }
            EraseMode::All => {
                self.clear();
            }
        }
    }

    /// Scroll the buffer up by n lines within [top, bottom] region.
    fn scroll_up(&mut self, n: usize, top: usize, bottom: usize) {
        if top >= bottom || n == 0 {
            return;
        }
        let n = n.min(bottom - top);
        // Shift rows up
        for row in top..=(bottom - n) {
            let src_start = (row + n) * self.cols;
            let dst_start = row * self.cols;
            for i in 0..self.cols {
                self.cells[dst_start + i] = self.cells[src_start + i].clone();
            }
        }
        // Clear the vacated rows at the bottom of the region
        for row in (bottom - n + 1)..=bottom {
            self.clear_row(row);
        }
    }

    /// Scroll the buffer down by n lines within [top, bottom] region.
    fn scroll_down(&mut self, n: usize, top: usize, bottom: usize) {
        if top >= bottom || n == 0 {
            return;
        }
        let n = n.min(bottom - top);
        // Shift rows down (iterate in reverse to avoid overwriting)
        for row in (top..=(bottom - n)).rev() {
            let src_start = row * self.cols;
            let dst_start = (row + n) * self.cols;
            for i in 0..self.cols {
                self.cells[dst_start + i] = self.cells[src_start + i].clone();
            }
        }
        // Clear the vacated rows at the top of the region
        for row in top..(top + n) {
            self.clear_row(row);
        }
    }

    /// Extract a row of cells as a string of visible characters.
    fn row_to_string(&self, row: usize) -> String {
        let mut s = String::with_capacity(self.cols);
        let mut col = 0;
        while col < self.cols {
            let cell = self.cell(row, col);
            if cell.wide {
                col += 1;
                continue;
            }
            s.push(cell.char);
            col += 1;
        }
        s.trim_end_matches(' ').to_string()
    }
}

// ─── Cursor State ──────────────────────────────────────────────────

/// Current and saved cursor state.
#[derive(Debug, Clone)]
struct CursorState {
    row: usize,
    col: usize,
    saved_row: usize,
    saved_col: usize,
    wrap_pending: bool,
}

impl CursorState {
    fn new() -> Self {
        Self {
            row: 0,
            col: 0,
            saved_row: 0,
            saved_col: 0,
            wrap_pending: false,
        }
    }

    fn save(&mut self) {
        self.saved_row = self.row;
        self.saved_col = self.col;
    }

    fn restore(&mut self) {
        self.row = self.saved_row;
        self.col = self.saved_col;
        self.wrap_pending = false;
    }
}

// ─── Character Width ───────────────────────────────────────────────

/// Determine the display width of a character (1 or 2 columns).
///
/// Handles the most common cases: CJK ideographs, Hangul, fullwidth forms,
/// and emoji. Returns 0 for control characters.
fn char_width(c: char) -> usize {
    if c < '\u{20}' {
        return 0;
    }
    if c < '\u{1100}' {
        return 1;
    }
    // Fullwidth and wide ranges based on Unicode east asian width
    match c {
        // Hangul Jamo
        '\u{1100}'..='\u{115F}' => 2,
        // CJK Radicals Supplement, Kangxi Radicals, CJK Symbols and Punctuation
        '\u{2E80}'..='\u{303E}' => 2,
        // Hiragana, Katakana, Bopomofo
        '\u{3040}'..='\u{33FF}' => 2,
        // CJK Unified Ideographs
        '\u{4E00}'..='\u{9FFF}' => 2,
        // Hangul Syllables
        '\u{AC00}'..='\u{D7AF}' => 2,
        // CJK Compatibility Ideographs
        '\u{F900}'..='\u{FAFF}' => 2,
        // Halfwidth and Fullwidth Forms
        '\u{FF01}'..='\u{FF60}' => 2,
        // Supplementary CJK
        '\u{20000}'..='\u{2FFFD}' => 2,
        '\u{30000}'..='\u{3FFFD}' => 2,
        // Emoji (simplified — most common ranges)
        '\u{1F600}'..='\u{1F64F}' => 2,
        '\u{1F300}'..='\u{1F5FF}' => 2,
        '\u{1F680}'..='\u{1F6FF}' => 2,
        '\u{1F900}'..='\u{1F9FF}' => 2,
        '\u{1FA00}'..='\u{1FA6F}' => 2,
        '\u{1FA70}'..='\u{1FAFF}' => 2,
        '\u{2600}'..='\u{27BF}' => 2,
        // Variations and tags
        '\u{FE00}'..='\u{FE0F}' => 0, // Variation selectors are zero-width
        '\u{200D}' => 0,              // ZWJ is zero-width
        _ => 1,
    }
}

// ─── Terminal Screen ───────────────────────────────────────────────

/// Which screen buffer is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveBuffer {
    Primary,
    Alternate,
}

/// The terminal screen model: owns primary/alternate buffers, cursor, attributes,
/// scroll region, and scrollback history. Consumes [`TerminalOp`] values to
/// maintain a structured representation of the terminal state.
pub struct TerminalScreen {
    primary: ScreenBuffer,
    alternate: ScreenBuffer,
    active: ActiveBuffer,
    cursor: CursorState,
    cursor_visible: bool,
    scroll_top: usize,
    scroll_bottom: usize,
    attrs: CellAttributes,
    autowrap: bool,
    origin_mode: bool,
    last_printed_char: char,
    scrollback: VecDeque<Vec<Cell>>,
    max_scrollback: usize,
    window_title: String,
}

/// Default maximum scrollback buffer size (lines).
pub const DEFAULT_MAX_SCROLLBACK: usize = 20_000;

impl TerminalScreen {
    pub fn new(cols: usize, rows: usize) -> Self {
        let scroll_bottom = rows.saturating_sub(1);
        Self {
            primary: ScreenBuffer::new(cols, rows),
            alternate: ScreenBuffer::new(cols, rows),
            active: ActiveBuffer::Primary,
            cursor: CursorState::new(),
            cursor_visible: true,
            scroll_top: 0,
            scroll_bottom,
            attrs: CellAttributes::default(),
            autowrap: true,
            origin_mode: false,
            last_printed_char: ' ',
            scrollback: VecDeque::new(),
            max_scrollback: DEFAULT_MAX_SCROLLBACK,
            window_title: String::new(),
        }
    }

    pub fn cols(&self) -> usize {
        self.active_buf().cols
    }

    pub fn rows(&self) -> usize {
        self.active_buf().rows
    }

    /// Set the maximum scrollback buffer size in lines.
    /// If the current scrollback exceeds the new limit, excess lines are discarded.
    pub fn set_max_scrollback(&mut self, max: usize) {
        self.max_scrollback = max;
        while self.scrollback.len() > self.max_scrollback {
            self.scrollback.pop_front();
        }
    }

    /// Resize the screen model to new dimensions.
    pub fn resize(&mut self, new_cols: usize, new_rows: usize) {
        self.primary.resize(new_cols, new_rows);
        self.alternate.resize(new_cols, new_rows);
        self.scroll_bottom = new_rows.saturating_sub(1);
        // Clamp cursor to new dimensions
        self.cursor.row = self.cursor.row.min(new_rows.saturating_sub(1));
        self.cursor.col = self.cursor.col.min(new_cols.saturating_sub(1));
    }

    /// Process a terminal operation, mutating screen state.
    pub fn process_op(&mut self, op: &TerminalOp) {
        match op {
            TerminalOp::Print(text) => self.print_text(text),
            TerminalOp::Repeat(n) => {
                let c = self.last_printed_char;
                for _ in 0..*n as usize {
                    self.put_char(c);
                }
            }
            TerminalOp::Linefeed => self.linefeed(),
            TerminalOp::CarriageReturn => {
                self.cursor.wrap_pending = false;
                self.cursor.col = 0;
            }
            TerminalOp::Backspace => {
                self.cursor.wrap_pending = false;
                if self.cursor.col > 0 {
                    self.cursor.col -= 1;
                }
            }
            TerminalOp::Tab => {
                self.cursor.wrap_pending = false;
                self.tab();
            }
            TerminalOp::Bell => {}
            TerminalOp::CursorUp(n) => {
                self.cursor.wrap_pending = false;
                self.cursor_up(*n as usize);
            }
            TerminalOp::CursorDown(n) => {
                self.cursor.wrap_pending = false;
                self.cursor_down(*n as usize);
            }
            TerminalOp::CursorForward(n) => {
                self.cursor.wrap_pending = false;
                self.cursor_forward(*n as usize);
            }
            TerminalOp::CursorBack(n) => {
                self.cursor.wrap_pending = false;
                self.cursor_back(*n as usize);
            }
            TerminalOp::CursorPosition { row, col } => {
                self.cursor.wrap_pending = false;
                let max_rows = self.active_buf().rows;
                let max_cols = self.active_buf().cols;
                let base_row = if self.origin_mode { self.scroll_top } else { 0 };
                let row_limit = if self.origin_mode {
                    self.scroll_bottom + 1
                } else {
                    max_rows
                };
                let target_row = base_row
                    + ((*row as usize).max(1) - 1).min(row_limit.saturating_sub(1) - base_row);
                self.cursor.row = target_row.min(max_rows.saturating_sub(1));
                self.cursor.col = ((*col as usize).max(1) - 1).min(max_cols.saturating_sub(1));
            }
            TerminalOp::CursorHorizontalAbsolute(col) => {
                self.cursor.wrap_pending = false;
                let max_cols = self.active_buf().cols;
                self.cursor.col = ((*col as usize).max(1) - 1).min(max_cols.saturating_sub(1));
            }
            TerminalOp::CursorVerticalAbsolute(row) => {
                self.cursor.wrap_pending = false;
                let max_rows = self.active_buf().rows;
                self.cursor.row = ((*row as usize).max(1) - 1).min(max_rows.saturating_sub(1));
            }
            TerminalOp::CursorNextLine(n) => {
                self.cursor.wrap_pending = false;
                let max_rows = self.active_buf().rows;
                self.cursor.row = (self.cursor.row + *n as usize).min(max_rows.saturating_sub(1));
                self.cursor.col = 0;
            }
            TerminalOp::CursorPreviousLine(n) => {
                self.cursor.wrap_pending = false;
                self.cursor.row = self.cursor.row.saturating_sub(*n as usize);
                self.cursor.col = 0;
            }
            TerminalOp::EraseInDisplay(mode) => {
                let (row, col) = (self.cursor.row, self.cursor.col);
                self.active_buf_mut().erase_display(row, col, *mode);
            }
            TerminalOp::EraseInLine(mode) => {
                let (row, col) = (self.cursor.row, self.cursor.col);
                self.active_buf_mut().erase_line(row, col, *mode);
            }
            TerminalOp::SetGraphicRendition(params) => {
                self.attrs.apply_sgr(params);
            }
            TerminalOp::SaveCursor => self.cursor.save(),
            TerminalOp::RestoreCursor => self.cursor.restore(),
            TerminalOp::ScrollUp(n) => self.scroll_up(*n as usize),
            TerminalOp::ScrollDown(n) => self.scroll_down(*n as usize),
            TerminalOp::SetScrollingRegion { top, bottom } => {
                let max_rows = self.active_buf().rows;
                let top = if *top == 0 {
                    0
                } else {
                    (*top as usize).max(1) - 1
                };
                let bottom = if *bottom == 0 {
                    max_rows.saturating_sub(1)
                } else {
                    (*bottom as usize).max(1).min(max_rows) - 1
                };
                if top < bottom {
                    self.scroll_top = top;
                    self.scroll_bottom = bottom;
                }
            }
            TerminalOp::DecPrivateModeSet(mode) => {
                match *mode {
                    1049 => self.enter_alternate_screen(),
                    25 => {
                        // ?25h (DECTCEM set) = show cursor
                        self.cursor_visible = true;
                    }
                    7 => {
                        // DECAWM — auto-wrap mode on
                        self.autowrap = true;
                    }
                    6 => {
                        // DECOM — origin mode on, cursor moves to home
                        self.origin_mode = true;
                        self.cursor.row = self.scroll_top;
                        self.cursor.col = 0;
                        self.cursor.wrap_pending = false;
                    }
                    // 2004 = Bracketed Paste — acknowledged but not enforced at screen level.
                    // Input handling in send_input wraps pasted content with \x1b[200~ / \x1b[201~.
                    2004 => {
                        debug!(mode = *mode, "Bracketed paste mode enabled (acknowledged)");
                    }
                    // Mouse tracking modes — not supported, log for diagnostics.
                    1000 | 1002 | 1003 | 1004 | 1006 | 1015 => {
                        warn!(
                            mode = *mode,
                            "Mouse/focus tracking mode requested but not supported; \
                             keyboard-only interaction may be limited"
                        );
                    }
                    _ => {
                        debug!(mode = *mode, "Unhandled DEC private mode set");
                    }
                }
            }
            TerminalOp::DecPrivateModeReset(mode) => match *mode {
                1049 => self.leave_alternate_screen(),
                25 => {
                    // ?25l (DECTCEM reset) = hide cursor
                    self.cursor_visible = false;
                }
                7 => {
                    // DECAWM — auto-wrap mode off
                    self.autowrap = false;
                    self.cursor.wrap_pending = false;
                }
                6 => {
                    // DECOM — origin mode off, cursor moves to home
                    self.origin_mode = false;
                    self.cursor.row = 0;
                    self.cursor.col = 0;
                    self.cursor.wrap_pending = false;
                }
                _ => {}
            },
            TerminalOp::SetWindowTitle(title) => {
                self.window_title = title.clone();
            }
            TerminalOp::InsertLine(n) => {
                let n = *n as usize;
                let bottom = self.scroll_bottom;
                let top = self.cursor.row;
                if top < bottom {
                    let count = n.min(bottom - top);
                    let buf = self.active_buf_mut();
                    // Shift lines down, losing the bottom-most lines
                    for row in (top + count..=bottom).rev() {
                        let src = row - count;
                        for col in 0..buf.cols {
                            let src_cell = buf.cell(src, col).clone();
                            *buf.cell_mut(row, col) = src_cell;
                        }
                    }
                    // Clear the vacated rows
                    for row in top..top + count {
                        buf.clear_row(row);
                    }
                }
            }
            TerminalOp::DeleteLine(n) => {
                let n = *n as usize;
                let bottom = self.scroll_bottom;
                let top = self.cursor.row;
                if top < bottom {
                    let count = n.min(bottom - top);
                    let buf = self.active_buf_mut();
                    // Shift lines up
                    for row in top..=bottom - count {
                        let src = row + count;
                        for col in 0..buf.cols {
                            let src_cell = buf.cell(src, col).clone();
                            *buf.cell_mut(row, col) = src_cell;
                        }
                    }
                    // Clear the vacated rows at the bottom
                    for row in bottom + 1 - count..=bottom {
                        buf.clear_row(row);
                    }
                }
            }
            TerminalOp::InsertCharacter(n) => {
                let n = *n as usize;
                let row = self.cursor.row;
                let col = self.cursor.col;
                let buf = self.active_buf_mut();
                if col < buf.cols {
                    let count = n.min(buf.cols - col);
                    // Shift cells right
                    for c in (col + count..buf.cols).rev() {
                        let src = c - count;
                        let src_cell = buf.cell(row, src).clone();
                        *buf.cell_mut(row, c) = src_cell;
                    }
                    // Clear inserted cells
                    for c in col..col + count {
                        *buf.cell_mut(row, c) = Cell::default();
                    }
                }
            }
            TerminalOp::DeleteCharacter(n) => {
                let n = *n as usize;
                let row = self.cursor.row;
                let col = self.cursor.col;
                let buf = self.active_buf_mut();
                if col < buf.cols {
                    let count = n.min(buf.cols - col);
                    // Shift cells left
                    for c in col..buf.cols - count {
                        let src = c + count;
                        let src_cell = buf.cell(row, src).clone();
                        *buf.cell_mut(row, c) = src_cell;
                    }
                    // Clear vacated cells at the right
                    for c in buf.cols - count..buf.cols {
                        *buf.cell_mut(row, c) = Cell::default();
                    }
                }
            }
            TerminalOp::DeviceAttributes(_) => {
                // DA response — no screen action needed
            }
        }
    }

    /// Take a snapshot of the current visible screen state.
    pub fn snapshot(&self) -> ScreenSnapshot {
        let buf = self.active_buf();
        let mut lines = Vec::with_capacity(buf.rows);
        for row in 0..buf.rows {
            lines.push(buf.row_to_string(row));
        }
        ScreenSnapshot {
            lines,
            cursor_row: self.cursor.row,
            cursor_col: self.cursor.col,
            cols: buf.cols,
            rows: buf.rows,
            is_alternate: self.active == ActiveBuffer::Alternate,
            window_title: self.window_title.clone(),
        }
    }

    /// Compute a lightweight hash of the current screen state for change detection.
    ///
    /// Uses a simple FNV-1a-inspired hash over all cell characters, cursor position,
    /// active buffer flag, and window title. This is intentionally fast rather than
    /// cryptographically strong — its purpose is to detect *any* visible mutation.
    pub fn state_hash(&self) -> u64 {
        use std::hash::Hasher;
        let mut h = FnvHasher::new(0xcbf29ce484222325);
        let buf = self.active_buf();
        for row in 0..buf.rows {
            let mut col = 0;
            while col < buf.cols {
                let cell = buf.cell(row, col);
                if cell.wide {
                    col += 1;
                    continue;
                }
                let mut char_buf = [0u8; 4];
                let s = cell.char.encode_utf8(&mut char_buf);
                h.write(s.as_bytes());
                col += 1;
            }
            h.write_u8(b'\n');
        }
        h.write_usize(self.cursor.row);
        h.write_usize(self.cursor.col);
        h.write_u8(if self.active == ActiveBuffer::Alternate {
            1
        } else {
            0
        });
        for b in self.window_title.as_bytes() {
            h.write_u8(*b);
        }
        h.write_u8(if self.cursor_visible { 1 } else { 0 });
        h.write_u8(if self.cursor.wrap_pending { 1 } else { 0 });
        h.finish()
    }

    /// Number of lines in the scrollback buffer.
    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    /// Return the last `max_lines` scrollback lines as plain text strings.
    ///
    /// Lines are returned in chronological order (oldest first). If `max_lines`
    /// exceeds the available scrollback, all lines are returned.
    pub fn scrollback_lines(&self, max_lines: usize) -> Vec<String> {
        let skip = self.scrollback.len().saturating_sub(max_lines);
        self.scrollback
            .iter()
            .skip(skip)
            .map(|row| {
                let mut s = String::with_capacity(row.len());
                let mut col = 0;
                while col < row.len() {
                    let cell = &row[col];
                    if cell.wide {
                        col += 1;
                        continue;
                    }
                    s.push(cell.char);
                    col += 1;
                }
                s.trim_end_matches(' ').to_string()
            })
            .collect()
    }

    // ─── Internal helpers ───────────────────────────────────────

    fn active_buf(&self) -> &ScreenBuffer {
        match self.active {
            ActiveBuffer::Primary => &self.primary,
            ActiveBuffer::Alternate => &self.alternate,
        }
    }

    fn active_buf_mut(&mut self) -> &mut ScreenBuffer {
        match self.active {
            ActiveBuffer::Primary => &mut self.primary,
            ActiveBuffer::Alternate => &mut self.alternate,
        }
    }

    fn print_text(&mut self, text: &str) {
        for c in text.chars() {
            self.put_char(c);
        }
    }

    fn put_char(&mut self, c: char) {
        let w = char_width(c);
        if w == 0 {
            return;
        }

        let (max_cols, max_rows) = {
            let buf = self.active_buf();
            (buf.cols, buf.rows)
        };
        let scroll_bottom = self.scroll_bottom;

        // If wrap is pending from a previous character, perform the wrap now
        if self.cursor.wrap_pending {
            self.cursor.wrap_pending = false;
            self.cursor.col = 0;
            if self.cursor.row == scroll_bottom {
                self.scroll_up(1);
            } else if self.cursor.row < max_rows - 1 {
                self.cursor.row += 1;
            }
        }

        // If character doesn't fit on current line and autowrap is on, wrap first
        if self.cursor.col + w > max_cols {
            if self.autowrap {
                self.cursor.col = 0;
                if self.cursor.row == scroll_bottom {
                    self.scroll_up(1);
                } else if self.cursor.row < max_rows - 1 {
                    self.cursor.row += 1;
                }
            }
        }

        // Clamp write position if still past end (autowrap off or wide char)
        let write_col = if self.cursor.col + w > max_cols {
            max_cols.saturating_sub(w)
        } else {
            self.cursor.col
        };

        let row = self.cursor.row;
        let attrs = self.attrs;

        // Clear any existing wide-character cells overlapped by this write.
        {
            let buf = self.active_buf_mut();
            let idx = row * max_cols + write_col;

            if buf.cells[idx].wide && write_col > 0 {
                let prev_idx = row * max_cols + (write_col - 1);
                buf.cells[prev_idx] = Cell::default();
                buf.cells[idx] = Cell::default();
            } else if write_col + 1 < max_cols && buf.cells[write_col + 1].wide {
                buf.cells[write_col + 1] = Cell::default();
            }

            if w == 2 && write_col + 1 < max_cols {
                buf.cells[write_col + 1] = Cell::default();
            }
        }

        // Write the character and continuation
        {
            let buf = self.active_buf_mut();
            let idx = row * max_cols + write_col;
            buf.cells[idx].char = c;
            buf.cells[idx].wide = false;
            buf.cells[idx].attrs = attrs;

            if w == 2 && write_col + 1 < max_cols {
                let cont_idx = row * max_cols + write_col + 1;
                buf.cells[cont_idx].char = ' ';
                buf.cells[cont_idx].wide = true;
                buf.cells[cont_idx].attrs = attrs;
            }
        }

        // Advance cursor
        self.cursor.col = write_col + w;

        // Set wrap_pending if cursor reached or passed end of line
        if self.cursor.col >= max_cols {
            if self.autowrap {
                self.cursor.wrap_pending = true;
            }
            self.cursor.col = max_cols.saturating_sub(1);
        }

        self.last_printed_char = c;
    }

    fn linefeed(&mut self) {
        if self.cursor.row == self.scroll_bottom {
            self.scroll_up(1);
        } else if self.cursor.row < self.active_buf().rows - 1 {
            self.cursor.row += 1;
        }
    }

    fn tab(&mut self) {
        let next_tab = ((self.cursor.col / 8) + 1) * 8;
        let max_cols = self.active_buf().cols;
        self.cursor.col = next_tab.min(max_cols.saturating_sub(1));
    }

    fn cursor_up(&mut self, n: usize) {
        self.cursor.row = self.cursor.row.saturating_sub(n);
    }

    fn cursor_down(&mut self, n: usize) {
        let max_rows = self.active_buf().rows;
        self.cursor.row = (self.cursor.row + n).min(max_rows.saturating_sub(1));
    }

    fn cursor_forward(&mut self, n: usize) {
        let max_cols = self.active_buf().cols;
        self.cursor.col = (self.cursor.col + n).min(max_cols.saturating_sub(1));
    }

    fn cursor_back(&mut self, n: usize) {
        self.cursor.col = self.cursor.col.saturating_sub(n);
    }

    fn scroll_up(&mut self, n: usize) {
        let scroll_top = self.scroll_top;
        let scroll_bottom = self.scroll_bottom;
        let is_primary = self.active == ActiveBuffer::Primary;

        // Save scrolled-out line to scrollback (primary buffer only)
        if is_primary && scroll_top == 0 {
            let cols = self.active_buf().cols;
            for _ in 0..n {
                let mut line = Vec::with_capacity(cols);
                for col in 0..cols {
                    let cell = self.active_buf().cell(scroll_top, col).clone();
                    line.push(cell);
                }
                while matches!(line.last(), Some(cell) if cell.char == ' ' && !cell.wide && cell.attrs == CellAttributes::default())
                {
                    line.pop();
                }
                self.scrollback.push_back(line);
                // Enforce scrollback limit
                while self.scrollback.len() > self.max_scrollback {
                    self.scrollback.remove(0);
                }
            }
        }

        self.active_buf_mut()
            .scroll_up(n, scroll_top, scroll_bottom);
    }

    fn scroll_down(&mut self, n: usize) {
        let scroll_top = self.scroll_top;
        let scroll_bottom = self.scroll_bottom;
        self.active_buf_mut()
            .scroll_down(n, scroll_top, scroll_bottom);
    }

    fn enter_alternate_screen(&mut self) {
        if self.active == ActiveBuffer::Alternate {
            return;
        }
        self.alternate.clear();
        self.cursor = CursorState::new();
        self.active = ActiveBuffer::Alternate;
        self.scroll_top = 0;
        self.scroll_bottom = self.alternate.rows.saturating_sub(1);
    }

    fn leave_alternate_screen(&mut self) {
        if self.active == ActiveBuffer::Primary {
            return;
        }
        self.active = ActiveBuffer::Primary;
        self.scroll_top = 0;
        self.scroll_bottom = self.primary.rows.saturating_sub(1);
    }
}

// ─── Screen Snapshot ───────────────────────────────────────────────

/// Immutable snapshot of the current terminal screen state.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ScreenSnapshot {
    /// Visible lines (one String per row, trailing spaces stripped).
    pub lines: Vec<String>,
    /// Cursor row (0-based).
    pub cursor_row: usize,
    /// Cursor column (0-based).
    pub cursor_col: usize,
    /// Screen width in columns.
    pub cols: usize,
    /// Screen height in rows.
    pub rows: usize,
    /// Whether the alternate screen is active.
    pub is_alternate: bool,
    /// Current window title.
    pub window_title: String,
}

impl ScreenSnapshot {
    /// Compute a structured diff between `self` (old) and `other` (new).
    ///
    /// Returns a [`ScreenDiff`] enumerating every line that changed, cursor
    /// movement, buffer mode switches, window title changes, and dimension
    /// changes. Intended for LLM consumption: callers can present only the
    /// delta rather than retransmitting the full screen.
    pub fn diff(&self, other: &ScreenSnapshot) -> ScreenDiff {
        let max_rows = self.lines.len().max(other.lines.len());
        let mut changed_lines = Vec::new();
        for row in 0..max_rows {
            let old_line = self.lines.get(row).map(|s| s.as_str()).unwrap_or("");
            let new_line = other.lines.get(row).map(|s| s.as_str()).unwrap_or("");
            if old_line != new_line {
                changed_lines.push(ChangedLine {
                    row,
                    old: old_line.to_string(),
                    new: new_line.to_string(),
                });
            }
        }

        let cursor_changed =
            self.cursor_row != other.cursor_row || self.cursor_col != other.cursor_col;
        let cursor_delta = if cursor_changed {
            Some((
                self.cursor_row,
                self.cursor_col,
                other.cursor_row,
                other.cursor_col,
            ))
        } else {
            None
        };

        let mode_changed = self.is_alternate != other.is_alternate;
        let title_changed = if self.window_title != other.window_title {
            Some(other.window_title.clone())
        } else {
            None
        };
        let dims_changed = self.cols != other.cols || self.rows != other.rows;

        ScreenDiff {
            changed_lines,
            cursor_changed,
            cursor_delta,
            mode_changed,
            title_changed,
            dims_changed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_screen() -> TerminalScreen {
        TerminalScreen::new(80, 24)
    }

    // ─── Cursor movement tests ──────────────────────────────────

    #[test]
    fn test_cursor_position() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::CursorPosition { row: 5, col: 10 });
        assert_eq!(s.cursor.row, 4);
        assert_eq!(s.cursor.col, 9);
    }

    #[test]
    fn test_cursor_position_clamp() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::CursorPosition { row: 0, col: 0 });
        assert_eq!(s.cursor.row, 0);
        assert_eq!(s.cursor.col, 0);
        s.process_op(&TerminalOp::CursorPosition { row: 100, col: 100 });
        assert_eq!(s.cursor.row, 23);
        assert_eq!(s.cursor.col, 79);
    }

    #[test]
    fn test_cursor_relative() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::CursorPosition { row: 10, col: 20 });
        s.process_op(&TerminalOp::CursorUp(3));
        assert_eq!(s.cursor.row, 6);
        s.process_op(&TerminalOp::CursorDown(5));
        assert_eq!(s.cursor.row, 11);
        s.process_op(&TerminalOp::CursorForward(7));
        assert_eq!(s.cursor.col, 26);
        s.process_op(&TerminalOp::CursorBack(4));
        assert_eq!(s.cursor.col, 22);
    }

    #[test]
    fn test_cursor_relative_clamp() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::CursorBack(100));
        assert_eq!(s.cursor.col, 0);
        s.process_op(&TerminalOp::CursorUp(100));
        assert_eq!(s.cursor.row, 0);
        s.process_op(&TerminalOp::CursorDown(100));
        assert_eq!(s.cursor.row, 23);
        s.process_op(&TerminalOp::CursorForward(100));
        assert_eq!(s.cursor.col, 79);
    }

    #[test]
    fn test_cursor_save_restore() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::CursorPosition { row: 5, col: 10 });
        s.process_op(&TerminalOp::SaveCursor);
        s.process_op(&TerminalOp::CursorPosition { row: 1, col: 1 });
        s.process_op(&TerminalOp::RestoreCursor);
        assert_eq!(s.cursor.row, 4);
        assert_eq!(s.cursor.col, 9);
    }

    #[test]
    fn test_cha_vpa() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::CursorHorizontalAbsolute(40));
        assert_eq!(s.cursor.col, 39);
        s.process_op(&TerminalOp::CursorVerticalAbsolute(12));
        assert_eq!(s.cursor.row, 11);
    }

    // ─── Print and wrap tests ───────────────────────────────────

    #[test]
    fn test_print_basic() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::Print("Hello".to_string()));
        let snap = s.snapshot();
        assert_eq!(snap.lines[0], "Hello");
    }

    #[test]
    fn test_print_overwrite() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::Print("ABCDE".to_string()));
        s.process_op(&TerminalOp::CarriageReturn);
        s.process_op(&TerminalOp::Print("XY".to_string()));
        let snap = s.snapshot();
        assert_eq!(snap.lines[0], "XYCDE");
    }

    #[test]
    fn test_print_wrap() {
        let mut s = TerminalScreen::new(5, 3);
        s.process_op(&TerminalOp::Print("ABCDEFGH".to_string()));
        let snap = s.snapshot();
        assert_eq!(snap.lines[0], "ABCDE");
        // After wrapping, cursor is on row 1 with "FGH"
        assert_eq!(snap.lines[1], "FGH");
    }

    #[test]
    fn test_linefeed_scroll() {
        let mut s = TerminalScreen::new(10, 3);
        s.process_op(&TerminalOp::Print("AAA".to_string()));
        s.process_op(&TerminalOp::CarriageReturn);
        s.process_op(&TerminalOp::Linefeed);
        s.process_op(&TerminalOp::Print("BBB".to_string()));
        s.process_op(&TerminalOp::CarriageReturn);
        s.process_op(&TerminalOp::Linefeed);
        s.process_op(&TerminalOp::Print("CCC".to_string()));
        // Cursor is at bottom row; next linefeed scrolls
        s.process_op(&TerminalOp::CarriageReturn);
        s.process_op(&TerminalOp::Linefeed);
        s.process_op(&TerminalOp::Print("DDD".to_string()));
        let snap = s.snapshot();
        // "AAA" scrolled off, "BBB" at top, "CCC" middle, "DDD" bottom
        assert_eq!(snap.lines[0], "BBB");
        assert_eq!(snap.lines[1], "CCC");
        assert_eq!(snap.lines[2], "DDD");
    }

    #[test]
    fn test_carriage_return_linefeed() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::Print("Hello".to_string()));
        s.process_op(&TerminalOp::CarriageReturn);
        s.process_op(&TerminalOp::Linefeed);
        s.process_op(&TerminalOp::Print("World".to_string()));
        let snap = s.snapshot();
        assert_eq!(snap.lines[0], "Hello");
        assert_eq!(snap.lines[1], "World");
    }

    #[test]
    fn test_linefeed_preserves_column() {
        let mut s = TerminalScreen::new(10, 3);
        s.process_op(&TerminalOp::Print("ABC".to_string()));
        // cursor at col 3
        s.process_op(&TerminalOp::Linefeed);
        // LF should move to row 1 but keep col at 3
        assert_eq!(s.cursor.row, 1);
        assert_eq!(s.cursor.col, 3);
        s.process_op(&TerminalOp::Print("DEF".to_string()));
        let snap = s.snapshot();
        assert_eq!(snap.lines[0], "ABC");
        assert_eq!(snap.lines[1], "   DEF");
    }

    #[test]
    fn test_cr_lf_resets_column() {
        let mut s = TerminalScreen::new(10, 3);
        s.process_op(&TerminalOp::Print("ABC".to_string()));
        s.process_op(&TerminalOp::CarriageReturn);
        s.process_op(&TerminalOp::Linefeed);
        assert_eq!(s.cursor.row, 1);
        assert_eq!(s.cursor.col, 0);
        s.process_op(&TerminalOp::Print("DEF".to_string()));
        let snap = s.snapshot();
        assert_eq!(snap.lines[0], "ABC");
        assert_eq!(snap.lines[1], "DEF");
    }

    // ─── Erase tests ────────────────────────────────────────────

    #[test]
    fn test_erase_in_display_to_end() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::Print("Hello World".to_string()));
        s.process_op(&TerminalOp::CarriageReturn);
        s.process_op(&TerminalOp::Linefeed);
        s.process_op(&TerminalOp::Print("Second line".to_string()));
        // Move cursor to row 0, col 5
        s.process_op(&TerminalOp::CursorPosition { row: 1, col: 6 });
        s.process_op(&TerminalOp::EraseInDisplay(EraseMode::ToEnd));
        let snap = s.snapshot();
        assert_eq!(snap.lines[0], "Hello");
        assert_eq!(snap.lines[1], "");
    }

    #[test]
    fn test_erase_in_display_to_start() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::Print("Hello".to_string()));
        s.process_op(&TerminalOp::CarriageReturn);
        s.process_op(&TerminalOp::Linefeed);
        s.process_op(&TerminalOp::Print("World".to_string()));
        s.process_op(&TerminalOp::CursorPosition { row: 2, col: 3 });
        s.process_op(&TerminalOp::EraseInDisplay(EraseMode::ToStart));
        let snap = s.snapshot();
        // Row 0 cleared, row 1 chars 0..=2 cleared
        assert_eq!(snap.lines[0], "");
        assert!(snap.lines[1].starts_with("   "));
    }

    #[test]
    fn test_erase_in_display_all() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::Print("Hello".to_string()));
        s.process_op(&TerminalOp::EraseInDisplay(EraseMode::All));
        let snap = s.snapshot();
        assert_eq!(snap.lines[0], "");
    }

    #[test]
    fn test_erase_in_line() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::Print("Hello World".to_string()));
        s.process_op(&TerminalOp::CursorPosition { row: 1, col: 6 });
        s.process_op(&TerminalOp::EraseInLine(EraseMode::ToEnd));
        let snap = s.snapshot();
        assert_eq!(snap.lines[0], "Hello");
    }

    #[test]
    fn test_erase_in_line_to_start() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::Print("Hello".to_string()));
        s.process_op(&TerminalOp::CursorPosition { row: 1, col: 3 });
        s.process_op(&TerminalOp::EraseInLine(EraseMode::ToStart));
        let snap = s.snapshot();
        // chars 0..=2 cleared, chars 3.. remain
        assert_eq!(&snap.lines[0][3..], "lo");
    }

    // ─── SGR tests ─────────────────────────────────────────────

    #[test]
    fn test_sgr_bold() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::SetGraphicRendition(vec![1]));
        s.process_op(&TerminalOp::Print("X".to_string()));
        let cell = s.active_buf().cell(0, 0);
        assert!(cell.attrs.bold);
    }

    #[test]
    fn test_sgr_reset() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::SetGraphicRendition(vec![1, 31]));
        s.process_op(&TerminalOp::Print("A".to_string()));
        s.process_op(&TerminalOp::SetGraphicRendition(vec![0]));
        s.process_op(&TerminalOp::Print("B".to_string()));
        let a = s.active_buf().cell(0, 0);
        assert!(a.attrs.bold);
        assert_eq!(a.attrs.fg, Color::Index(1));
        let b = s.active_buf().cell(0, 1);
        assert!(!b.attrs.bold);
        assert_eq!(b.attrs.fg, Color::Default);
    }

    #[test]
    fn test_sgr_colors() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::SetGraphicRendition(vec![33, 44]));
        s.process_op(&TerminalOp::Print("C".to_string()));
        let cell = s.active_buf().cell(0, 0);
        assert_eq!(cell.attrs.fg, Color::Index(3)); // 33 - 30 = 3
        assert_eq!(cell.attrs.bg, Color::Index(4)); // 44 - 40 = 4
    }

    // ─── Alternate screen tests ────────────────────────────────

    #[test]
    fn test_alternate_screen_switch() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::Print("Primary content".to_string()));
        // Enter alternate screen
        s.process_op(&TerminalOp::DecPrivateModeSet(1049));
        assert_eq!(s.active, ActiveBuffer::Alternate);
        let snap = s.snapshot();
        assert!(snap.is_alternate);
        assert_eq!(snap.lines[0], ""); // Alternate is cleared on enter
                                       // Write in alternate
        s.process_op(&TerminalOp::Print("Alternate content".to_string()));
        // Leave alternate
        s.process_op(&TerminalOp::DecPrivateModeReset(1049));
        assert_eq!(s.active, ActiveBuffer::Primary);
        let snap = s.snapshot();
        assert!(!snap.is_alternate);
        assert_eq!(snap.lines[0], "Primary content"); // Primary preserved
    }

    #[test]
    fn test_alternate_screen_cursor_reset() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::CursorPosition { row: 10, col: 20 });
        s.process_op(&TerminalOp::DecPrivateModeSet(1049));
        assert_eq!(s.cursor.row, 0);
        assert_eq!(s.cursor.col, 0);
    }

    // ─── Wide character tests ───────────────────────────────────

    #[test]
    fn test_wide_char_basic() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::Print("你".to_string()));
        let snap = s.snapshot();
        assert_eq!(snap.lines[0], "你");
        // The cell at col 1 should be a wide continuation
        assert!(s.active_buf().cell(0, 1).wide);
    }

    #[test]
    fn test_wide_char_wrap() {
        let mut s = TerminalScreen::new(5, 2);
        // "AB你D" — A(1) B(1) 你(2) = 4 cols, then D at col 4
        s.process_op(&TerminalOp::Print("AB你D".to_string()));
        let snap = s.snapshot();
        assert_eq!(snap.lines[0], "AB你D");
    }

    #[test]
    fn test_wide_char_wrap_at_boundary() {
        let mut s = TerminalScreen::new(4, 2);
        // "AB你" — A(1) B(1) 你(2) = 4 cols, fills exactly
        s.process_op(&TerminalOp::Print("AB你".to_string()));
        let snap = s.snapshot();
        assert_eq!(snap.lines[0], "AB你");
    }

    #[test]
    fn test_wide_char_wrap_overflow() {
        let mut s = TerminalScreen::new(3, 2);
        // "A你" — A(1) + 你(2) = 3, fills exactly
        s.process_op(&TerminalOp::Print("A你".to_string()));
        let snap = s.snapshot();
        assert_eq!(snap.lines[0], "A你");
        // Now "你" at col 1 won't fit (need 2 cols, only 2 remaining) — but it does fit
        // Test wrap: "AB你" — A(1) B(1) = 2, 你(2) won't fit (only 1 col left) — wraps
        let mut s2 = TerminalScreen::new(3, 2);
        s2.process_op(&TerminalOp::Print("AB你".to_string()));
        let snap2 = s2.snapshot();
        assert_eq!(snap2.lines[0], "AB");
        assert_eq!(snap2.lines[1], "你");
    }

    // ─── Scroll tests ──────────────────────────────────────────

    #[test]
    fn test_scroll_up() {
        let mut s = TerminalScreen::new(10, 3);
        s.process_op(&TerminalOp::Print("AAA".to_string()));
        s.process_op(&TerminalOp::CarriageReturn);
        s.process_op(&TerminalOp::Linefeed);
        s.process_op(&TerminalOp::Print("BBB".to_string()));
        s.process_op(&TerminalOp::CarriageReturn);
        s.process_op(&TerminalOp::Linefeed);
        s.process_op(&TerminalOp::Print("CCC".to_string()));
        // At bottom, explicit scroll
        s.process_op(&TerminalOp::ScrollUp(1));
        let snap = s.snapshot();
        assert_eq!(snap.lines[0], "BBB");
        assert_eq!(snap.lines[1], "CCC");
        assert_eq!(snap.lines[2], "");
    }

    #[test]
    fn test_scroll_down() {
        let mut s = TerminalScreen::new(10, 3);
        s.process_op(&TerminalOp::Print("AAA".to_string()));
        s.process_op(&TerminalOp::CarriageReturn);
        s.process_op(&TerminalOp::Linefeed);
        s.process_op(&TerminalOp::Print("BBB".to_string()));
        s.process_op(&TerminalOp::ScrollDown(1));
        let snap = s.snapshot();
        assert_eq!(snap.lines[0], "");
        assert_eq!(snap.lines[1], "AAA");
        assert_eq!(snap.lines[2], "BBB");
    }

    #[test]
    fn test_scroll_region() {
        let mut s = TerminalScreen::new(10, 5);
        // Fill all rows
        for i in 0..5 {
            s.process_op(&TerminalOp::Print(format!("Row{}", i)));
            if i < 4 {
                s.process_op(&TerminalOp::CarriageReturn);
                s.process_op(&TerminalOp::Linefeed);
            }
        }
        // Set scroll region to rows 1–3 (0-indexed)
        s.process_op(&TerminalOp::SetScrollingRegion { top: 2, bottom: 4 });
        // Move cursor to bottom of region and trigger scroll
        s.process_op(&TerminalOp::CursorPosition { row: 4, col: 1 });
        s.process_op(&TerminalOp::CarriageReturn);
        s.process_op(&TerminalOp::Linefeed);
        s.process_op(&TerminalOp::Print("New".to_string()));
        let snap = s.snapshot();
        // Row 0 should be unchanged
        assert_eq!(snap.lines[0], "Row0");
        // Rows 1-3 were scrolled
        assert_eq!(snap.lines[1], "Row2");
        assert_eq!(snap.lines[2], "Row3");
        assert_eq!(snap.lines[3], "New");
        // Row 4 unchanged
        assert_eq!(snap.lines[4], "Row4");
    }

    // ─── Resize test ───────────────────────────────────────────

    #[test]
    fn test_resize_preserves_content() {
        let mut s = TerminalScreen::new(10, 5);
        s.process_op(&TerminalOp::Print("Hello".to_string()));
        s.resize(20, 10);
        let snap = s.snapshot();
        assert_eq!(snap.cols, 20);
        assert_eq!(snap.rows, 10);
        assert_eq!(snap.lines[0], "Hello");
    }

    #[test]
    fn test_resize_shrink_clamps_cursor() {
        let mut s = TerminalScreen::new(80, 24);
        s.process_op(&TerminalOp::CursorPosition { row: 20, col: 60 });
        s.resize(40, 10);
        assert_eq!(s.cursor.row, 9);
        assert_eq!(s.cursor.col, 39);
    }

    // ─── Snapshot test ─────────────────────────────────────────

    #[test]
    fn test_snapshot_metadata() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::CursorPosition { row: 5, col: 10 });
        s.process_op(&TerminalOp::SetWindowTitle("Test".to_string()));
        let snap = s.snapshot();
        assert_eq!(snap.cursor_row, 4);
        assert_eq!(snap.cursor_col, 9);
        assert_eq!(snap.cols, 80);
        assert_eq!(snap.rows, 24);
        assert!(!snap.is_alternate);
        assert_eq!(snap.window_title, "Test");
    }

    // ─── Backspace and Tab tests ───────────────────────────────

    #[test]
    fn test_backspace() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::Print("ABC".to_string()));
        s.process_op(&TerminalOp::Backspace);
        assert_eq!(s.cursor.col, 2);
    }

    #[test]
    fn test_tab() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::Tab);
        assert_eq!(s.cursor.col, 8);
        s.process_op(&TerminalOp::Tab);
        assert_eq!(s.cursor.col, 16);
    }

    // ─── Scrollback test ───────────────────────────────────────

    #[test]
    fn test_scrollback_primary() {
        let mut s = TerminalScreen::new(10, 2);
        s.process_op(&TerminalOp::Print("AAA".to_string()));
        s.process_op(&TerminalOp::CarriageReturn);
        s.process_op(&TerminalOp::Linefeed);
        s.process_op(&TerminalOp::Print("BBB".to_string()));
        // At bottom row; next linefeed scrolls
        s.process_op(&TerminalOp::CarriageReturn);
        s.process_op(&TerminalOp::Linefeed);
        assert_eq!(s.scrollback.len(), 1);
        let first: String = s.scrollback[0].iter().map(|c| c.char).collect();
        assert_eq!(first, "AAA");
        s.process_op(&TerminalOp::Print("CCC".to_string()));
        s.process_op(&TerminalOp::CarriageReturn);
        s.process_op(&TerminalOp::Linefeed);
        assert_eq!(s.scrollback.len(), 2);
    }

    // ─── Window title test ─────────────────────────────────────

    #[test]
    fn test_window_title() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::SetWindowTitle("My App".to_string()));
        assert_eq!(s.window_title, "My App");
        let snap = s.snapshot();
        assert_eq!(snap.window_title, "My App");
    }

    // ─── SGR attribute parsing edge cases ──────────────────────

    #[test]
    fn test_sgr_high_intensity_colors() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::SetGraphicRendition(vec![91]));
        s.process_op(&TerminalOp::Print("X".to_string()));
        let cell = s.active_buf().cell(0, 0);
        assert_eq!(cell.attrs.fg, Color::Index(9));
    }

    #[test]
    fn test_sgr_italic_underline_reverse() {
        let mut attrs = CellAttributes::default();
        attrs.apply_sgr(&[3, 4, 7]);
        assert!(attrs.italic);
        assert!(attrs.underline);
        assert!(attrs.reverse);
        attrs.apply_sgr(&[23, 24, 27]);
        assert!(!attrs.italic);
        assert!(!attrs.underline);
        assert!(!attrs.reverse);
    }

    // ─── Char width test ───────────────────────────────────────

    #[test]
    fn test_char_width_ascii() {
        assert_eq!(char_width('A'), 1);
        assert_eq!(char_width('\n'), 0);
    }

    #[test]
    fn test_char_width_cjk() {
        assert_eq!(char_width('你'), 2);
        assert_eq!(char_width('世'), 2);
        assert_eq!(char_width('界'), 2);
    }

    #[test]
    fn test_char_width_emoji() {
        assert_eq!(char_width('\u{1F600}'), 2); // 😀
    }

    // ─── Scrollback accessor tests ─────────────────────────────────

    #[test]
    fn test_scrollback_len_empty() {
        let s = new_screen();
        assert_eq!(s.scrollback_len(), 0);
    }

    #[test]
    fn test_scrollback_lines_empty() {
        let s = new_screen();
        assert!(s.scrollback_lines(10).is_empty());
    }

    #[test]
    fn test_scrollback_lines_basic() {
        let mut s = TerminalScreen::new(10, 2);
        s.process_op(&TerminalOp::Print("AAA".to_string()));
        s.process_op(&TerminalOp::CarriageReturn);
        s.process_op(&TerminalOp::Linefeed);
        s.process_op(&TerminalOp::Print("BBB".to_string()));
        // Trigger scroll to push "AAA" into scrollback
        s.process_op(&TerminalOp::CarriageReturn);
        s.process_op(&TerminalOp::Linefeed);
        assert_eq!(s.scrollback_len(), 1);
        let lines = s.scrollback_lines(10);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "AAA");
    }

    #[test]
    fn test_scrollback_lines_max() {
        let mut s = TerminalScreen::new(10, 2);
        // Fill many lines to generate scrollback
        for i in 0..10 {
            s.process_op(&TerminalOp::Print(format!("L{}", i)));
            s.process_op(&TerminalOp::CarriageReturn);
            s.process_op(&TerminalOp::Linefeed);
        }
        assert!(s.scrollback_len() > 1);
        // Only request last 3
        let lines = s.scrollback_lines(3);
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn test_snapshot_serializable() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::Print("Hello".to_string()));
        s.process_op(&TerminalOp::CursorPosition { row: 5, col: 10 });
        let snap = s.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("Hello"));
        assert!(json.contains("cursor_row"));
        assert!(json.contains("is_alternate"));
    }

    #[test]
    fn test_state_hash_stable_when_unchanged() {
        let s = new_screen();
        let h1 = s.state_hash();
        let h2 = s.state_hash();
        assert_eq!(h1, h2, "Hash should be stable for identical screen state");
    }

    #[test]
    fn test_state_hash_changes_on_mutation() {
        let mut s = new_screen();
        let h1 = s.state_hash();
        s.process_op(&TerminalOp::Print("X".to_string()));
        let h2 = s.state_hash();
        assert_ne!(h1, h2, "Hash should change when screen content changes");
    }

    #[test]
    fn test_state_hash_changes_on_cursor_move() {
        let mut s = new_screen();
        let h1 = s.state_hash();
        s.process_op(&TerminalOp::CursorPosition { row: 5, col: 5 });
        let h2 = s.state_hash();
        assert_ne!(h1, h2, "Hash should change when cursor moves");
    }

    #[test]
    fn test_state_hash_changes_on_alternate_screen() {
        let mut s = new_screen();
        let h1 = s.state_hash();
        s.process_op(&TerminalOp::DecPrivateModeSet(1049));
        let h2 = s.state_hash();
        assert_ne!(h1, h2, "Hash should change when entering alternate screen");
    }

    #[test]
    fn test_state_hash_changes_on_cursor_visibility() {
        let s = new_screen();
        let h1 = s.state_hash();
        let mut s = s;
        s.process_op(&TerminalOp::DecPrivateModeReset(25)); // ?25l = hide cursor
        let h2 = s.state_hash();
        assert_ne!(h1, h2, "Hash should change when cursor visibility changes");
    }

    // ─── CNL / CPL screen tests ────────────────────────────────────

    #[test]
    fn test_cnl_moves_down_and_resets_col() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::CursorPosition { row: 2, col: 10 });
        assert_eq!(s.cursor.row, 1);
        assert_eq!(s.cursor.col, 9);
        s.process_op(&TerminalOp::CursorNextLine(3));
        assert_eq!(s.cursor.row, 4);
        assert_eq!(s.cursor.col, 0);
    }

    #[test]
    fn test_cnl_clamps_to_last_row() {
        let mut s = TerminalScreen::new(10, 5);
        s.process_op(&TerminalOp::CursorPosition { row: 4, col: 5 });
        s.process_op(&TerminalOp::CursorNextLine(100));
        assert_eq!(s.cursor.row, 4, "Should clamp to last row (0-indexed)");
        assert_eq!(s.cursor.col, 0);
    }

    #[test]
    fn test_cpl_moves_up_and_resets_col() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::CursorPosition { row: 10, col: 8 });
        s.process_op(&TerminalOp::CursorPreviousLine(3));
        assert_eq!(s.cursor.row, 6);
        assert_eq!(s.cursor.col, 0);
    }

    #[test]
    fn test_cpl_clamps_to_zero() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::CursorPosition { row: 2, col: 5 });
        s.process_op(&TerminalOp::CursorPreviousLine(100));
        assert_eq!(s.cursor.row, 0);
        assert_eq!(s.cursor.col, 0);
    }

    #[test]
    fn test_dectcem_hide_show() {
        let mut s = new_screen();
        assert!(s.cursor_visible);
        // ?25l (DECTCEM reset) = hide cursor
        s.process_op(&TerminalOp::DecPrivateModeReset(25));
        assert!(!s.cursor_visible, "Cursor should be hidden after ?25l");
        // ?25h (DECTCEM set) = show cursor
        s.process_op(&TerminalOp::DecPrivateModeSet(25));
        assert!(s.cursor_visible, "Cursor should be visible after ?25h");
    }

    // ─── Screen diff tests ─────────────────────────────────────────

    #[test]
    fn test_diff_identical_snapshots() {
        let s = new_screen();
        let snap = s.snapshot();
        let d = snap.diff(&snap);
        assert!(d.is_empty());
        assert!(d.changed_lines.is_empty());
        assert!(!d.cursor_changed);
        assert!(d.cursor_delta.is_none());
        assert!(!d.mode_changed);
        assert!(d.title_changed.is_none());
        assert!(!d.dims_changed);
    }

    #[test]
    fn test_diff_detects_line_change() {
        let mut s = new_screen();
        let snap1 = s.snapshot();
        s.process_op(&TerminalOp::Print("Hello".to_string()));
        let snap2 = s.snapshot();

        let d = snap1.diff(&snap2);
        assert!(!d.is_empty());
        assert_eq!(d.changed_lines.len(), 1);
        assert_eq!(d.changed_lines[0].row, 0);
        assert_eq!(d.changed_lines[0].old, "");
        assert_eq!(d.changed_lines[0].new, "Hello");
    }

    #[test]
    fn test_diff_detects_multiple_line_changes() {
        let mut s = TerminalScreen::new(10, 3);
        s.process_op(&TerminalOp::Print("AAA".to_string()));
        s.process_op(&TerminalOp::Linefeed);
        s.process_op(&TerminalOp::Print("BBB".to_string()));
        s.process_op(&TerminalOp::Linefeed);
        s.process_op(&TerminalOp::Print("CCC".to_string()));
        let snap1 = s.snapshot();

        // Overwrite line 1
        s.process_op(&TerminalOp::CursorPosition { row: 2, col: 1 });
        s.process_op(&TerminalOp::Print("XXX".to_string()));
        let snap2 = s.snapshot();

        let d = snap1.diff(&snap2);
        assert_eq!(d.changed_lines.len(), 1);
        assert_eq!(d.changed_lines[0].row, 1);
        assert_eq!(d.changed_lines[0].old, "BBB");
        assert_eq!(d.changed_lines[0].new, "XXX");
    }

    #[test]
    fn test_diff_detects_cursor_change() {
        let mut s = new_screen();
        let snap1 = s.snapshot();
        s.process_op(&TerminalOp::CursorPosition { row: 10, col: 5 });
        let snap2 = s.snapshot();

        let d = snap1.diff(&snap2);
        assert!(d.cursor_changed);
        assert_eq!(d.cursor_delta, Some((0, 0, 9, 4)));
        // No line changes
        assert!(d.changed_lines.is_empty());
    }

    #[test]
    fn test_diff_detects_mode_change() {
        let mut s = new_screen();
        s.process_op(&TerminalOp::Print("Primary".to_string()));
        let snap1 = s.snapshot();

        s.process_op(&TerminalOp::DecPrivateModeSet(1049));
        let snap2 = s.snapshot();

        let d = snap1.diff(&snap2);
        assert!(d.mode_changed);
        // Alternate screen is cleared, so lines differ
        assert!(!d.changed_lines.is_empty());
    }

    #[test]
    fn test_diff_detects_title_change() {
        let mut s = new_screen();
        let snap1 = s.snapshot();
        s.process_op(&TerminalOp::SetWindowTitle("New Title".to_string()));
        let snap2 = s.snapshot();

        let d = snap1.diff(&snap2);
        assert_eq!(d.title_changed.as_deref(), Some("New Title"));
        assert!(d.changed_lines.is_empty());
    }

    #[test]
    fn test_diff_detects_dimension_change() {
        let mut s = TerminalScreen::new(80, 24);
        let snap1 = s.snapshot();
        s.resize(120, 40);
        let snap2 = s.snapshot();

        let d = snap1.diff(&snap2);
        assert!(d.dims_changed);
    }

    #[test]
    fn test_diff_serializable() {
        let mut s = new_screen();
        let snap1 = s.snapshot();
        s.process_op(&TerminalOp::Print("Hello".to_string()));
        s.process_op(&TerminalOp::SetWindowTitle("Test".to_string()));
        let snap2 = s.snapshot();

        let d = snap1.diff(&snap2);
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains("changed_lines"));
        assert!(json.contains("cursor_changed"));
        assert!(json.contains("title_changed"));
    }

    // ─── Regression fixture: partial UTF-8 across reads ─────────

    #[test]
    fn test_fixture_partial_utf8_across_feeds() {
        // Use TerminalEmulatorHandle to preserve vte parser state across feeds
        use crate::builtins::terminal::emulator::TerminalEmulatorHandle;
        let mut emu = TerminalEmulatorHandle::new(20, 5);

        // First feed: only 2 bytes of 你 — vte parser buffers internally
        emu.feed_bytes(&[0xE4, 0xBD]);
        // No complete chars yet — screen should be empty
        let snap = emu.screen().snapshot();
        assert_eq!(snap.lines[0], "");

        // Second feed: the last byte completes 你
        emu.feed_bytes(&[0xA0]);
        emu.flush_parser();
        let snap = emu.screen().snapshot();
        assert_eq!(snap.lines[0], "你");
    }

    // ─── Regression fixture: partial ANSI sequence split across reads ──

    #[test]
    fn test_fixture_partial_ansi_across_feeds() {
        // Use TerminalEmulatorHandle to test cross-read state persistence
        use crate::builtins::terminal::emulator::TerminalEmulatorHandle;
        let mut emu = TerminalEmulatorHandle::new(20, 5);

        // Feed "Hello" then start a CSI sequence "\x1b[" but don't finish
        emu.feed_bytes(b"Hello\x1b[");
        // "Hello" is deferred (pure text), no ops yet
        let snap = emu.screen().snapshot();
        assert_eq!(snap.lines[0], "Hello");

        // Now finish the cursor-position sequence
        emu.feed_bytes(b"5;10HWorld");
        let snap = emu.screen().snapshot();
        assert_eq!(snap.lines[0], "Hello");
        // "World" printed at cursor position (4, 9) — row 4
        assert!(snap.lines[4].contains("World"));
    }

    // ─── Regression fixture: alternate screen enter/exit restore ─────

    #[test]
    fn test_fixture_alternate_screen_restore() {
        let mut screen = TerminalScreen::new(20, 5);
        screen.process_op(&TerminalOp::Print("Line1".to_string()));
        screen.process_op(&TerminalOp::CarriageReturn);
        screen.process_op(&TerminalOp::Linefeed);
        screen.process_op(&TerminalOp::Print("Line2".to_string()));

        let primary_snap = screen.snapshot();
        assert_eq!(primary_snap.lines[0], "Line1");
        assert_eq!(primary_snap.lines[1], "Line2");

        // Enter alternate screen
        screen.process_op(&TerminalOp::DecPrivateModeSet(1049));
        let alt_snap = screen.snapshot();
        assert!(alt_snap.is_alternate);
        assert_eq!(alt_snap.lines[0], ""); // Cleared on enter

        // Write on alternate screen
        screen.process_op(&TerminalOp::Print("AltContent".to_string()));
        let alt_content = screen.snapshot();
        assert_eq!(alt_content.lines[0], "AltContent");

        // Leave alternate screen — primary must be restored
        screen.process_op(&TerminalOp::DecPrivateModeReset(1049));
        let restored = screen.snapshot();
        assert!(!restored.is_alternate);
        assert_eq!(restored.lines[0], "Line1");
        assert_eq!(restored.lines[1], "Line2");
    }

    // ─── Regression fixture: full-screen redraw ─────────────────────

    #[test]
    fn test_fixture_full_screen_redraw() {
        let mut screen = TerminalScreen::new(10, 3);

        // Draw initial content
        screen.process_op(&TerminalOp::Print("AAA".to_string()));
        screen.process_op(&TerminalOp::CarriageReturn);
        screen.process_op(&TerminalOp::Linefeed);
        screen.process_op(&TerminalOp::Print("BBB".to_string()));
        screen.process_op(&TerminalOp::CarriageReturn);
        screen.process_op(&TerminalOp::Linefeed);
        screen.process_op(&TerminalOp::Print("CCC".to_string()));

        // Full erase + redraw
        screen.process_op(&TerminalOp::EraseInDisplay(EraseMode::All));
        screen.process_op(&TerminalOp::CursorPosition { row: 1, col: 1 });
        screen.process_op(&TerminalOp::Print("XXX".to_string()));
        screen.process_op(&TerminalOp::CarriageReturn);
        screen.process_op(&TerminalOp::Linefeed);
        screen.process_op(&TerminalOp::Print("YYY".to_string()));

        let snap = screen.snapshot();
        assert_eq!(snap.lines[0], "XXX");
        assert_eq!(snap.lines[1], "YYY");
        assert_eq!(snap.lines[2], "");
    }

    // ─── Regression fixture: wide char / CJK alignment ──────────────

    #[test]
    fn test_fixture_wide_char_alignment() {
        let mut screen = TerminalScreen::new(10, 3);

        // Print CJK text mixed with ASCII
        screen.process_op(&TerminalOp::Print("AB你好CD".to_string()));
        let snap = screen.snapshot();
        // A(1) B(1) 你(2) 好(2) C(1) D(1) = 8 cols
        assert_eq!(snap.lines[0], "AB你好CD");

        // Verify wide continuation cells
        // 你 at col 2-3, 好 at col 4-5
        let buf = screen.active_buf();
        assert!(
            buf.cell(0, 3).wide,
            "col 3 should be wide continuation for 你"
        );
        assert!(
            buf.cell(0, 5).wide,
            "col 5 should be wide continuation for 好"
        );
    }

    // ─── Regression fixture: menu navigation via cursor movement ────

    #[test]
    fn test_fixture_menu_navigation() {
        let mut screen = TerminalScreen::new(20, 10);

        // Simulate a simple menu with highlighted item
        screen.process_op(&TerminalOp::Print("Option 1".to_string()));
        screen.process_op(&TerminalOp::CarriageReturn);
        screen.process_op(&TerminalOp::Linefeed);
        screen.process_op(&TerminalOp::Print("> Option 2".to_string()));
        screen.process_op(&TerminalOp::CarriageReturn);
        screen.process_op(&TerminalOp::Linefeed);
        screen.process_op(&TerminalOp::Print("Option 3".to_string()));

        let snap1 = screen.snapshot();
        assert!(snap1.lines[1].contains("> Option 2"));

        // Simulate arrow-down: redraw menu with highlight moved
        screen.process_op(&TerminalOp::CursorPosition { row: 1, col: 1 });
        screen.process_op(&TerminalOp::EraseInDisplay(EraseMode::All));

        screen.process_op(&TerminalOp::Print("Option 1".to_string()));
        screen.process_op(&TerminalOp::CarriageReturn);
        screen.process_op(&TerminalOp::Linefeed);
        screen.process_op(&TerminalOp::Print("Option 2".to_string()));
        screen.process_op(&TerminalOp::CarriageReturn);
        screen.process_op(&TerminalOp::Linefeed);
        screen.process_op(&TerminalOp::Print("> Option 3".to_string()));

        let snap2 = screen.snapshot();
        assert!(snap2.lines[2].contains("> Option 3"));
        assert!(!snap2.lines[1].contains(">"));

        // Diff should show lines 1 and 2 changed
        let d = snap1.diff(&snap2);
        assert!(d.changed_lines.iter().any(|c| c.row == 1));
        assert!(d.changed_lines.iter().any(|c| c.row == 2));
    }

    // ─── Deferred wrap tests ────────────────────────────────────

    #[test]
    fn test_deferred_wrap_pending() {
        let mut s = TerminalScreen::new(5, 2);
        // Print exactly 5 chars — fills the line
        s.process_op(&TerminalOp::Print("ABCDE".to_string()));
        // cursor should be at col 4 (clamped) with wrap_pending = true
        assert_eq!(s.cursor.col, 4);
        assert!(s.cursor.wrap_pending);
        // Print one more — should wrap to next line first
        s.process_op(&TerminalOp::Print("F".to_string()));
        assert_eq!(s.cursor.row, 1);
        assert!(!s.cursor.wrap_pending);
        let snap = s.snapshot();
        assert_eq!(snap.lines[0], "ABCDE");
        assert_eq!(snap.lines[1], "F");
    }

    #[test]
    fn test_deferred_wrap_cleared_by_cursor_move() {
        let mut s = TerminalScreen::new(5, 2);
        s.process_op(&TerminalOp::Print("ABCDE".to_string()));
        assert!(s.cursor.wrap_pending);
        s.process_op(&TerminalOp::CursorBack(1));
        assert!(!s.cursor.wrap_pending);
        assert_eq!(s.cursor.col, 3);
        // Print should go at col 3, not wrap
        s.process_op(&TerminalOp::Print("X".to_string()));
        let snap = s.snapshot();
        assert_eq!(snap.lines[0], "ABCXE");
    }

    // ─── DECAWM tests ───────────────────────────────────────────

    #[test]
    fn test_autowrap_on_by_default() {
        let s = TerminalScreen::new(5, 2);
        assert!(s.autowrap);
    }

    #[test]
    fn test_autowrap_off_no_wrap() {
        let mut s = TerminalScreen::new(5, 2);
        s.process_op(&TerminalOp::DecPrivateModeReset(7)); // DECAWM off
        assert!(!s.autowrap);
        // Print 6 chars into a 5-col terminal — last char overwrites col 4
        s.process_op(&TerminalOp::Print("ABCDEF".to_string()));
        let snap = s.snapshot();
        assert_eq!(snap.lines[0], "ABCDF"); // E overwritten by F
        assert!(!s.cursor.wrap_pending);
    }

    // ─── DECOM tests ────────────────────────────────────────────

    #[test]
    fn test_origin_mode_cup_relative() {
        let mut s = TerminalScreen::new(80, 24);
        s.process_op(&TerminalOp::SetScrollingRegion { top: 5, bottom: 20 });
        s.process_op(&TerminalOp::DecPrivateModeSet(6)); // DECOM on
                                                         // Cursor should be at scroll_top (row 4, 0-indexed)
        assert_eq!(s.cursor.row, 4);
        assert_eq!(s.cursor.col, 0);
        // CUP (1,1) with origin mode = row 4, col 0 (0-indexed)
        s.process_op(&TerminalOp::CursorPosition { row: 1, col: 1 });
        assert_eq!(s.cursor.row, 4);
        assert_eq!(s.cursor.col, 0);
        // CUP (3,5) with origin mode = row 4+2=6, col 4 (0-indexed)
        s.process_op(&TerminalOp::CursorPosition { row: 3, col: 5 });
        assert_eq!(s.cursor.row, 6);
        assert_eq!(s.cursor.col, 4);
        // Disable origin mode
        s.process_op(&TerminalOp::DecPrivateModeReset(6));
        assert_eq!(s.cursor.row, 0);
        assert_eq!(s.cursor.col, 0);
        assert!(!s.origin_mode);
    }

    // ─── REP tests ──────────────────────────────────────────────

    #[test]
    fn test_repeat_character() {
        let mut s = TerminalScreen::new(20, 5);
        s.process_op(&TerminalOp::Print("-".to_string()));
        s.process_op(&TerminalOp::Repeat(9)); // 9 more dashes = 10 total
        let snap = s.snapshot();
        assert_eq!(snap.lines[0], "----------");
    }

    #[test]
    fn test_repeat_with_wrap() {
        let mut s = TerminalScreen::new(5, 3);
        s.process_op(&TerminalOp::Print("X".to_string()));
        s.process_op(&TerminalOp::Repeat(7)); // 1 + 7 = 8 X's across a 5-col terminal
        let snap = s.snapshot();
        assert_eq!(snap.lines[0], "XXXXX");
        assert_eq!(snap.lines[1], "XXX");
    }
}
