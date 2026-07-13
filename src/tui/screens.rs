// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::process::Command;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::tui::render::{
    Block, Color, Frame, Line, Modifier, Paragraph, Rect, Span, Style, Terminal,
    next_key_event, poll_key_event, tui_or_exit,
};

use crate::engine::block;
use crate::engine::firmware::{self, SetupModeStatus};
use crate::engine::pipeline;
use crate::tui::app::{Installer, ScreenAction, Trap};

fn clip_chars(s: &str, width: u16) -> String {
    s.chars().take(width as usize).collect()
}
const LANGUAGES: &[(&str, &str)] = &[
    ("English (US)", "en_US.UTF-8"),
    ("English (UK)", "en_GB.UTF-8"),
    ("Finnish", "fi_FI.UTF-8"),
    ("Swedish", "sv_SE.UTF-8"),
    ("Norwegian", "nb_NO.UTF-8"),
    ("Danish", "da_DK.UTF-8"),
    ("German", "de_DE.UTF-8"),
    ("French", "fr_FR.UTF-8"),
    ("Spanish", "es_ES.UTF-8"),
    ("Italian", "it_IT.UTF-8"),
];

const KEYMAPS: &[(&str, &str)] = &[
    ("en_US.UTF-8", "us"),
    ("en_GB.UTF-8", "uk"),
    ("fi_FI.UTF-8", "fi"),
    ("sv_SE.UTF-8", "se"),
    ("nb_NO.UTF-8", "no"),
    ("da_DK.UTF-8", "dk"),
    ("de_DE.UTF-8", "de"),
    ("fr_FR.UTF-8", "fr"),
    ("es_ES.UTF-8", "es"),
    ("it_IT.UTF-8", "it"),
];

const SHELLS: &[(&str, &str)] = &[("bash", "/bin/bash"), ("zsh", "/bin/zsh")];

const REBOOT_DELAY: u64 = 30;
const MIN_SCREEN_COLS: u16 = 80;
const MIN_SCREEN_ROWS: u16 = 23;

// Install screen vertical layout: rows 0-1 are the title/subtitle (drawn by
// draw_frame), the bottom row is the footer, and the log fills INSTALL_LOG_Y..
// down to it. The status band occupies the rows in between.
const INSTALL_CURRENT_Y: u16 = 2;
const INSTALL_STEP_BAR_Y: u16 = 3;
const INSTALL_OVERALL_Y: u16 = 4;
const INSTALL_LOG_HDR_Y: u16 = 5;
const INSTALL_LOG_Y: u16 = 6;

// 16-color ANSI palette. Indexed colors render identically on the Linux VT
// console and on graphical terminals, so the installer looks the same in a
// plain TTY and in a terminal emulator.
pub(crate) const BG: Color = Color::Indexed(0); // black        screen background
const PANEL: Color = Color::Indexed(8); // bright black  footer / panels
const TITLE_BG: Color = Color::Indexed(4); // blue           title bar
const FG: Color = Color::Indexed(15); // white         default text
pub(crate) const ACCENT: Color = Color::Indexed(6); // cyan   subtitles, rules
const ON_ACCENT: Color = Color::Indexed(0); // black        text on accent
pub(crate) const ERR: Color = Color::Indexed(9); // bright red
pub(crate) const WARN: Color = Color::Indexed(11); // bright yellow
pub(crate) const SELECT: Color = Color::Indexed(13); // bright magenta selection
pub(crate) const ON_SELECT: Color = Color::Indexed(0); // black        text on selection
const OK: Color = Color::Indexed(10); // bright green
pub(crate) const LAND: Color = Color::Indexed(2); // green   map continents

const WELCOME_CHOICES: &[(ScreenAction, &str)] = &[
    (ScreenAction::Install, "Install"),
    (ScreenAction::Shell, "Shell"),
    (ScreenAction::Reboot, "Reboot"),
];

const ENROLL_CHOICES: &[(ScreenAction, &str)] = &[
    (ScreenAction::Back, "Back"),
    (ScreenAction::Install, "Enroll keys"),
];

const FALLBACK_CHOICES: &[(ScreenAction, &str)] = &[
    (ScreenAction::Back, "Back"),
    (ScreenAction::Install, "Continue"),
    (ScreenAction::Reboot, "Reboot"),
];

// Per-step outcome, tracked to weight the overall progress bar (skipped steps
// are excluded from the total). The minimal-counter layout no longer renders a
// per-step marker, so only the variants themselves are needed.
#[derive(Clone, Copy)]
enum InstallStepState {
    Pending,
    Active,
    Done,
    Skipped,
    Failed,
}

pub(crate) fn highlight_style(selected: bool) -> Style {
    if selected {
        Style::default()
            .fg(ON_ACCENT)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

fn button_row(f: &mut Frame, choices: &[&str], selected: usize, y: u16, width: u16) {
    for (i, choice) in choices.iter().enumerate() {
        let label = format!("[ {choice} ]");
        let label_width = label.chars().count() as u16;
        f.render_widget(
            Paragraph::new(label).style(highlight_style(i == selected)),
            Rect::new(4 + i as u16 * width, y, label_width, 1),
        );
    }
}

/// Draw a full-width horizontal divider at `(x, y)` using plain hyphens so
/// it renders identically on the Linux VT console and in terminal emulators.
/// Callers may overpaint a styled label onto the rule to form a titled section
/// header.
fn rule(f: &mut Frame, x: u16, y: u16, width: u16) {
    f.render_widget(
        Paragraph::new("-".repeat(width as usize)).style(Style::default().fg(FG)),
        Rect::new(x, y, width, 1),
    );
}

fn keymap_for_locale(locale: &str) -> &str {
    KEYMAPS
        .iter()
        .find(|(l, _)| *l == locale)
        .map_or("us", |(_, k)| *k)
}


/// Suspend the TUI, run an interactive command on the real terminal, then
/// restore the alternate screen and raw mode.
fn run_on_console(term: &mut Terminal, mut command: Command) {
    tui_or_exit(term.suspend_command(&mut command), "suspend_command");
}

/// Validate POSIX username.
fn valid_username(name: &str) -> bool {
    if name.is_empty() || name.len() > 32 {
        return false;
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() && first != '_' {
        return false;
    }
    for c in chars {
        if !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '_' && c != '-' {
            return false;
        }
    }
    true
}

fn validate_identity(hostname: &str, username: &str, password: &str, password2: &str) -> String {
    if !crate::engine::util::valid_hostname(hostname.trim()) {
        return "Hostname must contain letters, digits or hyphens, without edge hyphens.".into();
    }

    validate_account(username, password, password2)
}

fn validate_account(username: &str, password: &str, password2: &str) -> String {
    let username = username.trim();

    if !valid_username(username) {
        return "Username is alphanumeric and at most 32 chars.".into();
    }
    if password.is_empty() {
        return "Password must not be empty.".into();
    }
    if password != password2 {
        return "Passwords do not match.".into();
    }
    String::new()
}

fn id_field<'a>(
    idx: usize,
    hostname: &'a mut String,
    username: &'a mut String,
    password: &'a mut String,
    password2: &'a mut String,
) -> Option<&'a mut String> {
    match idx {
        1 => Some(hostname),
        2 => Some(username),
        3 => Some(password),
        4 => Some(password2),
        _ => None,
    }
}

fn toggle_choice(selected: &mut usize, key: &KeyEvent) -> bool {
    match key.code {
        KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
            *selected = 1 - *selected;
            true
        }
        _ => false,
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn progress_bar(fraction: f64, width: usize) -> Line {
    let bar_w = (width.saturating_sub(8)).clamp(10, 40);
    let fraction = fraction.clamp(0.0, 1.0);
    let percent = (fraction * 100.0) as usize;

    let filled = (bar_w as f64 * fraction) as usize;
    let (filled, track) = ("#".repeat(filled), "-".repeat(bar_w.saturating_sub(filled)));

    Line::from(vec![
        Span::raw("["),
        Span::styled(filled, Style::default().fg(ACCENT)),
        Span::styled(track, Style::default().fg(FG).add_modifier(Modifier::DIM)),
        Span::raw(format!("] {percent:3}%")),
    ])
}

fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

fn spinner(frame: usize) -> &'static str {
    const FRAMES: [&str; 4] = ["|", "/", "-", "\\"];
    FRAMES[frame % FRAMES.len()]
}

impl Installer {
    pub(crate) fn title(&self) -> String {
        let name = &self.ctx.config.branding.name;
        if self.ctx.image_version.is_empty() {
            format!("{name} Installer")
        } else {
            format!("{name} {} Installer", self.ctx.image_version)
        }
    }

    pub(crate) fn draw_frame(f: &mut Frame, title: &str, subtitle: &str, footer: &str) {
        let area = f.area();

        // Fill the whole screen with the Nord background.
        f.render_widget(Block::default().style(Style::default().fg(FG).bg(BG)), area);

        // Full-width title bar.
        f.render_widget(
            Paragraph::new(format!(" {title}")).style(
                Style::default()
                    .fg(FG)
                    .bg(TITLE_BG)
                    .add_modifier(Modifier::BOLD),
            ),
            Rect::new(0, 0, area.width, 1),
        );

        if !subtitle.is_empty() {
            let sub_area = Rect::new(0, 1, area.width, 1);
            f.render_widget(
                Paragraph::new(format!(" {subtitle}")).style(Style::default().fg(ACCENT)),
                sub_area,
            );
        }

        if !footer.is_empty() {
            let foot_area = Rect::new(0, area.height.saturating_sub(1), area.width, 1);
            f.render_widget(
                Paragraph::new(format!(" {footer}")).style(Style::default().fg(ACCENT).bg(PANEL)),
                foot_area,
            );
        }
    }

    #[allow(
        clippy::unused_self,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss
    )]
    pub(crate) fn screen_too_small(
        &self,
        term: &mut Terminal,
    ) -> bool {
        let size = tui_or_exit(term.size(), "term.size");
        size.height < MIN_SCREEN_ROWS || size.width < MIN_SCREEN_COLS
    }

    pub(crate) fn welcome_screen(
        &mut self,
        term: &mut Terminal,
    ) -> ScreenAction {
        let choices: Vec<&str> = WELCOME_CHOICES.iter().map(|(_, label)| *label).collect();
        let mut selected: usize = 0;

        loop {
            if self.screen_too_small(term) {
                tui_or_exit(term.draw(|f| {
                        Self::draw_frame(
                            f,
                            &self.title(),
                            "",
                            "Esc/Ctrl-C to quit, resize terminal to continue",
                        );
                        let area = f.area();
                        let msg = Paragraph::new(format!(
                            "Current size {}x{}; minimum {}x{}",
                            area.width, area.height, MIN_SCREEN_COLS, MIN_SCREEN_ROWS
                        ))
                        .style(Style::default().fg(WARN));
                        f.render_widget(msg, Rect::new(2, 3, area.width.saturating_sub(4), 3));
                    }),
                    "draw",
                );
                if let Some(key) = next_key_event() {
                    if matches!(key.code, KeyCode::Esc)
                        || (matches!(key.code, KeyCode::Char('c'))
                            && key.modifiers.contains(KeyModifiers::CONTROL))
                    {
                        return ScreenAction::Quit;
                    }
                }
                continue;
            }

            tui_or_exit(term.draw(|f| {
                Self::draw_frame(
                    f,
                    &self.title(),
                    "Choose an option to begin.",
                    "Arrows select, Enter activates, Esc quits",
                );
                let area = f.area();
                let menu_start_y = 4.max((area.height as i16 - choices.len() as i16) / 2) as u16;
                for (i, choice) in choices.iter().enumerate() {
                    let marker = if i == selected { ">" } else { " " };
                    let style = highlight_style(i == selected);
                    f.render_widget(
                        Paragraph::new(format!("{marker} {choice}")).style(style),
                        Rect::new(2, menu_start_y + i as u16, 20, 1),
                    );
                }
            }), "draw");

            if let Some(key) = next_key_event() {
                match key.code {
                    KeyCode::Up => {
                        selected = selected.checked_sub(1).unwrap_or(choices.len() - 1);
                    }
                    KeyCode::Down | KeyCode::Tab => {
                        selected = (selected + 1) % choices.len();
                    }
                    KeyCode::Enter => {
                        if selected < WELCOME_CHOICES.len() {
                            let (action, _) = &WELCOME_CHOICES[selected];
                            return *action;
                        }
                        return ScreenAction::Quit;
                    }
                    KeyCode::Esc => return ScreenAction::Quit,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return ScreenAction::Quit;
                    }
                    _ => {}
                }
            }
        }
    }

    #[allow(clippy::unused_self, clippy::cast_possible_truncation)]
    pub(crate) fn confirm(
        &mut self,
        term: &mut Terminal,
        title: &str,
        body: &str,
        danger: bool,
    ) -> bool {
        let mut selected = 0usize;

        loop {
            tui_or_exit(term.draw(|f| {
                Self::draw_frame(
                    f,
                    title,
                    "",
                    "Left/Right select, Enter activates, Esc cancels",
                );
                let area = f.area();

                let body_style = if danger {
                    Style::default().fg(ERR)
                } else {
                    Style::default()
                };
                let lines: Vec<&str> = body.lines().collect();
                for (i, line) in lines.iter().enumerate() {
                    f.render_widget(
                        Paragraph::new(*line).style(body_style),
                        Rect::new(4, 5 + i as u16, area.width.saturating_sub(8), 1),
                    );
                }

                let y = 8 + lines.len() as u16;
                button_row(f, &["No", "Yes"], selected, y, 10);
            }), "draw");

            if let Some(key) = next_key_event() {
                if !toggle_choice(&mut selected, &key) {
                    match key.code {
                        KeyCode::Enter => return selected == 1,
                        KeyCode::Esc => return false,
                        _ => {}
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
    pub(crate) fn target_screen(
        &mut self,
        term: &mut Terminal,
    ) -> ScreenAction {
        let mut selected: usize = 0;
        let mut top: usize = 0;
        let mut devices: Vec<block::Device> = Vec::new();
        let mut status = String::new();

        let refresh = |devices: &mut Vec<block::Device>,
                       selected: &mut usize,
                       status: &mut String,
                       pre_drive: &str| {
            match block::list_disks() {
                Ok(devs) => {
                    *devices = devs;
                    if !pre_drive.is_empty() {
                        for (i, dev) in devices.iter().enumerate() {
                            if dev.path == pre_drive {
                                *selected = i;
                                break;
                            }
                        }
                    }
                    if !devices.is_empty() {
                        *selected = (*selected).min(devices.len() - 1);
                    }
                    *status = format!("{} disk(s) detected", devices.len());
                }
                Err(e) => {
                    devices.clear();
                    *status = format!("Failed to enumerate devices: {e}");
                }
            }
        };

        refresh(&mut devices, &mut selected, &mut status, &self.ctx.drive);

        loop {
            let size = tui_or_exit(term.size(), "term.size");
            let visible = (size.height.saturating_sub(9)).max(1) as usize;
            top = top.min(selected);
            if selected >= top + visible {
                top = selected - visible + 1;
            }

            let drive_for_refresh = self.ctx.drive.clone();
            tui_or_exit(term.draw(|f| {
                Self::draw_frame(
                    f,
                    "Select Target Drive",
                    "",
                    "Up/Down select, Enter continue, Ctrl-R refresh, Esc back",
                );
                let area = f.area();

                f.render_widget(
                    Paragraph::new(format!(
                        "  {:<19} {:<10} {:<8} {:<20} {}",
                        "Device", "Size", "Bus", "Model", "Status"
                    ))
                    .style(Style::default().add_modifier(Modifier::BOLD)),
                    Rect::new(0, 4, area.width, 1),
                );
                rule(f, 0, 5, area.width);

                for (row, dev) in devices[top..(top + visible).min(devices.len())]
                    .iter()
                    .enumerate()
                {
                    let idx = top + row;
                    let status_text = if dev.busy_reason.is_some() {
                        "BUSY"
                    } else {
                        "available"
                    };
                    let marker = if idx == selected { ">" } else { " " };
                    let style = highlight_style(idx == selected);
                    let model: String = dev.model.chars().take(20).collect();
                    let line = format!(
                        "{marker} {:<19} {:<10} {:<8} {model:<20} {status_text}",
                        dev.path, dev.size, dev.transport
                    );
                    f.render_widget(
                        Paragraph::new(line).style(style),
                        Rect::new(0, 6 + row as u16, area.width, 1),
                    );
                }

                let status_style = if status.contains("Failed") || status.contains("busy") {
                    Style::default().fg(ERR)
                } else {
                    Style::default().fg(ACCENT)
                };
                f.render_widget(
                    Paragraph::new(status.as_str()).style(status_style),
                    Rect::new(
                        2,
                        area.height.saturating_sub(3),
                        area.width.saturating_sub(4),
                        1,
                    ),
                );
            }), "draw");

            if let Some(key) = next_key_event() {
                match key.code {
                    KeyCode::Up if !devices.is_empty() => {
                        selected = selected.checked_sub(1).unwrap_or(devices.len() - 1);
                    }
                    KeyCode::Down | KeyCode::Tab if !devices.is_empty() => {
                        selected = (selected + 1) % devices.len();
                    }
                    KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        refresh(&mut devices, &mut selected, &mut status, &drive_for_refresh);
                    }
                    KeyCode::Esc => return ScreenAction::Back,
                    KeyCode::Enter => {
                        if devices.is_empty() {
                            status = "No target disks detected".into();
                            continue;
                        }
                        let dev = &devices[selected];
                        if let Some(reason) = &dev.busy_reason {
                            status = format!("{} is busy: {reason}", dev.path);
                            continue;
                        }
                        self.ctx.drive.clone_from(&dev.path);
                        self.ctx.drive_label =
                            format!("{} - {}, {}", dev.path, dev.model, dev.size);
                        return ScreenAction::Continue;
                    }
                    _ => {}
                }
            }
        }
    }

    #[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
    pub(crate) fn identity_screen(
        &mut self,
        term: &mut Terminal,
    ) -> ScreenAction {
        let current_locale = if self.ctx.locale.is_empty() {
            self.ctx.config.identity.locale.as_str()
        } else {
            &self.ctx.locale
        };
        let mut language_idx = LANGUAGES
            .iter()
            .position(|(_, l)| *l == current_locale)
            .unwrap_or(0);
        let mut hostname = self.ctx.config.identity.hostname.clone();
        let mut username = self.ctx.username.clone();
        let mut password = self.ctx.user_password.clone();
        let mut password2 = self.ctx.user_password.clone();
        let mut shell_idx: usize = SHELLS
            .iter()
            .position(|(_, v)| *v == self.ctx.user_shell)
            .unwrap_or(0);
        let mut field_idx: usize = 0;
        let mut reveal = false;
        let mut status = String::new();
        let labels = [
            "Language",
            "Hostname",
            "Username",
            "Password",
            "Repeat password",
            "Login shell",
        ];

        loop {
            let (_, locale) = LANGUAGES[language_idx];
            let keymap = keymap_for_locale(locale);
            let fields = [
                format!("{} ({locale}, keymap {keymap})", LANGUAGES[language_idx].0),
                hostname.clone(),
                username.clone(),
                password.clone(),
                password2.clone(),
                SHELLS[shell_idx].0.to_string(),
            ];

            tui_or_exit(term.draw(|f| {
                Self::draw_frame(
                    f,
                    "System Identity",
                    "Choose language, hostname and the first user account.",
                    "Tab next, Left/Right change choices, Enter submit, Esc back",
                );
                let area = f.area();

                for (i, label) in labels.iter().enumerate() {
                    let y = 4 + i as u16 * 2;
                    let selected = i == field_idx;
                    let style = highlight_style(selected);

                    let count = fields[i].chars().count();
                    let value = if (i == 3 || i == 4) && !reveal {
                        "*".repeat(count.min(40))
                    } else if count > 40 {
                        fields[i].chars().skip(count - 40).collect()
                    } else {
                        fields[i].clone()
                    };

                    f.render_widget(
                        Paragraph::new(format!("{label:<16}")),
                        Rect::new(4, y, 16, 1),
                    );
                    f.render_widget(
                        Paragraph::new(format!("[ {value:<40} ]")).style(style),
                        Rect::new(22, y, 44, 1),
                    );
                }

                if matches!(field_idx, 1..=4) {
                    let caret = fields[field_idx].chars().count().min(40) as u16;
                    f.set_cursor_position((24 + caret, 4 + field_idx as u16 * 2));
                }

                if reveal {
                    f.render_widget(
                        Paragraph::new("Password reveal is on")
                            .style(Style::default().fg(WARN).add_modifier(Modifier::BOLD)),
                        Rect::new(4, 17, area.width, 1),
                    );
                }

                if !status.is_empty() {
                    f.render_widget(
                        Paragraph::new(status.as_str()).style(Style::default().fg(ERR)),
                        Rect::new(4, 19, area.width, 1),
                    );
                }
            }), "draw");

            if let Some(key) = next_key_event() {
                match key.code {
                    KeyCode::Esc => return ScreenAction::Back,
                    KeyCode::Tab | KeyCode::Down => {
                        field_idx = (field_idx + 1) % labels.len();
                    }
                    KeyCode::Up => {
                        field_idx = field_idx.checked_sub(1).unwrap_or(labels.len() - 1);
                    }
                    KeyCode::Left if field_idx == 0 => {
                        language_idx = language_idx.checked_sub(1).unwrap_or(LANGUAGES.len() - 1);
                    }
                    KeyCode::Right if field_idx == 0 => {
                        language_idx = (language_idx + 1) % LANGUAGES.len();
                    }
                    KeyCode::Left | KeyCode::Right if field_idx == 5 => {
                        shell_idx = 1 - shell_idx;
                    }
                    KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        reveal = !reveal;
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if let Some(f) = id_field(
                            field_idx,
                            &mut hostname,
                            &mut username,
                            &mut password,
                            &mut password2,
                        ) {
                            f.clear();
                        }
                    }
                    KeyCode::Backspace => {
                        if let Some(f) = id_field(
                            field_idx,
                            &mut hostname,
                            &mut username,
                            &mut password,
                            &mut password2,
                        ) {
                            f.pop();
                        }
                    }
                    KeyCode::Enter => {
                        let result = validate_identity(&hostname, &username, &password, &password2);
                        if result.is_empty() {
                            let (_, locale) = LANGUAGES[language_idx];
                            self.ctx.locale = locale.to_string();
                            self.ctx.keymap = keymap_for_locale(locale).to_string();
                            self.ctx.config.identity.hostname = hostname.trim().to_string();
                            self.ctx.username = username.trim().to_string();
                            self.ctx.user_password.clone_from(&password);
                            self.ctx.user_shell = SHELLS[shell_idx].1.to_string();
                            return ScreenAction::Continue;
                        }
                        if field_idx < labels.len() - 1 {
                            field_idx += 1;
                        } else {
                            status = result;
                        }
                    }
                    KeyCode::Char(c) if c.is_ascii() && c >= ' ' => {
                        if let Some(f) = id_field(
                            field_idx,
                            &mut hostname,
                            &mut username,
                            &mut password,
                            &mut password2,
                        ) {
                            let max = match field_idx {
                                1 => 253,
                                2 => 32,
                                _ => usize::MAX,
                            };
                            if f.len() < max {
                                f.push(c);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    pub(crate) fn secureboot_screen(
        &mut self,
        term: &mut Terminal,
    ) -> ScreenAction {
        let status = firmware::detect_setup_mode();
        if status.setup_mode == Some(true) {
            self.secureboot_enroll_screen(term, &status)
        } else {
            self.secureboot_fallback_screen(term, &status)
        }
    }

    pub(crate) fn secureboot_enroll_screen(
        &mut self,
        term: &mut Terminal,
        status: &SetupModeStatus,
    ) -> ScreenAction {
        let mut selected = 1usize;
        let state = if status.secure_boot == Some(true) {
            "enabled"
        } else {
            "disabled"
        };

        loop {
            tui_or_exit(term.draw(|f| {
                Self::draw_frame(
                    f,
                    "Secure Boot",
                    &format!(
                        "{} can enroll its own Secure Boot keys when firmware Setup Mode is on.",
                        self.ctx.config.branding.name
                    ),
                    "Left/Right select, Enter activates, Esc back",
                );
                let area = f.area();
                f.render_widget(
                    Paragraph::new(format!(
                        "Setup Mode is on. Secure Boot is currently {state}."
                    )),
                    Rect::new(4, 5, area.width.saturating_sub(8), 1),
                );
                f.render_widget(
                    Paragraph::new("This writes firmware Secure Boot variables.")
                        .style(Style::default().fg(WARN).add_modifier(Modifier::BOLD)),
                    Rect::new(4, 7, area.width.saturating_sub(8), 1),
                );
                button_row(f, &["Back", "Enroll keys"], selected, 10, 16);
            }), "draw");

            if let Some(key) = next_key_event() {
                if !toggle_choice(&mut selected, &key) {
                    match key.code {
                        KeyCode::Esc => return ScreenAction::Back,
                        KeyCode::Enter => {
                            let (action, _) = &ENROLL_CHOICES[selected];
                            if *action == ScreenAction::Install {
                                self.ctx.secure_boot = true;
                            }
                            return *action;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    pub(crate) fn secureboot_fallback_screen(
        &mut self,
        term: &mut Terminal,
        status: &SetupModeStatus,
    ) -> ScreenAction {
        let mut selected = 1usize;
        let (title, body) = if status.setup_mode == Some(false) {
            (
                "Setup Mode is off",
                "Secure Boot keys can only be enrolled while firmware Setup Mode is on.\n\
                 Reboot to firmware setup, or continue without Secure Boot.",
            )
        } else {
            (
                "Setup Mode not detected",
                "The installer could not read firmware Setup Mode.\n\
                 Continue without Secure Boot, or reboot and check firmware settings.",
            )
        };
        let body = if status.reason.is_empty() {
            body.to_string()
        } else {
            format!("{body}\n{}", status.reason)
        };

        loop {
            tui_or_exit(term.draw(|f| {
                Self::draw_frame(
                    f,
                    "Secure Boot",
                    title,
                    "Left/Right select, Enter activates, Esc back",
                );
                let area = f.area();
                let lines: Vec<&str> = body.lines().collect();
                for (i, line) in lines.iter().enumerate() {
                    f.render_widget(
                        Paragraph::new(*line)
                            .style(Style::default().fg(WARN).add_modifier(Modifier::BOLD)),
                        Rect::new(4, 5 + i as u16, area.width.saturating_sub(8), 1),
                    );
                }
                let y = 8 + lines.len() as u16;
                let choices: Vec<&str> = FALLBACK_CHOICES.iter().map(|(_, label)| *label).collect();
                button_row(f, &choices, selected, y, 14);
            }), "draw");

            if let Some(key) = next_key_event() {
                match key.code {
                    KeyCode::Esc => return ScreenAction::Back,
                    KeyCode::Left => {
                        selected = selected
                            .checked_sub(1)
                            .unwrap_or(FALLBACK_CHOICES.len() - 1);
                    }
                    KeyCode::Right | KeyCode::Tab => {
                        selected = (selected + 1) % FALLBACK_CHOICES.len();
                    }
                    KeyCode::Enter => {
                        let (action, _) = &FALLBACK_CHOICES[selected];
                        if *action == ScreenAction::Install {
                            self.ctx.secure_boot = false;
                        }
                        return *action;
                    }
                    _ => {}
                }
            }
        }
    }

    pub(crate) fn review_screen(
        &mut self,
        term: &mut Terminal,
    ) -> ScreenAction {
        let mut selected = 1usize;

        loop {
            let secure_boot = if self.ctx.secure_boot {
                "Enroll keys"
            } else {
                "Disabled"
            };
            let image = if self.ctx.image_version.is_empty() {
                self.ctx.target_imgref.clone()
            } else {
                format!("{} ({})", self.ctx.target_imgref, self.ctx.image_version)
            };
            let k3s = if self.ctx.config.k3s.server_url.is_empty() {
                self.ctx.config.k3s.role.clone()
            } else {
                format!(
                    "{} ({})",
                    self.ctx.config.k3s.role, self.ctx.config.k3s.server_url
                )
            };
            let rows = [
                ("Target disk", self.ctx.drive_label.as_str()),
                ("Image", image.as_str()),
                ("Hostname", self.ctx.config.identity.hostname.as_str()),
                ("Timezone", self.ctx.config.identity.timezone.as_str()),
                ("Language", self.ctx.locale.as_str()),
                ("Keymap", self.ctx.keymap.as_str()),
                ("User", self.ctx.username.as_str()),
                ("Shell", self.ctx.user_shell.as_str()),
                ("k3s", k3s.as_str()),
                ("Secure Boot", secure_boot),
            ];

            tui_or_exit(term.draw(|f| {
                Self::draw_frame(
                    f,
                    "Does this look right?",
                    "Review the installation choices before the disk is modified.",
                    "Left/Right select, Enter activates, Esc back",
                );
                let area = f.area();

                for (i, (label, value)) in rows.iter().enumerate() {
                    let y = 4 + i as u16;
                    f.render_widget(
                        Paragraph::new(format!("{label:<13} {value}")),
                        Rect::new(4, y, area.width.saturating_sub(8), 1),
                    );
                }

                f.render_widget(
                    Paragraph::new("Installation will erase the selected target disk.")
                        .style(Style::default().fg(WARN).add_modifier(Modifier::BOLD)),
                    Rect::new(4, 16, area.width.saturating_sub(8), 1),
                );

                button_row(f, &["Back", "Start install"], selected, 19, 18);
            }), "draw");

            if let Some(key) = next_key_event() {
                if !toggle_choice(&mut selected, &key) {
                    match key.code {
                        KeyCode::Enter if selected == 1 => return ScreenAction::Install,
                        KeyCode::Esc | KeyCode::Enter => return ScreenAction::Back,
                        _ => {}
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
    pub(crate) fn install_screen(
        &mut self,
        term: &mut Terminal,
    ) -> ScreenAction {
        let steps = pipeline::STEPS;
        let mut states = vec![InstallStepState::Pending; steps.len()];

        let mut worker = crate::tui::app::InstallTask::start(&self.ctx);

        let mut logs: Vec<String> = Vec::new();
        let mut state = "Starting install".to_string();
        let mut step_fraction = 0.0f64;
        let mut current_step = 0;
        let mut log_scroll: usize = 0;
        let mut running = true;
        let mut committed = false;
        let mut failed = false;
        let mut complete = false;
        let started = Instant::now();
        let mut finished: Option<Instant> = None;

        let install_title = if self.ctx.image_version.is_empty() {
            format!("Installing {}", self.ctx.config.branding.name)
        } else {
            format!(
                "Installing {} {}",
                self.ctx.config.branding.name, self.ctx.image_version
            )
        };

        loop {
            while let Ok(evt) = worker.events_rx.try_recv() {
                match evt {
                    crate::tui::app::TaskEvent::Log(line) => {
                        logs.push(line);
                        if log_scroll > 0 {
                            log_scroll += 1;
                        }
                        if logs.len() > 2000 {
                            logs.drain(0..logs.len() - 2000);
                        }
                        log_scroll = log_scroll.min(logs.len().saturating_sub(1));
                    }
                    crate::tui::app::TaskEvent::Progress {
                        step_index,
                        fraction,
                    } => {
                        current_step = step_index;
                        if steps[step_index].point_of_no_return {
                            committed = true;
                        }
                        state = steps[step_index].title.to_string();
                        step_fraction = fraction;
                        if step_index < states.len() {
                            states[step_index] = if fraction >= 1.0 {
                                InstallStepState::Done
                            } else {
                                InstallStepState::Active
                            };
                        }
                    }
                    crate::tui::app::TaskEvent::Skipped { step_index } => {
                        if step_index < states.len() {
                            states[step_index] = InstallStepState::Skipped;
                        }
                    }
                    crate::tui::app::TaskEvent::Done => {
                        running = false;
                        complete = true;
                        finished.get_or_insert_with(Instant::now);
                        state = "Install complete.".into();
                    }
                    crate::tui::app::TaskEvent::Failed { at, error } => {
                        let err_msg = format!("{error:#}");
                        running = false;
                        failed = true;
                        finished.get_or_insert_with(Instant::now);
                        if at < states.len() {
                            states[at] = InstallStepState::Failed;
                        }
                        state = format!("Install failed at step {}: {err_msg}", at + 1);
                        self.ctx.last_error = err_msg;
                    }
                }
            }

            let frame = (started.elapsed().as_millis() / 80) as usize;

            tui_or_exit(term.draw(|f| {
                Self::draw_frame(
                    f,
                    &install_title,
                    &format!("Target: {}", self.ctx.drive_label),
                    if complete {
                        "Enter continue"
                    } else if failed {
                        "Enter back to menu"
                    } else if committed {
                        "PgUp/PgDn scroll"
                    } else {
                        "Esc cancel, PgUp/PgDn scroll"
                    },
                );
                let area = f.area();
                let width = area.width;
                let content_w = width.saturating_sub(2);
                let bold = Style::default().add_modifier(Modifier::BOLD);
                let sep = " - ";

                // Current step (row 2): spinner + counter + title, or the final
                // status message once the run has stopped.
                let st_style = if failed {
                    Style::default().fg(ERR)
                } else if complete {
                    Style::default().fg(OK).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let current_line = if running {
                    format!(
                        "{} Step {}/{}{sep}{state}",
                        spinner(frame),
                        current_step + 1,
                        steps.len()
                    )
                } else {
                    state.clone()
                };
                f.render_widget(
                    Paragraph::new(clip_chars(&current_line, content_w)).style(st_style),
                    Rect::new(2, INSTALL_CURRENT_Y, content_w, 1),
                );

                // Progress bars: an 8-wide label column, then a wide bar.
                let bar_x = 10u16;
                f.render_widget(
                    Paragraph::new("Step").style(bold),
                    Rect::new(2, INSTALL_STEP_BAR_Y, 8, 1),
                );
                let step_bar_w = width.saturating_sub(bar_x + 2).max(20);
                f.render_widget(
                    Paragraph::new(progress_bar(step_fraction, step_bar_w as usize)),
                    Rect::new(bar_x, INSTALL_STEP_BAR_Y, step_bar_w, 1),
                );

                // Overall bar (row 4) with elapsed time pinned to the right.
                let runnable_weight = |range: std::ops::Range<usize>| -> u32 {
                    range
                        .filter(|&i| !matches!(states[i], InstallStepState::Skipped))
                        .map(|i| steps[i].weight)
                        .sum()
                };
                let total = runnable_weight(0..steps.len()).max(1);
                let done = runnable_weight(0..current_step);
                let active = f64::from(steps[current_step].weight) * step_fraction.min(1.0);
                let overall = (f64::from(done) + active) / f64::from(total);

                let elapsed = finished
                    .unwrap_or_else(Instant::now)
                    .duration_since(started);
                let elapsed_str = format!("Elapsed {}", format_elapsed(elapsed));
                let elapsed_w = elapsed_str.chars().count() as u16;

                f.render_widget(
                    Paragraph::new("Overall").style(bold),
                    Rect::new(2, INSTALL_OVERALL_Y, 8, 1),
                );
                let overall_bar_w = width.saturating_sub(bar_x + elapsed_w + 2).max(20);
                f.render_widget(
                    Paragraph::new(progress_bar(overall, overall_bar_w as usize)),
                    Rect::new(bar_x, INSTALL_OVERALL_Y, overall_bar_w, 1),
                );
                f.render_widget(
                    Paragraph::new(elapsed_str).style(Style::default().fg(ACCENT)),
                    Rect::new(
                        width.saturating_sub(elapsed_w + 1),
                        INSTALL_OVERALL_Y,
                        elapsed_w,
                        1,
                    ),
                );

                // Log header (row 5): a full-width rule with an inline label.
                rule(f, 0, INSTALL_LOG_HDR_Y, width);
                let (log_label, log_style) = if log_scroll > 0 {
                    (
                        format!(" Log  ^{log_scroll}  PgDn to follow "),
                        Style::default().fg(WARN).add_modifier(Modifier::BOLD),
                    )
                } else {
                    (" Log ".to_string(), bold)
                };
                f.render_widget(
                    Paragraph::new(clip_chars(&log_label, content_w)).style(log_style),
                    Rect::new(2, INSTALL_LOG_HDR_Y, content_w, 1),
                );

                // Log pane (rows INSTALL_LOG_Y..): full terminal width.
                let log_h = area.height.saturating_sub(INSTALL_LOG_Y + 1).max(1) as usize;
                f.render_widget(
                    Block::default().style(Style::default().fg(FG).bg(BG)),
                    Rect::new(0, INSTALL_LOG_Y, width, log_h as u16),
                );
                let log_w = width.saturating_sub(1);
                let end = logs.len().saturating_sub(log_scroll);
                let start = end.saturating_sub(log_h);
                for (row, line) in logs[start..end].iter().enumerate() {
                    f.render_widget(
                        Paragraph::new(clip_chars(line, log_w)),
                        Rect::new(1, INSTALL_LOG_Y + row as u16, log_w, 1),
                    );
                }
            }), "draw");

            if let Some(key) = poll_key_event(Duration::from_millis(100)) {
                match key.code {
                    KeyCode::PageDown => {
                        log_scroll = log_scroll.saturating_sub(10);
                    }
                    KeyCode::PageUp => {
                        log_scroll = (log_scroll + 10).min(logs.len().saturating_sub(1));
                    }
                    KeyCode::Esc
                        if running
                            && !committed
                            && self.confirm(
                                term,
                                "Cancel install?",
                                "The installer will stop and clean up mounts.",
                                false,
                            ) =>
                    {
                        state = "Cancelling install...".into();
                        worker.cancel();
                    }
                    KeyCode::Char('c')
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            && running
                            && !committed
                            && self.confirm(
                                term,
                                "Cancel install?",
                                "The installer will stop and clean up mounts.",
                                false,
                            ) =>
                    {
                        state = "Cancelling install...".into();
                        worker.cancel();
                    }
                    KeyCode::Enter if complete => {
                        worker.join();
                        return ScreenAction::Reboot;
                    }
                    KeyCode::Enter if failed => {
                        worker.join();
                        return ScreenAction::Menu;
                    }
                    _ => {}
                }
            }
        }
    }

    pub(crate) fn reboot_screen(
        &mut self,
        term: &mut Terminal,
    ) -> Trap {
        let mut selected = 1usize;
        #[allow(clippy::cast_possible_wrap)]
        let mut remaining = REBOOT_DELAY as i64;
        let mut last_tick = Instant::now();
        let mut countdown = true;

        loop {
            if countdown {
                let now = Instant::now();
                if now.duration_since(last_tick) >= Duration::from_secs(1) {
                    remaining -= 1;
                    last_tick = now;
                }
                if remaining <= 0 {
                    return Trap::Reboot;
                }
            }

            tui_or_exit(term.draw(|f| {
                Self::draw_frame(
                    f,
                    "Install Complete",
                    &format!("{} is ready to boot.", self.ctx.config.branding.name),
                    "Left/Right select, Enter activates",
                );
                let area = f.area();
                f.render_widget(
                    Paragraph::new(format!("Target: {}", self.ctx.drive_label)),
                    Rect::new(4, 5, area.width.saturating_sub(8), 1),
                );
                f.render_widget(
                    Paragraph::new("Remove the installation media before rebooting.")
                        .style(Style::default().fg(ACCENT)),
                    Rect::new(4, 7, area.width.saturating_sub(8), 1),
                );
                let countdown_line = if countdown {
                    format!("Auto-reboot in {remaining}s")
                } else {
                    "Auto-reboot cancelled; select an option.".to_string()
                };
                f.render_widget(
                    Paragraph::new(countdown_line),
                    Rect::new(4, 9, area.width.saturating_sub(8), 1),
                );
                button_row(f, &["Shell", "Reboot"], selected, 12, 14);
            }), "draw");

            if let Some(key) = poll_key_event(Duration::from_millis(200)) {
                countdown = false;
                if !toggle_choice(&mut selected, &key) && key.code == KeyCode::Enter {
                    if selected == 0 {
                        let shell = crate::engine::util::default_shell();
                        run_on_console(term, std::process::Command::new(&shell));
                    } else {
                        return Trap::Reboot;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{toggle_choice, validate_account, valid_username};
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    #[test]
    fn valid_username_accepts_installer_format() {
        assert!(valid_username("user_01"));
        assert!(!valid_username("User"));
        assert!(!valid_username("bad name"));
    }

    #[test]
    fn validate_account_catches_mismatch() {
        assert_eq!(validate_account("user", "pw1", "pw2"), "Passwords do not match.");
    }

    #[test]
    fn toggle_choice_switches_selection() {
        let mut selected = 0;
        let key = KeyEvent {
            code: KeyCode::Tab,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert!(toggle_choice(&mut selected, &key));
        assert_eq!(selected, 1);
    }
}
