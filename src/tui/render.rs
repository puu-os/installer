// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::io::{self, Stdout, Write};
use std::panic;
use std::sync::{Mutex, Once, OnceLock};
use std::thread::{self, ThreadId};
use std::time::Duration;

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind},
    execute,
    style::{Attribute, ResetColor, SetAttribute},
    terminal::{
        self, Clear, ClearType, DisableLineWrap, EnableLineWrap, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};

use crate::tui::message::{expand_char, sanitize_char};
use crate::tui::style::{Color, ColorMode, Modifier, Style};

/// The 16-colour ANSI palette, as the two SGR runs that carry it: 30-37 and
/// 90-97 for the foreground, 40-47 and 100-107 for the background.
///
/// Theme and map colours are 32-bit RGB. Emit quantizes them to these indexes
/// when the terminal only advertised 16 colours.
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
const SYNC_UPDATE_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_UPDATE_END: &[u8] = b"\x1b[?2026l";
const CURSOR_STEADY_BLOCK: &[u8] = b"\x1b[2 q";
const CURSOR_STEADY_BAR: &[u8] = b"\x1b[6 q";
const CURSOR_DEFAULT: &[u8] = b"\x1b[0 q";

static PANIC_HOOK_INIT: Once = Once::new();
static TUI_THREAD: Mutex<Option<ThreadId>> = Mutex::new(None);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CursorStyle {
    #[default]
    SteadyBlock,
    SteadyBar,
}

impl CursorStyle {
    const fn seq(self) -> &'static [u8] {
        match self {
            CursorStyle::SteadyBlock => CURSOR_STEADY_BLOCK,
            CursorStyle::SteadyBar => CURSOR_STEADY_BAR,
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

impl<T: Into<String>> From<(T, Style)> for Span {
    fn from((content, style): (T, Style)) -> Self {
        Self::styled(content, style)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Line {
    spans: Vec<Span>,
}

impl Line {
    pub(crate) fn new(spans: Vec<Span>) -> Self {
        Self { spans }
    }

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

#[derive(Clone, Debug, Default)]
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

/// A single-line box border drawn in CP437 box-drawing glyphs.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Border {
    color: Color,
}

impl Border {
    pub(crate) fn color(color: Color) -> Self {
        Self { color }
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
    cursor_style: CursorStyle,
}

impl Frame {
    pub(crate) fn new(size: Size) -> Self {
        let area = Rect::new(0, 0, size.width, size.height);
        let len = usize::from(size.width) * usize::from(size.height);
        Self {
            area,
            cells: vec![Cell::default(); len],
            cursor: None,
            cursor_style: CursorStyle::default(),
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
        self.cursor_style = CursorStyle::default();
    }

    fn resize(&mut self, size: Size) {
        self.area = Rect::new(0, 0, size.width, size.height);
        let len = usize::from(size.width) * usize::from(size.height);
        self.cells.clear();
        self.cells.resize(len, Cell::default());
        self.cursor = None;
        self.cursor_style = CursorStyle::default();
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

    pub(crate) fn set_cursor_style(&mut self, style: CursorStyle) {
        self.cursor_style = style;
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

impl Widget for Border {
    fn render(self, frame: &mut Frame, rect: Rect) {
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        let style = Style::default().fg(self.color);
        let right = rect.x + rect.width - 1;
        let bottom = rect.y + rect.height - 1;
        let ul = '\u{250c}';
        let ur = '\u{2510}';
        let ll = '\u{2514}';
        let lr = '\u{2518}';
        let hor = '\u{2500}';
        let ver = '\u{2502}';
        if let Some(cell) = frame.cell_mut(rect.x, rect.y) {
            cell.set_char(ul).set_style(style);
        }
        if let Some(cell) = frame.cell_mut(right, rect.y) {
            cell.set_char(ur).set_style(style);
        }
        if let Some(cell) = frame.cell_mut(rect.x, bottom) {
            cell.set_char(ll).set_style(style);
        }
        if let Some(cell) = frame.cell_mut(right, bottom) {
            cell.set_char(lr).set_style(style);
        }
        for x in (rect.x + 1)..right {
            if let Some(cell) = frame.cell_mut(x, rect.y) {
                cell.set_char(hor).set_style(style);
            }
            if let Some(cell) = frame.cell_mut(x, bottom) {
                cell.set_char(hor).set_style(style);
            }
        }
        for y in (rect.y + 1)..bottom {
            if let Some(cell) = frame.cell_mut(rect.x, y) {
                cell.set_char(ver).set_style(style);
            }
            if let Some(cell) = frame.cell_mut(right, y) {
                cell.set_char(ver).set_style(style);
            }
        }
    }
}

impl Widget for Paragraph {
    fn render(self, frame: &mut Frame, rect: Rect) {
        if self.style.has_background() {
            Block { style: self.style }.render(frame, rect);
        }

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

/// Width in cells after applying the renderer's character folding rules.
pub(crate) fn rendered_width(text: &str) -> u16 {
    text.chars()
        .map(|ch| expand_char(ch).map_or(1, |expanded| expanded.chars().count() as u16))
        .sum()
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn detect_color_mode() -> ColorMode {
    if std::env::var_os("NO_COLOR").is_some() {
        return ColorMode::Mono;
    }
    let term = env_nonempty("TERM");
    if term.as_deref() == Some("dumb") {
        return ColorMode::Mono;
    }
    let colorterm = env_nonempty("COLORTERM");
    if colorterm.as_deref() == Some("truecolor") || colorterm.as_deref() == Some("24bit") {
        return ColorMode::TrueColor;
    }
    if let Some(term) = term.as_deref() {
        if term.ends_with("direct") || term.ends_with("truecolor") {
            return ColorMode::TrueColor;
        }
        if term.ends_with("256color") || term.ends_with("256") {
            return ColorMode::Ansi256;
        }
    }
    ColorMode::Ansi16
}

pub(crate) fn color_mode() -> ColorMode {
    static MODE: OnceLock<ColorMode> = OnceLock::new();
    *MODE.get_or_init(detect_color_mode)
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
/// Generic for the writer so tests can render into a `Vec<u8>` without a tty.
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
    wire_cursor_style: CursorStyle,
    cursor_shown: bool,
    full_repaint: bool,
}

impl<W: Write> Terminal<W> {
    pub(crate) fn new(out: W) -> Self {
        let color_mode = color_mode();
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
            wire_cursor_style: CursorStyle::default(),
            cursor_shown: false,
            full_repaint: true,
        }
    }
    /// Forget everything we believe about the terminal's escape state. Called
    /// whenever something else may have written to the terminal behind our back,
    /// so the next frame re-establishes it.
    fn reset_wire_state(&mut self) {
        self.wire_style = Style::default();
        self.wire_cursor = None;
        self.wire_cursor_style = CursorStyle::default();
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
        let result = restore_terminal(&mut self.out).and_then(|()| self.out.flush());
        self.active = false;
        self.reset_wire_state();
        let _ = terminal::disable_raw_mode();
        result
    }

    pub(crate) fn draw<F>(&mut self, draw: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame),
    {
        let size = screen_size()?;
        if size != self.back.size() {
            self.back.resize(size);
            self.front.resize(size);
            self.full_repaint = true;
        }

        self.back.blank();
        draw(&mut self.back);
        self.emit();

        if self.bytes.is_empty() {
            std::mem::swap(&mut self.front, &mut self.back);
            return Ok(());
        }

        if let Err(error) = self
            .out
            .write_all(&self.bytes)
            .and_then(|()| self.out.flush())
        {
            // A partial write may have entered synchronized-output mode without
            // reaching its terminator. Best-effort cleanup prevents a supporting
            // terminal from holding subsequent output until its safety timeout.
            let _ = self.out.write_all(SYNC_UPDATE_END);
            let _ = self.out.flush();
            self.reset_wire_state();
            return Err(error);
        }

        std::mem::swap(&mut self.front, &mut self.back);
        Ok(())
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
            wire_cursor_style,
            cursor_shown,
            full_repaint,
            ..
        } = self;

        bytes.clear();
        // DEC private mode 2026 makes each framebuffer diff visible as one
        // presentation. Without it, animated map and log updates can expose a
        // partly painted frame as the terminal consumes this byte stream.
        bytes.extend_from_slice(SYNC_UPDATE_BEGIN);
        let payload_start = bytes.len();

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
                if back.cursor_style != *wire_cursor_style {
                    bytes.extend_from_slice(back.cursor_style.seq());
                    *wire_cursor_style = back.cursor_style;
                }
            }
            None => {
                if *cursor_shown {
                    bytes.extend_from_slice(CURSOR_HIDE);
                    *cursor_shown = false;
                }
            }
        }

        if bytes.len() == payload_start {
            bytes.clear();
        } else {
            bytes.extend_from_slice(SYNC_UPDATE_END);
        }
    }
}

fn restore_terminal<W: Write>(out: &mut W) -> io::Result<()> {
    // Continue cleanup after errors so the terminal is restored as completely as possible.
    let mut result = out.write_all(CURSOR_DEFAULT).err();
    if let Err(error) = execute!(
        out,
        ResetColor,
        SetAttribute(Attribute::Reset),
        Show,
        EnableLineWrap,
        LeaveAlternateScreen
    ) && result.is_none()
    {
        result = Some(error);
    }

    result.map_or(Ok(()), Err)
}

pub(crate) struct TuiPanicHookGuard {
    thread: ThreadId,
}

impl Drop for TuiPanicHookGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = TUI_THREAD.lock()
            && active.as_ref() == Some(&self.thread)
        {
            *active = None;
        }
    }
}

pub(crate) fn install_panic_hook() -> TuiPanicHookGuard {
    PANIC_HOOK_INIT.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let current = thread::current().id();
            let tui_panicked = TUI_THREAD
                .lock()
                .is_ok_and(|active| active.as_ref() == Some(&current));
            if tui_panicked {
                restore_terminal_now();
            }
            previous(info);
        }));
    });

    let thread = thread::current().id();
    if let Ok(mut active) = TUI_THREAD.lock() {
        *active = Some(thread);
    }
    TuiPanicHookGuard { thread }
}

fn restore_terminal_now() {
    let _ = terminal::disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = restore_terminal(&mut stdout);
    let _ = stdout.flush();
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

fn emit_sgr_color(bytes: &mut Vec<u8>, color: Color, foreground: bool, mode: ColorMode) {
    let (r, g, b) = color.channels();
    match mode {
        ColorMode::Mono => {}
        ColorMode::Ansi16 => {
            let index = color.ansi16_index();
            let table = if foreground { FG_SGR } else { BG_SGR };
            if let Some(seq) = table.get(usize::from(index)).or_else(|| table.get(15)) {
                bytes.extend_from_slice(seq);
            }
        }
        ColorMode::Ansi256 => {
            bytes.extend_from_slice(if foreground {
                b"\x1b[38;5;"
            } else {
                b"\x1b[48;5;"
            });
            push_num(bytes, u16::from(color.ansi256_index()));
            bytes.push(b'm');
        }
        ColorMode::TrueColor => {
            bytes.extend_from_slice(if foreground {
                b"\x1b[38;2;"
            } else {
                b"\x1b[48;2;"
            });
            push_num(bytes, u16::from(r));
            bytes.push(b';');
            push_num(bytes, u16::from(g));
            bytes.push(b';');
            push_num(bytes, u16::from(b));
            bytes.push(b'm');
        }
    }
}

/// Emit only the escape sequences needed to move the terminal from the style it
/// is in (`wire`) to `next`, and update `wire` to match.
///
/// An individual attribute cannot be switched off, so dropping one costs a full
/// SGR reset — which also drops the colours, hence the re-emit that follows.
fn emit_style(bytes: &mut Vec<u8>, wire: &mut Style, next: Style, color_mode: ColorMode) {
    *wire = wire.quantize(color_mode);
    let next = next.quantize(color_mode);
    if *wire == next {
        return;
    }

    if wire.drops_modifiers(next) {
        bytes.extend_from_slice(SGR_RESET);
        *wire = Style::default();
    }

    let wire_reverse = wire.has_modifier(Modifier::REVERSE);
    let next_reverse = next.has_modifier(Modifier::REVERSE);

    match color_mode {
        ColorMode::Mono => {
            // In mono mode the only visible fill is an explicit reverse.
            // Theme backgrounds must not invert the whole chrome.
            let wire_fill = wire_reverse;
            let next_fill = next_reverse;
            if wire_fill != next_fill {
                bytes.extend_from_slice(if next_fill {
                    SGR_REVERSE
                } else {
                    SGR_NO_REVERSE
                });
            }
        }
        ColorMode::Ansi16 | ColorMode::Ansi256 | ColorMode::TrueColor => {
            if wire.foreground() != next.foreground() {
                match next.foreground() {
                    Some(color) => emit_sgr_color(bytes, color, true, color_mode),
                    None => bytes.extend_from_slice(SGR_FG_DEFAULT),
                }
            }
            if wire.background() != next.background() {
                match next.background() {
                    Some(color) => emit_sgr_color(bytes, color, false, color_mode),
                    None => bytes.extend_from_slice(SGR_BG_DEFAULT),
                }
            }
            if wire_reverse != next_reverse {
                bytes.extend_from_slice(if next_reverse {
                    SGR_REVERSE
                } else {
                    SGR_NO_REVERSE
                });
            }
        }
    }

    if next.has_modifier(Modifier::BOLD) && !wire.has_modifier(Modifier::BOLD) {
        bytes.extend_from_slice(SGR_BOLD);
    }
    if next.has_modifier(Modifier::DIM) && !wire.has_modifier(Modifier::DIM) {
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

pub(crate) fn screen_size() -> io::Result<Size> {
    let (width, height) = terminal::size()?;
    Ok(Size { width, height })
}

pub(crate) fn fatal_io(err: &io::Error, ctx: &str) -> ! {
    restore_terminal_now();
    eprintln!("Terminal error ({ctx}): {err}");
    std::process::exit(1);
}

pub(crate) fn tui_or_exit<T>(result: io::Result<T>, ctx: &str) -> T {
    match result {
        Ok(v) => v,
        Err(e) => fatal_io(&e, ctx),
    }
}

/// A terminal event the runtime reacts to.
pub(crate) enum Input {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize,
}

fn to_input(event: &Event) -> Option<Input> {
    match event {
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
            Some(Input::Key(*key))
        }
        Event::Mouse(mouse) => Some(Input::Mouse(*mouse)),
        Event::Resize(_, _) => Some(Input::Resize),
        _ => None,
    }
}

pub(crate) fn poll_input(timeout: Duration) -> Option<Input> {
    if !tui_or_exit(event::poll(timeout), "event::poll") {
        return None;
    }
    let event = tui_or_exit(event::read(), "event::read");
    to_input(&event)
}

/// Translate a crossterm mouse event into the subset the installer acts on.
pub(crate) fn mouse_kind(kind: MouseEventKind) -> Option<crate::tui::message::MouseKind> {
    match kind {
        MouseEventKind::Down(MouseButton::Left) => Some(crate::tui::message::MouseKind::LeftDown),
        MouseEventKind::ScrollUp => Some(crate::tui::message::MouseKind::ScrollUp),
        MouseEventKind::ScrollDown => Some(crate::tui::message::MouseKind::ScrollDown),
        _ => None,
    }
}
