// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::io::{self, Stdout, Write};
use std::process::{Command, ExitStatus};
use std::time::Duration;

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyEvent, KeyEventKind},
    execute,
    style::{Attribute, ResetColor, SetAttribute},
    terminal::{
        self, Clear, ClearType, DisableLineWrap, EnableLineWrap, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};

/// The 16-colour ANSI palette, as the two SGR runs that carry it: 30-37 and
/// 90-97 for the foreground, 40-47 and 100-107 for the background.
///
/// These are emitted by hand rather than through crossterm, which renders even
/// a plain ANSI colour as a 256-colour `38;5;N` sequence. The short form is half
/// the bytes and asks less of the terminal — a plain VT100-class serial console
/// understands it — and the installer only ever uses these 16 colours.
const FG_SGR: [&[u8]; 16] = [
    b"\x1b[30m",
    b"\x1b[31m",
    b"\x1b[32m",
    b"\x1b[33m",
    b"\x1b[34m",
    b"\x1b[35m",
    b"\x1b[36m",
    b"\x1b[37m",
    b"\x1b[90m",
    b"\x1b[91m",
    b"\x1b[92m",
    b"\x1b[93m",
    b"\x1b[94m",
    b"\x1b[95m",
    b"\x1b[96m",
    b"\x1b[97m",
];
const BG_SGR: [&[u8]; 16] = [
    b"\x1b[40m",
    b"\x1b[41m",
    b"\x1b[42m",
    b"\x1b[43m",
    b"\x1b[44m",
    b"\x1b[45m",
    b"\x1b[46m",
    b"\x1b[47m",
    b"\x1b[100m",
    b"\x1b[101m",
    b"\x1b[102m",
    b"\x1b[103m",
    b"\x1b[104m",
    b"\x1b[105m",
    b"\x1b[106m",
    b"\x1b[107m",
];
const SGR_RESET: &[u8] = b"\x1b[0m";
const SGR_FG_DEFAULT: &[u8] = b"\x1b[39m";
const SGR_BG_DEFAULT: &[u8] = b"\x1b[49m";
const SGR_BOLD: &[u8] = b"\x1b[1m";
const SGR_DIM: &[u8] = b"\x1b[2m";
const SGR_REVERSE: &[u8] = b"\x1b[7m";
const SGR_NO_REVERSE: &[u8] = b"\x1b[27m";
const CLEAR_SCREEN: &[u8] = b"\x1b[2J";
const CURSOR_SHOW: &[u8] = b"\x1b[?25h";
const CURSOR_HIDE: &[u8] = b"\x1b[?25l";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Color {
    Indexed(u8),
}

impl Color {
    /// Index into the 16-colour palette. Anything above the palette clamps to
    /// white rather than running off the end of the SGR tables.
    const fn index(self) -> usize {
        let Self::Indexed(index) = self;
        if index > 15 { 15 } else { index as usize }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Modifier(u8);

impl Modifier {
    pub(crate) const BOLD: Self = Self(1 << 0);
    pub(crate) const DIM: Self = Self(1 << 1);

    fn contains(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Style {
    fg: Option<Color>,
    bg: Option<Color>,
    modifiers: Modifier,
}

impl Style {
    pub(crate) fn fg(mut self, color: Color) -> Self {
        self.fg = Some(color);
        self
    }

    pub(crate) fn bg(mut self, color: Color) -> Self {
        self.bg = Some(color);
        self
    }

    pub(crate) fn add_modifier(mut self, modifier: Modifier) -> Self {
        self.modifiers.0 |= modifier.0;
        self
    }

    fn patch(self, overlay: Self) -> Self {
        Self {
            fg: overlay.fg.or(self.fg),
            bg: overlay.bg.or(self.bg),
            modifiers: Modifier(self.modifiers.0 | overlay.modifiers.0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Size {
    pub(crate) width: u16,
    pub(crate) height: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rect {
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) width: u16,
    pub(crate) height: u16,
}

impl Rect {
    pub(crate) const fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Span {
    content: String,
    style: Style,
}

impl Span {
    pub(crate) fn raw<T: Into<String>>(content: T) -> Self {
        Self {
            content: content.into(),
            style: Style::default(),
        }
    }

    pub(crate) fn styled<T: Into<String>>(content: T, style: Style) -> Self {
        Self {
            content: content.into(),
            style,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Line {
    spans: Vec<Span>,
}

impl Line {
    fn plain<T: Into<String>>(content: T) -> Self {
        Self {
            spans: vec![Span::raw(content)],
        }
    }
}

impl From<Vec<Span>> for Line {
    fn from(spans: Vec<Span>) -> Self {
        Self { spans }
    }
}

impl From<String> for Line {
    fn from(content: String) -> Self {
        Self::plain(content)
    }
}

impl From<&str> for Line {
    fn from(content: &str) -> Self {
        Self::plain(content)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Paragraph {
    lines: Vec<Line>,
    style: Style,
}

impl Paragraph {
    pub(crate) fn new<T: Into<Paragraph>>(content: T) -> Self {
        content.into()
    }

    pub(crate) fn style(mut self, style: Style) -> Self {
        self.style = self.style.patch(style);
        self
    }
}

impl From<&str> for Paragraph {
    fn from(content: &str) -> Self {
        Self {
            lines: content.lines().map(Line::from).collect(),
            style: Style::default(),
        }
    }
}

impl From<String> for Paragraph {
    fn from(content: String) -> Self {
        Self::from(content.as_str())
    }
}

impl From<Line> for Paragraph {
    fn from(line: Line) -> Self {
        Self {
            lines: vec![line],
            style: Style::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Block {
    style: Style,
}

impl Block {
    pub(crate) fn style(mut self, style: Style) -> Self {
        self.style = self.style.patch(style);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cell {
    ch: char,
    style: Style,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            style: Style::default(),
        }
    }
}

impl Cell {
    pub(crate) fn set_char(&mut self, ch: char) -> &mut Self {
        self.ch = sanitize_char(ch);
        self
    }

    pub(crate) fn set_fg(&mut self, color: Color) -> &mut Self {
        self.style.fg = Some(color);
        self
    }

    pub(crate) fn set_bg(&mut self, color: Color) -> &mut Self {
        self.style.bg = Some(color);
        self
    }

    pub(crate) fn set_style(&mut self, style: Style) -> &mut Self {
        self.style = style;
        self
    }
}

pub(crate) struct Buffer<'a> {
    width: u16,
    height: u16,
    cells: &'a mut [Cell],
}

impl Buffer<'_> {
    pub(crate) fn cell_mut(&mut self, pos: (u16, u16)) -> Option<&mut Cell> {
        let (x, y) = pos;
        if x >= self.width || y >= self.height {
            return None;
        }
        let index = usize::from(y) * usize::from(self.width) + usize::from(x);
        self.cells.get_mut(index)
    }
}

#[derive(Clone)]
pub(crate) struct Frame {
    area: Rect,
    cells: Vec<Cell>,
    cursor: Option<(u16, u16)>,
}

impl Frame {
    fn new(size: Size) -> Self {
        let area = Rect::new(0, 0, size.width, size.height);
        let len = usize::from(size.width) * usize::from(size.height);
        Self {
            area,
            cells: vec![Cell::default(); len],
            cursor: None,
        }
    }

    pub(crate) const fn area(&self) -> Rect {
        self.area
    }

    const fn size(&self) -> Size {
        Size {
            width: self.area.width,
            height: self.area.height,
        }
    }

    /// Reset every cell to the blank default. `Vec::fill` on a `Copy` cell is a
    /// memset, so this replaces the per-frame `Vec` allocation of the old
    /// renderer.
    fn blank(&mut self) {
        self.cells.fill(Cell::default());
        self.cursor = None;
    }

    fn resize(&mut self, size: Size) {
        self.area = Rect::new(0, 0, size.width, size.height);
        let len = usize::from(size.width) * usize::from(size.height);
        self.cells.clear();
        self.cells.resize(len, Cell::default());
        self.cursor = None;
    }

    pub(crate) fn render_widget<W: Widget>(&mut self, widget: W, rect: Rect) {
        widget.render(self, clip_rect(rect, self.area));
    }

    pub(crate) fn buffer_mut(&mut self) -> Buffer<'_> {
        Buffer {
            width: self.area.width,
            height: self.area.height,
            cells: &mut self.cells,
        }
    }

    pub(crate) fn set_cursor_position(&mut self, pos: (u16, u16)) {
        let (x, y) = pos;
        if x < self.area.width && y < self.area.height {
            self.cursor = Some((x, y));
        }
    }

    fn cell_mut(&mut self, x: u16, y: u16) -> Option<&mut Cell> {
        if x >= self.area.width || y >= self.area.height {
            return None;
        }
        let index = usize::from(y) * usize::from(self.area.width) + usize::from(x);
        self.cells.get_mut(index)
    }

    /// Read a cell that is known to be inside the frame.
    fn cell_at(&self, x: u16, y: u16) -> Cell {
        let index = usize::from(y) * usize::from(self.area.width) + usize::from(x);
        self.cells[index]
    }
}

pub(crate) trait Widget {
    fn render(self, frame: &mut Frame, rect: Rect);
}

impl Widget for Block {
    fn render(self, frame: &mut Frame, rect: Rect) {
        for y in rect.y..rect.y.saturating_add(rect.height) {
            for x in rect.x..rect.x.saturating_add(rect.width) {
                if let Some(cell) = frame.cell_mut(x, y) {
                    cell.set_char(' ').set_style(self.style);
                }
            }
        }
    }
}

impl Widget for Paragraph {
    fn render(self, frame: &mut Frame, rect: Rect) {
        for (row, line) in self.lines.into_iter().enumerate() {
            if row >= usize::from(rect.height) {
                break;
            }
            render_line(
                frame,
                rect.x,
                rect.y + row as u16,
                rect.width,
                line,
                self.style,
            );
        }
    }
}

fn render_line(frame: &mut Frame, x: u16, y: u16, width: u16, line: Line, base_style: Style) {
    let mut cursor_x = x;
    let limit = x.saturating_add(width);

    for span in line.spans {
        let style = base_style.patch(span.style);
        for ch in span.content.chars() {
            let mut utf8 = [0u8; 4];
            let text = match expand_char(ch) {
                Some(text) => text,
                None => &*ch.encode_utf8(&mut utf8),
            };
            for ch in text.chars() {
                if cursor_x >= limit {
                    return;
                }
                if let Some(cell) = frame.cell_mut(cursor_x, y) {
                    cell.set_char(ch).set_style(style);
                }
                cursor_x += 1;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColorMode {
    Mono,
    Ansi16,
}

/// A double-buffered terminal that writes one diffed, run-batched frame per
/// `draw` call.
///
/// The renderer models the escape state the terminal is actually in (`wire_*`)
/// so a frame only emits what changed: cells that differ from the previous
/// frame, the SGR sequences that differ from the last one emitted, and a cursor
/// move only when the cursor is not already in place. An unchanged frame emits
/// nothing at all and never touches the writer.
///
/// Generic over the writer so tests can render into a `Vec<u8>` without a tty.
pub(crate) struct Terminal<W: Write = Stdout> {
    out: W,
    active: bool,
    color_mode: ColorMode,
    /// The frame being drawn.
    back: Frame,
    /// What the terminal is currently showing.
    front: Frame,
    /// Reusable escape-sequence buffer, flushed once per frame.
    bytes: Vec<u8>,
    /// Reusable run of same-styled characters, printed as one string.
    run: String,
    wire_style: Style,
    wire_cursor: Option<(u16, u16)>,
    cursor_shown: bool,
    full_repaint: bool,
    /// Size to render at instead of asking the tty. Only set by headless test
    /// terminals; `None` in production.
    size_override: Option<Size>,
}

impl<W: Write> Terminal<W> {
    pub(crate) fn new(out: W) -> Self {
        let color_mode = if std::env::var_os("NO_COLOR").is_some()
            || std::env::var("TERM").is_ok_and(|term| term == "dumb")
        {
            ColorMode::Mono
        } else {
            ColorMode::Ansi16
        };
        let empty = Size {
            width: 0,
            height: 0,
        };
        Self {
            out,
            active: false,
            color_mode,
            back: Frame::new(empty),
            front: Frame::new(empty),
            bytes: Vec::new(),
            run: String::new(),
            wire_style: Style::default(),
            wire_cursor: None,
            cursor_shown: false,
            full_repaint: true,
            size_override: None,
        }
    }

    /// Forget everything we believe about the terminal's escape state. Called
    /// whenever something else may have written to the terminal behind our back
    /// (`activate` after `suspend_command`), so the next frame re-establishes it.
    fn reset_wire_state(&mut self) {
        self.wire_style = Style::default();
        self.wire_cursor = None;
        self.cursor_shown = false;
        self.full_repaint = true;
        self.front.blank();
    }

    pub(crate) fn activate(&mut self) -> io::Result<()> {
        if self.active {
            return Ok(());
        }
        terminal::enable_raw_mode()?;
        execute!(
            self.out,
            EnterAlternateScreen,
            DisableLineWrap,
            Hide,
            ResetColor,
            SetAttribute(Attribute::Reset),
            Clear(ClearType::All),
            MoveTo(0, 0)
        )?;
        self.out.flush()?;
        self.reset_wire_state();
        self.active = true;
        Ok(())
    }

    pub(crate) fn deactivate(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        let result = execute!(
            self.out,
            ResetColor,
            SetAttribute(Attribute::Reset),
            Show,
            EnableLineWrap,
            LeaveAlternateScreen
        )
        .and_then(|()| self.out.flush());
        self.active = false;
        self.reset_wire_state();
        let _ = terminal::disable_raw_mode();
        result
    }

    pub(crate) fn draw<F>(&mut self, draw: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame),
    {
        let size = match self.size_override {
            Some(size) => size,
            None => screen_size()?,
        };
        if size != self.back.size() {
            self.back.resize(size);
            self.front.resize(size);
            self.full_repaint = true;
        }

        self.back.blank();
        draw(&mut self.back);
        self.emit();
        std::mem::swap(&mut self.front, &mut self.back);

        if self.bytes.is_empty() {
            return Ok(());
        }
        self.out.write_all(&self.bytes)?;
        self.out.flush()
    }

    /// Diff `back` against `front` into `bytes`. Writing to a `Vec` cannot fail,
    /// so the whole hot path is infallible and the caller does the one write.
    fn emit(&mut self) {
        let Self {
            color_mode,
            back,
            front,
            bytes,
            run,
            wire_style,
            wire_cursor,
            cursor_shown,
            full_repaint,
            ..
        } = self;

        bytes.clear();

        if *full_repaint {
            bytes.extend_from_slice(SGR_RESET);
            bytes.extend_from_slice(CLEAR_SCREEN);
            // A cleared screen is blank in the default style, so diffing against
            // that keeps a full repaint from re-emitting the empty cells.
            front.blank();
            *wire_style = Style::default();
            *wire_cursor = None;
            *full_repaint = false;
        }

        let Size { width, height } = back.size();
        for y in 0..height {
            let mut x = 0;
            while x < width {
                if back.cell_at(x, y) == front.cell_at(x, y) {
                    x += 1;
                    continue;
                }

                if *wire_cursor != Some((x, y)) {
                    push_move_to(bytes, x, y);
                }

                while x < width && back.cell_at(x, y) != front.cell_at(x, y) {
                    let cell = back.cell_at(x, y);
                    if cell.style != *wire_style {
                        flush_run(bytes, run);
                        emit_style(bytes, wire_style, cell.style, *color_mode);
                    }
                    run.push(cell.ch);
                    x += 1;
                }
                flush_run(bytes, run);

                // Line wrap is disabled, so a run that reaches the right margin
                // leaves the cursor pinned there rather than on the next row.
                *wire_cursor = if x < width { Some((x, y)) } else { None };
            }
        }

        match back.cursor {
            Some((x, y)) => {
                if *wire_cursor != Some((x, y)) {
                    push_move_to(bytes, x, y);
                    *wire_cursor = Some((x, y));
                }
                if !*cursor_shown {
                    bytes.extend_from_slice(CURSOR_SHOW);
                    *cursor_shown = true;
                }
            }
            None => {
                if *cursor_shown {
                    bytes.extend_from_slice(CURSOR_HIDE);
                    *cursor_shown = false;
                }
            }
        }
    }

    pub(crate) fn suspend_command(&mut self, command: &mut Command) -> io::Result<ExitStatus> {
        self.deactivate()?;
        let status = command.status();
        let reactivate = self.activate();
        match (status, reactivate) {
            (Ok(status), Ok(())) => Ok(status),
            (Err(err), _) | (_, Err(err)) => Err(err),
        }
    }
}

impl<W: Write> Drop for Terminal<W> {
    fn drop(&mut self) {
        let _ = self.deactivate();
    }
}

fn flush_run(bytes: &mut Vec<u8>, run: &mut String) {
    if run.is_empty() {
        return;
    }
    bytes.extend_from_slice(run.as_bytes());
    run.clear();
}

fn push_num(bytes: &mut Vec<u8>, mut n: u16) {
    let mut digits = [0u8; 5];
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    bytes.extend_from_slice(&digits[i..]);
}

fn push_move_to(bytes: &mut Vec<u8>, x: u16, y: u16) {
    bytes.extend_from_slice(b"\x1b[");
    push_num(bytes, y + 1);
    bytes.push(b';');
    push_num(bytes, x + 1);
    bytes.push(b'H');
}

/// Emit only the escape sequences needed to move the terminal from the style it
/// is in (`wire`) to `next`, and update `wire` to match.
///
/// An individual attribute cannot be switched off, so dropping one costs a full
/// SGR reset — which also drops the colours, hence the re-emit that follows.
fn emit_style(bytes: &mut Vec<u8>, wire: &mut Style, next: Style, color_mode: ColorMode) {
    if *wire == next {
        return;
    }

    if wire.modifiers.0 & !next.modifiers.0 != 0 {
        bytes.extend_from_slice(SGR_RESET);
        *wire = Style::default();
    }

    match color_mode {
        ColorMode::Ansi16 => {
            if wire.fg != next.fg {
                match next.fg {
                    Some(color) => bytes.extend_from_slice(FG_SGR[color.index()]),
                    None => bytes.extend_from_slice(SGR_FG_DEFAULT),
                }
            }
            if wire.bg != next.bg {
                match next.bg {
                    Some(color) => bytes.extend_from_slice(BG_SGR[color.index()]),
                    None => bytes.extend_from_slice(SGR_BG_DEFAULT),
                }
            }
        }
        ColorMode::Mono => {
            if wire.bg.is_some() != next.bg.is_some() {
                if next.bg.is_some() {
                    bytes.extend_from_slice(SGR_REVERSE);
                } else {
                    bytes.extend_from_slice(SGR_NO_REVERSE);
                }
            }
        }
    }

    if next.modifiers.contains(Modifier::BOLD) && !wire.modifiers.contains(Modifier::BOLD) {
        bytes.extend_from_slice(SGR_BOLD);
    }
    if next.modifiers.contains(Modifier::DIM) && !wire.modifiers.contains(Modifier::DIM) {
        bytes.extend_from_slice(SGR_DIM);
    }

    *wire = next;
}

fn clip_rect(rect: Rect, area: Rect) -> Rect {
    let x = rect.x.min(area.width);
    let y = rect.y.min(area.height);
    let width = rect.width.min(area.width.saturating_sub(x));
    let height = rect.height.min(area.height.saturating_sub(y));
    Rect::new(x, y, width, height)
}

/// The non-ASCII glyphs a cell is allowed to hold.
///
/// Every one of these is a CP437 glyph, so it exists in the VGA and Linux
/// console fonts as well as in every terminal-emulator font. Quadrant blocks
/// (U+2596..U+259F) and Braille (U+2800..) are deliberately absent: they are
/// not in the console fonts, and the installer's primary target is a Linux VT.
const fn is_console_glyph(ch: char) -> bool {
    matches!(
        ch,
        '\u{00b0}'      // degree sign
        | '\u{00b7}'    // middle dot
        | '\u{2500}'    // box drawings light horizontal
        | '\u{2502}'    // box drawings light vertical
        | '\u{250c}' | '\u{2510}' | '\u{2514}' | '\u{2518}'  // light corners
        | '\u{251c}' | '\u{2524}' | '\u{252c}' | '\u{2534}'  // light tees
        | '\u{253c}'    // box drawings light vertical and horizontal
        | '\u{2580}'    // upper half block
        | '\u{2584}'    // lower half block
        | '\u{2588}'    // full block
        | '\u{258c}' | '\u{2590}'   // left half block, right half block
        | '\u{2591}' | '\u{2592}' | '\u{2593}'  // light, medium, dark shade
        | '\u{25a0}' // black square
    )
}

/// Fold one source character into the single narrow glyph a cell can hold.
///
/// Anything wide, combining, control or simply absent from the console font
/// becomes `?`, so no string — a disk model name, a timezone, a subprocess log
/// line — can ever break the cell grid.
fn sanitize_char(ch: char) -> char {
    match ch {
        ' ' | '\t' | '\n' | '\r' => ' ',
        c if c.is_ascii_graphic() => c,
        '\u{2014}' | '\u{2013}' | '\u{2212}' => '-', // em dash, en dash, minus
        '\u{2018}' | '\u{2019}' => '\'',             // curly single quotes
        '\u{201c}' | '\u{201d}' => '"',              // curly double quotes
        c if is_console_glyph(c) => c,
        _ => '?',
    }
}

/// Characters that fold to *more than one* cell. Kept separate from
/// `sanitize_char`, which is per-cell and cannot widen.
fn expand_char(ch: char) -> Option<&'static str> {
    match ch {
        '\u{2192}' => Some("->"),
        '\u{2190}' => Some("<-"),
        '\u{2265}' => Some(">="),
        '\u{2264}' => Some("<="),
        _ => None,
    }
}

pub(crate) fn screen_size() -> io::Result<Size> {
    let (width, height) = terminal::size()?;
    Ok(Size { width, height })
}

pub(crate) fn fatal_io(err: &io::Error, ctx: &str) -> ! {
    let _ = terminal::disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(
        stdout,
        ResetColor,
        SetAttribute(Attribute::Reset),
        Show,
        LeaveAlternateScreen
    );
    let _ = stdout.flush();
    eprintln!("Terminal error ({ctx}): {err}");
    std::process::exit(1);
}

pub(crate) fn tui_or_exit<T>(result: io::Result<T>, ctx: &str) -> T {
    match result {
        Ok(v) => v,
        Err(e) => fatal_io(&e, ctx),
    }
}

/// A terminal event a screen cares about. Screens that redraw unconditionally
/// use the `*_key_event` helpers and never see `Resize`; screens that only
/// redraw when dirty must handle it.
pub(crate) enum Input {
    Key(KeyEvent),
    Resize,
}

fn to_input(event: &Event) -> Option<Input> {
    match event {
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
            Some(Input::Key(*key))
        }
        Event::Resize(_, _) => Some(Input::Resize),
        _ => None,
    }
}

pub(crate) fn read_input() -> Option<Input> {
    let event = tui_or_exit(event::read(), "event::read");
    to_input(&event)
}

pub(crate) fn poll_input(timeout: Duration) -> Option<Input> {
    if !tui_or_exit(event::poll(timeout), "event::poll") {
        return None;
    }
    read_input()
}

pub(crate) fn next_key_event() -> Option<KeyEvent> {
    match read_input() {
        Some(Input::Key(key)) => Some(key),
        _ => None,
    }
}

pub(crate) fn poll_key_event(timeout: Duration) -> Option<KeyEvent> {
    match poll_input(timeout) {
        Some(Input::Key(key)) => Some(key),
        _ => None,
    }
}
