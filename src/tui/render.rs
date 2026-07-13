// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::io::{self, Stdout, Write};
use std::process::{Command, ExitStatus};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyEvent, KeyEventKind},
    execute, queue,
    style::{
        Attribute, Color as CrosstermColor, Print, ResetColor, SetAttribute, SetBackgroundColor,
        SetForegroundColor,
    },
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Color {
    Indexed(u8),
}

impl Color {
    fn to_crossterm(self) -> CrosstermColor {
        match self {
            Self::Indexed(index) => CrosstermColor::AnsiValue(index),
        }
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
        for ch in sanitize_text(&span.content).chars() {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColorMode {
    Mono,
    Ansi16,
}

pub(crate) struct Terminal {
    stdout: Stdout,
    active: bool,
    color_mode: ColorMode,
    previous_frame: Option<Frame>,
}

impl Terminal {
    pub(crate) fn new(stdout: Stdout) -> Self {
        let color_mode = if std::env::var_os("NO_COLOR").is_some()
            || std::env::var("TERM").is_ok_and(|term| term == "dumb")
        {
            ColorMode::Mono
        } else {
            ColorMode::Ansi16
        };
        Self {
            stdout,
            active: false,
            color_mode,
            previous_frame: None,
        }
    }

    pub(crate) fn activate(&mut self) -> io::Result<()> {
        if self.active {
            return Ok(());
        }
        terminal::enable_raw_mode()?;
        execute!(
            self.stdout,
            EnterAlternateScreen,
            Hide,
            ResetColor,
            SetAttribute(Attribute::Reset),
            Clear(ClearType::All),
            MoveTo(0, 0)
        )?;
        self.stdout.flush()?;
        self.previous_frame = None;
        self.active = true;
        Ok(())
    }

    pub(crate) fn deactivate(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        let result = execute!(
            self.stdout,
            ResetColor,
            SetAttribute(Attribute::Reset),
            Show,
            LeaveAlternateScreen
        )
        .and_then(|()| self.stdout.flush());
        self.active = false;
        self.previous_frame = None;
        let _ = terminal::disable_raw_mode();
        result
    }

    pub(crate) fn size() -> io::Result<Size> {
        let (width, height) = terminal::size()?;
        Ok(Size { width, height })
    }

    pub(crate) fn draw<F>(&mut self, draw: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame),
    {
        let mut frame = Frame::new(Self::size()?);
        draw(&mut frame);
        self.flush_frame(&frame)
    }

    fn flush_frame(&mut self, frame: &Frame) -> io::Result<()> {
        queue!(
            self.stdout,
            Hide,
            ResetColor,
            SetAttribute(Attribute::Reset)
        )?;

        let previous = self.previous_frame.as_ref();
        for y in 0..frame.area.height {
            let mut x = 0;
            while x < frame.area.width {
                if !cell_changed(previous, frame, x, y) {
                    x += 1;
                    continue;
                }

                queue!(self.stdout, MoveTo(x, y))?;
                let mut style = None;
                while x < frame.area.width && cell_changed(previous, frame, x, y) {
                    let cell = frame.cells
                        [usize::from(y) * usize::from(frame.area.width) + usize::from(x)];
                    if style != Some(cell.style) {
                        apply_style(&mut self.stdout, cell.style, self.color_mode)?;
                        style = Some(cell.style);
                    }
                    queue!(self.stdout, Print(cell.ch))?;
                    x += 1;
                }
            }
        }

        queue!(
            self.stdout,
            ResetColor,
            SetAttribute(Attribute::Reset),
            MoveTo(0, 0)
        )?;
        if let Some((x, y)) = frame.cursor {
            queue!(self.stdout, Show, MoveTo(x, y))?;
        } else {
            queue!(self.stdout, Hide)?;
        }
        self.stdout.flush()?;
        self.previous_frame = Some(frame.clone());
        Ok(())
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

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.deactivate();
    }
}

fn cell_changed(previous: Option<&Frame>, frame: &Frame, x: u16, y: u16) -> bool {
    let index = usize::from(y) * usize::from(frame.area.width) + usize::from(x);
    previous.is_none_or(|previous| {
        previous.area != frame.area || previous.cells[index] != frame.cells[index]
    })
}

fn apply_style(stdout: &mut Stdout, style: Style, color_mode: ColorMode) -> io::Result<()> {
    queue!(stdout, ResetColor, SetAttribute(Attribute::Reset))?;

    match color_mode {
        ColorMode::Ansi16 => {
            if let Some(color) = style.fg {
                queue!(stdout, SetForegroundColor(color.to_crossterm()))?;
            }
            if let Some(color) = style.bg {
                queue!(stdout, SetBackgroundColor(color.to_crossterm()))?;
            }
            if style.modifiers.contains(Modifier::BOLD) {
                queue!(stdout, SetAttribute(Attribute::Bold))?;
            }
            if style.modifiers.contains(Modifier::DIM) {
                queue!(stdout, SetAttribute(Attribute::Dim))?;
            }
        }
        ColorMode::Mono => {
            if style.bg.is_some() {
                queue!(stdout, SetAttribute(Attribute::Reverse))?;
            }
            if style.modifiers.contains(Modifier::BOLD) {
                queue!(stdout, SetAttribute(Attribute::Bold))?;
            }
            if style.modifiers.contains(Modifier::DIM) {
                queue!(stdout, SetAttribute(Attribute::Dim))?;
            }
        }
    }

    Ok(())
}

fn clip_rect(rect: Rect, area: Rect) -> Rect {
    let x = rect.x.min(area.width);
    let y = rect.y.min(area.height);
    let width = rect.width.min(area.width.saturating_sub(x));
    let height = rect.height.min(area.height.saturating_sub(y));
    Rect::new(x, y, width, height)
}

fn sanitize_char(ch: char) -> char {
    match ch {
        c if c.is_ascii() && !c.is_ascii_control() => c,
        '—' | '–' => '-',
        _ => '?',
    }
}

pub(crate) fn sanitize_text(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        match ch {
            '\n' => out.push('\n'),
            '\t' => out.push(' '),
            c if c.is_ascii() && !c.is_ascii_control() => out.push(c),
            '—' | '–' => out.push('-'),
            '→' => out.push_str("->"),
            '←' => out.push_str("<-"),
            '≥' => out.push_str(">="),
            '≤' => out.push_str("<="),
            _ => out.push('?'),
        }
    }
    out
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

pub(crate) fn next_key_event() -> Option<KeyEvent> {
    match tui_or_exit(event::read(), "event::read") {
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
            Some(key)
        }
        _ => None,
    }
}

pub(crate) fn poll_key_event(timeout: std::time::Duration) -> Option<KeyEvent> {
    if !tui_or_exit(event::poll(timeout), "event::poll") {
        return None;
    }
    next_key_event()
}
