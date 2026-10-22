//! A headless copy of each terminal's screen, fed the same output the
//! clients get, so a client joining late receives the exact screen as
//! escape sequences (`terminal.attached` with `snapshot: true`) instead of
//! a raw output tail that may start mid-sequence or after a full-screen
//! program switched screens.
//!
//! The emulator is `alacritty_terminal`; this module serializes its state:
//! primary screen with scrollback, the alternate screen when in use, cursor
//! position and shape, the current style, scroll region, modes, keyboard
//! protocol flags and title. A mirror never answers the program's queries.

use std::{
    fmt::Write as _,
    sync::{Arc, Mutex},
};

use alacritty_terminal::{
    event::{Event, EventListener},
    grid::{Dimensions, Grid},
    index::{Column, Line},
    term::{
        cell::{Cell, Flags, Hyperlink},
        Config, TermMode,
    },
    vte::{
        ansi::{Color, CursorShape, NamedColor, Processor},
        Params, Parser, Perform,
    },
    Term,
};

/// Scrollback kept per terminal and sent in a snapshot.
pub const MIRROR_SCROLLBACK_LINES: usize = 2000;

/// `TODEX_AGENTD_TERMINAL_SNAPSHOT=0` turns mirrors off: attach then replays
/// the raw output tail as before.
pub fn snapshots_enabled() -> bool {
    std::env::var("TODEX_AGENTD_TERMINAL_SNAPSHOT")
        .map(|value| value.trim() != "0")
        .unwrap_or(true)
}

#[derive(Clone, Default)]
struct TitleListener {
    title: Arc<Mutex<Option<String>>>,
}

impl EventListener for TitleListener {
    fn send_event(&self, event: Event) {
        // Everything else (replies to queries, clipboard, bell) is the
        // clients' business: a mirror must stay silent.
        let title = match event {
            Event::Title(title) => Some(title),
            Event::ResetTitle => None,
            _ => return,
        };
        if let Ok(mut current) = self.title.lock() {
            *current = title;
        }
    }
}

struct MirrorSize {
    rows: usize,
    cols: usize,
}

impl Dimensions for MirrorSize {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// What alacritty keeps private but a snapshot needs: the scroll region.
#[derive(Default)]
struct RegionTracker {
    rows: usize,
    /// One-based top and bottom, `None` for the full screen.
    region: Option<(usize, usize)>,
}

impl Perform for RegionTracker {
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        match (action, intermediates) {
            ('r', []) => {
                let mut values = params
                    .iter()
                    .map(|param| param.first().copied().unwrap_or(0));
                let top = values.next().filter(|value| *value > 0).unwrap_or(1) as usize;
                let bottom = values
                    .next()
                    .filter(|value| *value > 0)
                    .map_or(self.rows, |value| value as usize)
                    .min(self.rows);
                self.region =
                    (top < bottom && (top, bottom) != (1, self.rows)).then_some((top, bottom));
            }
            // DECSTR soft reset.
            ('p', [b'!']) => self.region = None,
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        // RIS full reset.
        if intermediates.is_empty() && byte == b'c' {
            self.region = None;
        }
    }
}

pub struct TerminalMirror {
    term: Term<TitleListener>,
    processor: Processor,
    title: Arc<Mutex<Option<String>>>,
    regions: Parser,
    region: RegionTracker,
}

impl TerminalMirror {
    pub fn new(rows: u16, cols: u16) -> Self {
        let listener = TitleListener::default();
        let title = listener.title.clone();
        let config = Config {
            scrolling_history: MIRROR_SCROLLBACK_LINES,
            // Track the keyboard protocol flags programs push.
            kitty_keyboard: true,
            ..Config::default()
        };
        let size = MirrorSize {
            rows: rows as usize,
            cols: cols as usize,
        };
        Self {
            term: Term::new(config, &size, listener),
            processor: Processor::new(),
            title,
            regions: Parser::new(),
            region: RegionTracker {
                rows: rows as usize,
                region: None,
            },
        }
    }

    pub fn advance(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
        self.regions.advance(&mut self.region, bytes);
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.term.resize(MirrorSize {
            rows: rows as usize,
            cols: cols as usize,
        });
        self.region = RegionTracker {
            rows: rows as usize,
            region: None,
        };
    }

    /// What the clients' Clear does: the cursor's line becomes the first
    /// line, everything else and the scrollback go.
    pub fn clear(&mut self) {
        let point = self.term.grid().cursor.point;
        let mut sequence = String::new();
        if point.line.0 > 0 {
            let _ = write!(sequence, "\x1b[{}S", point.line.0);
        }
        let _ = write!(
            sequence,
            "\x1b[2;1H\x1b[J\x1b[1;{}H\x1b[3J",
            point.column.0 + 1
        );
        self.advance(sequence.as_bytes());
    }

    /// The current state as escape sequences for a freshly reset terminal
    /// of the same size.
    pub fn snapshot(&mut self) -> String {
        let mut out = String::new();
        let alt = self.term.mode().contains(TermMode::ALT_SCREEN);
        if alt {
            // The primary screen is only reachable as the active grid. Going
            // back to the alternate screen resets it, so keep a copy.
            let alt_grid = self.term.grid().clone();
            self.term.swap_alt();
            write_grid(&mut out, self.term.grid(), true);
            write_cursor_position(&mut out, self.term.grid());
            self.term.swap_alt();
            *self.term.grid_mut() = alt_grid;
            out.push_str("\x1b[?1049h\x1b[H");
            write_grid(&mut out, self.term.grid(), false);
        } else {
            write_grid(&mut out, self.term.grid(), true);
        }
        self.write_state(&mut out);
        out
    }

    fn write_state(&self, out: &mut String) {
        let mode = *self.term.mode();
        let grid = self.term.grid();
        if let Some((top, bottom)) = self.region.region {
            let _ = write!(out, "\x1b[{top};{bottom}r");
        }
        if mode.contains(TermMode::ORIGIN) {
            out.push_str("\x1b[?6h");
        }
        write_cursor_position(out, grid);
        out.push_str("\x1b[0m");
        let pen = sgr(&grid.cursor.template);
        if !pen.is_empty() {
            let _ = write!(out, "\x1b[{pen}m");
        }
        let modes: &[(TermMode, &str)] = &[
            (TermMode::APP_CURSOR, "\x1b[?1h"),
            (TermMode::APP_KEYPAD, "\x1b="),
            (TermMode::INSERT, "\x1b[4h"),
            (TermMode::LINE_FEED_NEW_LINE, "\x1b[20h"),
            (TermMode::MOUSE_REPORT_CLICK, "\x1b[?1000h"),
            (TermMode::MOUSE_DRAG, "\x1b[?1002h"),
            (TermMode::MOUSE_MOTION, "\x1b[?1003h"),
            (TermMode::FOCUS_IN_OUT, "\x1b[?1004h"),
            (TermMode::UTF8_MOUSE, "\x1b[?1005h"),
            (TermMode::SGR_MOUSE, "\x1b[?1006h"),
            (TermMode::BRACKETED_PASTE, "\x1b[?2004h"),
        ];
        for (flag, sequence) in modes {
            if mode.contains(*flag) {
                out.push_str(sequence);
            }
        }
        if !mode.contains(TermMode::LINE_WRAP) {
            out.push_str("\x1b[?7l");
        }
        if !mode.contains(TermMode::SHOW_CURSOR) {
            out.push_str("\x1b[?25l");
        }
        let style = self.term.cursor_style();
        if style != Config::default().default_cursor_style {
            let code = match (style.shape, style.blinking) {
                (CursorShape::Underline, true) => 3,
                (CursorShape::Underline, false) => 4,
                (CursorShape::Beam, true) => 5,
                (CursorShape::Beam, false) => 6,
                (_, true) => 1,
                (_, false) => 2,
            };
            let _ = write!(out, "\x1b[{code} q");
        }
        let keyboard = [
            TermMode::DISAMBIGUATE_ESC_CODES,
            TermMode::REPORT_EVENT_TYPES,
            TermMode::REPORT_ALTERNATE_KEYS,
            TermMode::REPORT_ALL_KEYS_AS_ESC,
            TermMode::REPORT_ASSOCIATED_TEXT,
        ]
        .iter()
        .enumerate()
        .filter(|(_, flag)| mode.contains(**flag))
        .fold(0_u32, |flags, (bit, _)| flags | (1 << bit));
        if keyboard != 0 {
            let _ = write!(out, "\x1b[>{keyboard}u");
        }
        if let Some(title) = self.title.lock().ok().and_then(|title| title.clone()) {
            let title: String = title.chars().filter(|c| !c.is_control()).collect();
            let _ = write!(out, "\x1b]2;{title}\x07");
        }
    }
}

fn write_cursor_position(out: &mut String, grid: &Grid<Cell>) {
    let point = grid.cursor.point;
    let _ = write!(out, "\x1b[{};{}H", point.line.0 + 1, point.column.0 + 1);
}

/// Writes a grid's rows (with its scrollback when `history`) from the top.
fn write_grid(out: &mut String, grid: &Grid<Cell>, history: bool) {
    let screen = grid.screen_lines() as i32;
    let top = if history {
        -(grid.history_size() as i32)
    } else {
        0
    };
    let columns = grid.columns();
    let mut style = String::new();
    let mut link: Option<Hyperlink> = None;
    for line in top..screen {
        let row = &grid[Line(line)];
        let wrapped = row[Column(columns - 1)].flags.contains(Flags::WRAPLINE);
        // Blank cells at the end of a line are left out (a wrapped line is
        // written in full so the next one continues it).
        let end = if wrapped {
            columns
        } else {
            (0..columns)
                .rev()
                .find(|&column| !is_blank(&row[Column(column)]))
                .map_or(0, |column| column + 1)
        };
        for column in 0..end {
            let cell = &row[Column(column)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let cell_link = cell.hyperlink();
            if cell_link != link {
                match &cell_link {
                    Some(next) => {
                        let _ = write!(out, "\x1b]8;id={};{}\x1b\\", next.id(), next.uri());
                    }
                    None => out.push_str("\x1b]8;;\x1b\\"),
                }
                link = cell_link;
            }
            let cell_style = sgr(cell);
            if cell_style != style {
                out.push_str("\x1b[0");
                if !cell_style.is_empty() {
                    out.push(';');
                    out.push_str(&cell_style);
                }
                out.push('m');
                style = cell_style;
            }
            out.push(if cell.c == '\0' { ' ' } else { cell.c });
            if let Some(zerowidth) = cell.zerowidth() {
                out.extend(zerowidth.iter());
            }
        }
        if link.take().is_some() {
            out.push_str("\x1b]8;;\x1b\\");
        }
        if !style.is_empty() {
            out.push_str("\x1b[0m");
            style.clear();
        }
        if line + 1 < screen && !wrapped {
            out.push_str("\r\n");
        }
    }
}

fn is_blank(cell: &Cell) -> bool {
    (cell.c == ' ' || cell.c == '\0')
        && cell.bg == Color::Named(NamedColor::Background)
        && !cell
            .flags
            .intersects(Flags::INVERSE | Flags::ALL_UNDERLINES | Flags::STRIKEOUT)
        && cell.extra.is_none()
}

/// SGR parameters (without `CSI` / `m`) for a cell's style.
fn sgr(cell: &Cell) -> String {
    let mut params: Vec<String> = Vec::new();
    let flags = cell.flags;
    if flags.contains(Flags::BOLD) {
        params.push("1".into());
    }
    if flags.contains(Flags::DIM) {
        params.push("2".into());
    }
    if flags.contains(Flags::ITALIC) {
        params.push("3".into());
    }
    if flags.contains(Flags::DOUBLE_UNDERLINE) {
        params.push("4:2".into());
    } else if flags.contains(Flags::UNDERCURL) {
        params.push("4:3".into());
    } else if flags.contains(Flags::DOTTED_UNDERLINE) {
        params.push("4:4".into());
    } else if flags.contains(Flags::DASHED_UNDERLINE) {
        params.push("4:5".into());
    } else if flags.contains(Flags::UNDERLINE) {
        params.push("4".into());
    }
    if flags.contains(Flags::INVERSE) {
        params.push("7".into());
    }
    if flags.contains(Flags::HIDDEN) {
        params.push("8".into());
    }
    if flags.contains(Flags::STRIKEOUT) {
        params.push("9".into());
    }
    if let Some(color) = color_param(cell.fg, 30, 90, 38) {
        params.push(color);
    }
    if let Some(color) = color_param(cell.bg, 40, 100, 48) {
        params.push(color);
    }
    if let Some(color) = cell
        .underline_color()
        .and_then(|color| color_param(color, 0, 0, 58))
    {
        params.push(color);
    }
    params.join(";")
}

/// One SGR color parameter; `None` for the default colors.
fn color_param(color: Color, base: u16, bright: u16, extended: u16) -> Option<String> {
    match color {
        Color::Spec(rgb) => Some(format!("{extended};2;{};{};{}", rgb.r, rgb.g, rgb.b)),
        Color::Indexed(index) => Some(format!("{extended};5;{index}")),
        Color::Named(named) => {
            let index = match named {
                NamedColor::Black | NamedColor::DimBlack => 0,
                NamedColor::Red | NamedColor::DimRed => 1,
                NamedColor::Green | NamedColor::DimGreen => 2,
                NamedColor::Yellow | NamedColor::DimYellow => 3,
                NamedColor::Blue | NamedColor::DimBlue => 4,
                NamedColor::Magenta | NamedColor::DimMagenta => 5,
                NamedColor::Cyan | NamedColor::DimCyan => 6,
                NamedColor::White | NamedColor::DimWhite => 7,
                NamedColor::BrightBlack => 8,
                NamedColor::BrightRed => 9,
                NamedColor::BrightGreen => 10,
                NamedColor::BrightYellow => 11,
                NamedColor::BrightBlue => 12,
                NamedColor::BrightMagenta => 13,
                NamedColor::BrightCyan => 14,
                NamedColor::BrightWhite => 15,
                _ => return None,
            };
            // Underline colors only have the extended form.
            Some(if base == 0 {
                format!("{extended};5;{index}")
            } else if index < 8 {
                (base + index).to_string()
            } else {
                (bright + index - 8).to_string()
            })
        }
    }
}

#[cfg(test)]
impl TerminalMirror {
    fn rows_for_test(&self) -> Vec<String> {
        dump_grid(self.term.grid())
    }

    fn scroll_region_for_test(&self) -> Option<(usize, usize)> {
        self.region.region
    }
}

#[cfg(test)]
fn dump_grid(grid: &Grid<Cell>) -> Vec<String> {
    let screen = grid.screen_lines() as i32;
    let top = -(grid.history_size() as i32);
    let range: std::ops::Range<i32> = top..screen;
    range
        .map(|line| {
            let row = &grid[Line(line)];
            (0..grid.columns())
                .map(|column| {
                    let cell = &row[Column(column)];
                    format!(
                        "{}{:?}{:?}{:?}{:?}|",
                        cell.c,
                        cell.fg,
                        cell.bg,
                        cell.flags,
                        cell.hyperlink().map(|link| link.uri().to_owned())
                    )
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replays `input` into a mirror, snapshots it, replays the snapshot
    /// into a fresh mirror of the same size and checks they agree.
    fn round_trip(rows: u16, cols: u16, input: &str) -> (TerminalMirror, TerminalMirror, String) {
        let mut original = TerminalMirror::new(rows, cols);
        original.advance(input.as_bytes());
        let snapshot = original.snapshot();
        let mut copy = TerminalMirror::new(rows, cols);
        copy.advance(snapshot.as_bytes());
        assert_eq!(
            copy.rows_for_test(),
            original.rows_for_test(),
            "{snapshot:?}"
        );
        assert_eq!(
            copy.term.grid().cursor.point,
            original.term.grid().cursor.point,
            "cursor; {snapshot:?}"
        );
        let relevant = TermMode::ANY ^ TermMode::VI;
        assert_eq!(
            *copy.term.mode() & relevant,
            *original.term.mode() & relevant,
            "modes; {snapshot:?}"
        );
        assert_eq!(copy.term.cursor_style(), original.term.cursor_style());
        assert_eq!(
            copy.scroll_region_for_test(),
            original.scroll_region_for_test()
        );
        assert_eq!(*copy.title.lock().unwrap(), *original.title.lock().unwrap());
        (original, copy, snapshot)
    }

    #[test]
    fn styled_text_scrollback_and_wrapping_survive() {
        let mut input = String::new();
        for line in 0..40 {
            input.push_str(&format!(
                "\x1b[3{}mline {line}\x1b[0m \x1b[1;4;48;5;200mbold\x1b[0m \x1b[38;2;1;2;3mrgb\x1b[0m\r\n",
                line % 8
            ));
        }
        input.push_str("a very long line that wraps past the edge of twenty columns");
        input.push_str("\r\n\x1b[4:3;58;5;9mcurly\x1b[0m \x1b[9;7;2;3mmixed\x1b[0m");
        round_trip(10, 20, &input);
    }

    #[test]
    fn wide_characters_and_links_survive() {
        round_trip(
            6,
            12,
            "中文宽度✓ 😀 x\r\n\x1b]8;id=a;https://example.com\x1b\\link\x1b]8;;\x1b\\ end\r\n文文文文文文文",
        );
    }

    #[test]
    fn alternate_screen_keeps_both_screens() {
        let (original, _, snapshot) = round_trip(
            8,
            30,
            "shell line 1\r\nshell line 2\r\n$ vim\x1b[?1049h\x1b[H\x1b[2J\x1b[2;7r\x1b[3;1Hin vim \x1b[7mstatus\x1b[0m\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[5 q\x1b]2;vim file\x07",
        );
        assert!(original.term.mode().contains(TermMode::ALT_SCREEN));
        assert!(snapshot.contains("shell line 2"), "{snapshot:?}");

        // Leaving vim in the copy brings the shell screen back.
        let mut copy = TerminalMirror::new(8, 30);
        copy.advance(snapshot.as_bytes());
        copy.advance(b"\x1b[?1049l");
        assert!(copy.rows_for_test().iter().any(|row| row.starts_with('s')));
        let text: String = copy.term.grid()[Line(1)]
            .into_iter()
            .map(|cell| cell.c)
            .collect();
        assert!(text.starts_with("shell line 2"), "{text:?}");
    }

    #[test]
    fn modes_cursor_and_keyboard_flags_survive() {
        round_trip(
            5,
            20,
            "abc\x1b[?1h\x1b=\x1b[?1004h\x1b[?25l\x1b[>5u\x1b[4h\x1b[?7l\x1b[3;9H\x1b[1;32m",
        );
    }

    #[test]
    fn snapshot_of_a_mirror_is_stable_and_leaves_it_unchanged() {
        let mut mirror = TerminalMirror::new(6, 20);
        mirror.advance(b"one\r\ntwo\x1b[?1049h\x1b[Halt");
        let before = mirror.rows_for_test();
        let first = mirror.snapshot();
        assert_eq!(mirror.rows_for_test(), before);
        assert_eq!(mirror.snapshot(), first);
        mirror.advance(b"!");
        assert!(mirror.rows_for_test()[0].starts_with("a"));
    }

    #[test]
    fn scrollback_is_bounded_and_clear_keeps_only_the_cursor_line() {
        let mut mirror = TerminalMirror::new(4, 10);
        for line in 0..(MIRROR_SCROLLBACK_LINES + 500) {
            mirror.advance(format!("{line}\r\n").as_bytes());
        }
        assert_eq!(mirror.term.grid().history_size(), MIRROR_SCROLLBACK_LINES);
        mirror.advance(b"$ prompt");
        mirror.clear();
        assert_eq!(mirror.term.grid().history_size(), 0);
        let rows = mirror.rows_for_test();
        assert!(rows[0].starts_with('$'), "{:?}", &rows[0][..20]);
        assert!(rows[1].starts_with(' '));
        assert_eq!(mirror.term.grid().cursor.point.line.0, 0);
    }

    #[test]
    fn queries_are_never_answered() {
        // A listener that would see replies: the mirror's only records titles.
        let mut mirror = TerminalMirror::new(4, 10);
        mirror.advance(b"\x1b[c\x1b[6n\x1b]11;?\x07\x1b]2;t\x07");
        assert_eq!(mirror.title.lock().unwrap().as_deref(), Some("t"));
    }
}
