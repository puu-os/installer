// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! Single-loop TUI runtime and pure state transitions.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::engine::{block, firmware, pipeline};
use crate::tui::app::{InstallTask, Installer, Trap};
use crate::tui::engine::{
    Align, Direction, Edges, FrameScheduler, LayoutStyle, Length, NodeId, Schedule, ViewNode,
};
use crate::tui::message::{DriveInfo, Effect, FieldId, FormId, Message, ToggleId};
use crate::tui::render::{
    Block, Color, CursorStyle, Frame, Input, Line, Modifier, Paragraph, Rect, Span, Style,
    Terminal, screen_size, tui_or_exit,
};
use crate::tui::state::{AppState, IdentityDraft, InstallStepState, Modal, Route};
use crate::tui::timezone;

const MAX_EVENTS_PER_TURN: usize = 64;
const MAX_LOG_LINES: usize = 2000;
const REBOOT_DELAY_SECONDS: u16 = 30;
const TICK_INTERVAL: Duration = Duration::from_millis(100);
const MIN_SCREEN_COLS: u16 = 80;
const MIN_SCREEN_ROWS: u16 = 23;

const fn screen_too_small(width: u16, height: u16) -> bool {
    width < MIN_SCREEN_COLS || height < MIN_SCREEN_ROWS
}

fn cycle(index: &mut usize, count: usize, backwards: bool) {
    if count == 0 {
        *index = 0;
    } else if backwards {
        *index = index.checked_sub(1).unwrap_or(count - 1);
    } else {
        *index = (*index + 1) % count;
    }
}

fn selected_drive(state: &AppState) -> Option<&DriveInfo> {
    state
        .selection
        .target
        .and_then(|selected| state.drives.get(selected))
}

fn update_modal(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    let Some(modal) = state.active_modal() else {
        return Vec::new();
    };

    match key.code {
        KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
            state.selection.modal = 1 - state.selection.modal.min(1);
        }
        KeyCode::Esc => {
            state.pop_modal();
            state.selection.modal = 0;
        }
        KeyCode::Enter => {
            let confirmed = state.selection.modal == 1;
            state.pop_modal();
            state.selection.modal = 0;
            match (modal, confirmed) {
                (Modal::WipeConfirmation, true) => state.route = Route::Identity,
                (Modal::RebootConfirmation, true) => return vec![Effect::Reboot],
                (Modal::Cancellation, true) => return vec![Effect::CancelPipeline],
                _ => {}
            }
        }
        _ => {}
    }
    Vec::new()
}

fn update_identity_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    let Some(draft) = state.identity.as_mut() else {
        return Vec::new();
    };

    match key.code {
        KeyCode::Esc => state.route = Route::Target,
        KeyCode::Tab | KeyCode::Down => {
            draft.field_index = (draft.field_index + 1) % 6;
        }
        KeyCode::Up => cycle(&mut draft.field_index, 6, true),
        KeyCode::Left if draft.field_index == 0 => {
            cycle(&mut draft.language_index, LANGUAGES.len(), true);
        }
        KeyCode::Right if draft.field_index == 0 => {
            cycle(&mut draft.language_index, LANGUAGES.len(), false);
        }
        KeyCode::Left | KeyCode::Right if draft.field_index == 5 => {
            draft.shell_index = 1 - draft.shell_index.min(1);
        }
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            draft.reveal_password = !draft.reveal_password;
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(field) = identity_field_mut(draft) {
                field.clear();
            }
        }
        KeyCode::Backspace => {
            if let Some(field) = identity_field_mut(draft) {
                field.pop();
            }
        }
        KeyCode::Char(ch)
            if ch.is_ascii_graphic() && !key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            let field_index = draft.field_index;
            if let Some(field) = identity_field_mut(draft) {
                let max = match field_index {
                    1 => 253,
                    2 => 32,
                    _ => usize::MAX,
                };
                if field.len() < max {
                    field.push(ch);
                }
            }
        }
        KeyCode::Enter => {
            return vec![Effect::ValidateForm {
                form: FormId::Identity,
            }];
        }
        _ => {}
    }
    Vec::new()
}

fn identity_field_mut(draft: &mut IdentityDraft) -> Option<&mut String> {
    match draft.field_index {
        1 => Some(&mut draft.hostname),
        2 => Some(&mut draft.username),
        3 => Some(&mut draft.password),
        4 => Some(&mut draft.repeated_password),
        _ => None,
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "per-route input dispatch; arms extract once the migration settles"
)]
fn update_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    if state.active_modal().is_some() {
        return update_modal(state, key);
    }

    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if state.route == Route::Install && !state.install.committed && !state.install.complete {
            state.push_modal(Modal::Cancellation);
            return Vec::new();
        }
        return vec![Effect::Quit];
    }

    match state.route {
        Route::Welcome => match key.code {
            KeyCode::Up => cycle(&mut state.selection.welcome, 3, true),
            KeyCode::Down | KeyCode::Tab => cycle(&mut state.selection.welcome, 3, false),
            KeyCode::Esc => return vec![Effect::Quit],
            KeyCode::Enter => match state.selection.welcome {
                0 => {
                    state.reset_flow();
                    return vec![Effect::RefreshDrives];
                }
                1 => {
                    return vec![Effect::RunShell {
                        command: Vec::new(),
                    }];
                }
                _ => state.push_modal(Modal::RebootConfirmation),
            },
            _ => {}
        },
        Route::Target => match key.code {
            KeyCode::Esc => state.route = Route::Welcome,
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return vec![Effect::RefreshDrives];
            }
            KeyCode::Up => {
                let mut selected = state.selection.target.unwrap_or(0);
                cycle(&mut selected, state.drives.len(), true);
                state.selection.target = (!state.drives.is_empty()).then_some(selected);
            }
            KeyCode::Down | KeyCode::Tab => {
                let mut selected = state.selection.target.unwrap_or(0);
                cycle(&mut selected, state.drives.len(), false);
                state.selection.target = (!state.drives.is_empty()).then_some(selected);
            }
            KeyCode::Enter => {
                if let Some(drive) = selected_drive(state)
                    && drive.busy_reasons.is_empty()
                {
                    return vec![Effect::SelectDrive { index: drive.index }];
                }
            }
            _ => {}
        },
        Route::Identity => return update_identity_key(state, key),
        Route::Timezone => return timezone::update_key(state, key),
        Route::SecureBoot => {
            let setup = state.firmware.as_ref().and_then(|s| s.setup_mode);
            let count = if setup == Some(true) { 2 } else { 3 };
            match key.code {
                KeyCode::Esc => state.route = Route::Timezone,
                KeyCode::Left => cycle(&mut state.selection.secure_boot, count, true),
                KeyCode::Right | KeyCode::Tab => {
                    cycle(&mut state.selection.secure_boot, count, false);
                }
                KeyCode::Enter => {
                    let sel = state.selection.secure_boot;
                    match (setup, sel) {
                        (_, 0) => state.route = Route::Timezone,
                        (Some(true), 1) => {
                            state.route = Route::Review;
                            return vec![Effect::SetSecureBoot(true)];
                        }
                        (_, 1) => {
                            state.route = Route::Review;
                            return vec![Effect::SetSecureBoot(false)];
                        }
                        (_, 2) => return vec![Effect::Reboot],
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        Route::Review => match key.code {
            KeyCode::Esc => state.route = Route::SecureBoot,
            KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                state.selection.review = 1 - state.selection.review.min(1);
            }
            KeyCode::Enter if state.selection.review == 0 => state.route = Route::SecureBoot,
            KeyCode::Enter => {
                state.route = Route::Install;
                state.install.step_states = vec![InstallStepState::Pending; pipeline::STEPS.len()];
                return vec![Effect::StartInstall];
            }
            _ => {}
        },
        Route::Install => match key.code {
            KeyCode::Esc if !state.install.committed && !state.install.complete => {
                state.push_modal(Modal::Cancellation);
            }
            KeyCode::PageUp => {
                state.scroll.install_log =
                    (state.scroll.install_log + 10).min(state.install.logs.len().saturating_sub(1));
                state.install.follow_logs = false;
            }
            KeyCode::PageDown => {
                state.scroll.install_log = state.scroll.install_log.saturating_sub(10);
                state.install.follow_logs = state.scroll.install_log == 0;
            }
            KeyCode::Enter if state.install.complete => {
                state.route = Route::Reboot;
                state.reboot.remaining_seconds = Some(REBOOT_DELAY_SECONDS);
                state.reboot.automatic = true;
            }
            KeyCode::Enter if state.install.failed => state.route = Route::Welcome,
            _ => {}
        },
        Route::Reboot => {
            state.reboot.automatic = false;
            match key.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                    state.selection.reboot = 1 - state.selection.reboot.min(1);
                }
                KeyCode::Enter if state.selection.reboot == 0 => {
                    state.reboot.shell_active = true;
                    return vec![Effect::RunShell {
                        command: Vec::new(),
                    }];
                }
                KeyCode::Enter => return vec![Effect::Reboot],
                _ => {}
            }
        }
    }
    Vec::new()
}

/// Pure transition from one application state to the next.
#[allow(
    clippy::too_many_lines,
    reason = "per-message dispatch; arms extract once the migration settles"
)]
pub(crate) fn update(state: &mut AppState, message: Message) -> Vec<Effect> {
    match message {
        Message::Key(key) => return update_key(state, key),
        Message::Resized { .. } | Message::Tick | Message::Paste(_) => {}
        Message::CountdownTick => {
            if state.route == Route::Reboot && state.reboot.automatic {
                let remaining = state
                    .reboot
                    .remaining_seconds
                    .unwrap_or(REBOOT_DELAY_SECONDS);
                state.reboot.remaining_seconds = Some(remaining.saturating_sub(1));
                if remaining <= 1 {
                    return vec![Effect::Reboot];
                }
            }
        }
        Message::DrivesRefreshed(drives) => {
            state.drives = drives;
            state.selection.target = (!state.drives.is_empty()).then_some(0);
        }
        Message::DriveSelected { index } => {
            if state.drives.iter().any(|drive| drive.index == index) {
                state.push_modal(Modal::WipeConfirmation);
            }
        }
        Message::FieldEdit { field, value, .. } => {
            if let Some(draft) = state.identity.as_mut() {
                match field {
                    FieldId::Hostname => draft.hostname = value,
                    FieldId::Username => draft.username = value,
                    FieldId::Password => draft.password = value,
                    FieldId::RepeatedPassword => draft.repeated_password = value,
                    _ => {}
                }
            }
        }
        Message::FieldFocused { field, .. } => {
            state.focused = Some(NodeId::new(format!("{field:?}")));
        }
        Message::FormValidated { form, error } => {
            if form == FormId::Identity {
                state.validation_error = error;
                if state.validation_error.is_none() {
                    state.route = Route::Timezone;
                }
            }
        }
        Message::Toggle { id } => match id {
            ToggleId::RevealPassword => {
                if let Some(draft) = state.identity.as_mut() {
                    draft.reveal_password = !draft.reveal_password;
                }
            }
            ToggleId::FollowLogs => state.install.follow_logs = !state.install.follow_logs,
            _ => {}
        },
        Message::Submit => {
            return update_key(state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }
        Message::Back => return update_key(state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        Message::CancelInstall => state.push_modal(Modal::Cancellation),
        Message::Log(line) => {
            state.install.logs.push(line);
            if state.install.logs.len() > MAX_LOG_LINES {
                state.install.logs.remove(0);
            }
            if state.scroll.install_log > 0 {
                state.scroll.install_log += 1;
            }
        }
        Message::InstallProgress {
            step_index,
            fraction_pct,
        } => {
            if step_index < pipeline::STEPS.len() {
                state.install.current_step = Some(step_index);
                state.install.step_fraction = fraction_pct.min(100);
                state.install.committed |= pipeline::STEPS[step_index].point_of_no_return;
                if state.install.step_states.len() != pipeline::STEPS.len() {
                    state.install.step_states =
                        vec![InstallStepState::Pending; pipeline::STEPS.len()];
                }
                state.install.step_states[step_index] = if fraction_pct >= 100 {
                    InstallStepState::Done
                } else {
                    InstallStepState::Active
                };
            }
        }
        Message::InstallSkipped { step_index } => {
            if let Some(step) = state.install.step_states.get_mut(step_index) {
                *step = InstallStepState::Skipped;
            }
        }
        Message::InstallFinished { at, elapsed, error } => {
            state.install.elapsed_seconds = elapsed.map(|duration| duration.as_secs());
            state.install.failed = error.is_some();
            state.install.complete = error.is_none();
            state.install.failure = error;
            if let Some(step) = at.and_then(|index| state.install.step_states.get_mut(index)) {
                *step = InstallStepState::Failed;
            }
            if state.install.failed {
                state.push_modal(Modal::Failure);
            }
        }
        Message::ShellReturned { status } => {
            state.reboot.shell_active = false;
            if let Err(error) = status {
                state.validation_error = Some(error);
            }
        }
        Message::FirmwareReady { status } => {
            state.selection.secure_boot = 1;
            state.firmware = Some(status);
        }
        Message::EnrollFinished { error } => {
            state.validation_error = error;
            if state.validation_error.is_none() {
                state.route = Route::Review;
            }
        }
        Message::Reboot => return vec![Effect::Reboot],
        Message::Quit => return vec![Effect::Quit],
    }
    Vec::new()
}

fn validate_identity(draft: &IdentityDraft) -> Option<String> {
    let hostname = draft.hostname.trim();
    if !crate::engine::util::valid_hostname(hostname) {
        return Some("Invalid hostname".to_string());
    }
    let username = draft.username.trim();
    let mut chars = username.chars();
    let Some(first) = chars.next() else {
        return Some("Username must not be empty".to_string());
    };
    if (!first.is_ascii_lowercase() && first != '_')
        || chars
            .any(|ch| !ch.is_ascii_lowercase() && !ch.is_ascii_digit() && ch != '_' && ch != '-')
    {
        return Some("Invalid username".to_string());
    }
    if draft.password.is_empty() {
        return Some("Password must not be empty".to_string());
    }
    if draft.password != draft.repeated_password {
        return Some("Passwords do not match".to_string());
    }
    None
}

fn drive_messages() -> Vec<DriveInfo> {
    block::list_disks().map_or_else(
        |_| Vec::new(),
        |devices| {
            devices
                .into_iter()
                .enumerate()
                .map(|(index, device)| DriveInfo {
                    index,
                    path: device.path.clone(),
                    label: format!("{} - {}, {}", device.path, device.model, device.size),
                    size: device.size,
                    model: device.model.clone(),
                    transport: device.transport.clone(),
                    busy_reasons: device
                        .busy_reason
                        .map(|reason| vec![reason.to_string()])
                        .unwrap_or_default(),
                    selected: false,
                })
                .collect()
        },
    )
}

fn execute_effect(
    installer: &mut Installer,
    task: &mut Option<InstallTask>,
    effect: Effect,
) -> (Option<Message>, Option<Trap>) {
    match effect {
        Effect::RefreshDrives => (Some(Message::DrivesRefreshed(drive_messages())), None),
        Effect::DetectFirmware => {
            let status = firmware::detect_setup_mode();
            (
                Some(Message::FirmwareReady {
                    status: crate::tui::message::FirmwareStatus {
                        setup_mode: status.setup_mode,
                        secure_boot: status.secure_boot,
                        reason: status.reason,
                    },
                }),
                None,
            )
        }
        Effect::SelectDrive { index } => {
            if let Some(drive) = installer
                .state
                .drives
                .iter()
                .find(|drive| drive.index == index)
            {
                installer.ctx.drive.clone_from(&drive.path);
                installer.ctx.drive_label.clone_from(&drive.label);
            }
            (Some(Message::DriveSelected { index }), None)
        }
        Effect::SelectTimezone { name } => {
            installer.ctx.config.identity.timezone = name;
            (None, None)
        }
        Effect::SetSecureBoot(enable) => {
            installer.ctx.secure_boot = enable;
            (None, None)
        }
        Effect::ValidateForm {
            form: FormId::Identity,
        } => {
            let error = installer
                .state
                .identity
                .as_ref()
                .and_then(validate_identity);
            if error.is_none()
                && let Some(draft) = installer.state.identity.as_ref()
            {
                installer.ctx.config.identity.hostname = draft.hostname.trim().to_string();
                installer.ctx.username = draft.username.trim().to_string();
                installer.ctx.user_password.clone_from(&draft.password);
                installer.ctx.user_shell = if draft.shell_index == 0 {
                    "/bin/bash".to_string()
                } else {
                    "/bin/zsh".to_string()
                };
            }
            (
                Some(Message::FormValidated {
                    form: FormId::Identity,
                    error,
                }),
                None,
            )
        }
        Effect::ValidateForm { form } => (Some(Message::FormValidated { form, error: None }), None),
        Effect::StartInstall => {
            installer.state.install.started = Some(Instant::now());
            *task = Some(InstallTask::start(&installer.ctx));
            (None, None)
        }
        Effect::CancelPipeline => {
            if let Some(task) = task.as_ref() {
                task.cancel();
            }
            (None, None)
        }
        Effect::RunShell { .. } => (None, Some(Trap::Shell)),
        Effect::EnrollKeys | Effect::Log(_) | Effect::None => (None, None),
        Effect::Reboot => (None, Some(Trap::Reboot)),
        Effect::Quit => (None, Some(Trap::Quit)),
    }
}

fn selected_style(selected: bool) -> Style {
    if selected {
        Style::default()
            .fg(Color::Indexed(0))
            .bg(Color::Indexed(6))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

fn render_lines(frame: &mut Frame, lines: impl IntoIterator<Item = (u16, String, Style)>) {
    let width = frame.area().width.saturating_sub(4);
    for (y, text, style) in lines {
        frame.render_widget(Paragraph::new(text).style(style), Rect::new(2, y, width, 1));
    }
}

// 16-color ANSI indexes shared by the install screen. The terminal supplies
// their RGB values, so Linux VTs and graphical terminal themes can differ.
pub(crate) const ERR: Color = Color::Indexed(9); // bright red
const FG: Color = Color::Indexed(15); // white default text
const BG: Color = Color::Indexed(0); // black screen / log background
const OK: Color = Color::Indexed(10); // bright green success
const WARN: Color = Color::Indexed(11); // bright yellow warnings
pub(crate) const ACCENT: Color = Color::Indexed(6); // cyan bars / elapsed
pub(crate) const SELECT: Color = Color::Indexed(13); // bright magenta selection
pub(crate) const ON_SELECT: Color = Color::Indexed(0); // black text on selection
pub(crate) const LAND: Color = Color::Indexed(2); // green map continents
const TITLE_BG: Color = Color::Indexed(4); // blue title bar
const PANEL: Color = Color::Indexed(8); // bright black footer / panels

impl Installer {
    /// Window title shown in the title bar, derived from the image version
    /// (if set) and the brand name.
    pub(crate) fn title(&self) -> String {
        let name = &self.ctx.config.branding.name;
        if self.ctx.image_version.is_empty() {
            format!("{name} Installer")
        } else {
            format!("{name} {} Installer", self.ctx.image_version)
        }
    }

    /// Lay out the screen chrome — title bar, optional subtitle, a fill-sized
    /// content slot, and an optional footer — into the retained render tree.
    /// Each route draws its body directly into `frame` on top of this frame.
    pub(crate) fn draw_frame(&self, f: &mut Frame, title: &str, subtitle: &str, footer: &str) {
        let title_style = Style::default()
            .fg(FG)
            .bg(TITLE_BG)
            .add_modifier(Modifier::BOLD);
        let footer_style = Style::default().fg(ACCENT).bg(PANEL);
        let mut children = vec![
            ViewNode::text("screen-title", format!(" {title}"))
                .layout(LayoutStyle {
                    width: Length::Fill(1),
                    height: Length::Cells(1),
                    ..LayoutStyle::default()
                })
                .style(title_style),
        ];
        if !subtitle.is_empty() {
            children.push(
                ViewNode::text("screen-subtitle", format!(" {subtitle}"))
                    .layout(LayoutStyle {
                        width: Length::Fill(1),
                        height: Length::Cells(1),
                        ..LayoutStyle::default()
                    })
                    .style(Style::default().fg(ACCENT).bg(BG)),
            );
        }
        children.push(
            ViewNode::box_("screen-content")
                .layout(LayoutStyle {
                    width: Length::Fill(1),
                    height: Length::Fill(1),
                    ..LayoutStyle::default()
                })
                .style(Style::default().fg(FG).bg(BG)),
        );
        if !footer.is_empty() {
            children.push(
                ViewNode::text("screen-footer", format!(" {footer}"))
                    .layout(LayoutStyle {
                        width: Length::Fill(1),
                        height: Length::Cells(1),
                        ..LayoutStyle::default()
                    })
                    .style(footer_style),
            );
        }

        let root = ViewNode::box_("screen-root")
            .layout(LayoutStyle {
                direction: Direction::Column,
                width: Length::Percent(100),
                height: Length::Percent(100),
                padding: Edges::all(0),
                align: Align::Stretch,
                ..LayoutStyle::default()
            })
            .style(Style::default().fg(FG).bg(BG))
            .children(children);
        let mut tree = self.render_tree.borrow_mut();
        tree.reconcile(root);
        tree.render(f);
    }
}

// Install-screen vertical layout: rows 0-1 are the title/subtitle (draw_frame),
// bottom row is the footer, log fills INSTALL_LOG_Y.. down to it.
const INSTALL_CURRENT_Y: u16 = 2;
const INSTALL_STEP_BAR_Y: u16 = 3;
const INSTALL_OVERALL_Y: u16 = 4;
const INSTALL_LOG_HDR_Y: u16 = 5;
const INSTALL_LOG_Y: u16 = 6;

// Identity-screen layout: render_lines indents values 2 columns past a
// 15-character label, so the editable value begins at column 17. The
// paragraph clips at the field edge (frame width minus the 2-col margin),
// which the cursor must respect so it does not desync on long values.
const IDENTITY_VALUE_X: u16 = 17;

fn clip_chars(s: &str, width: u16) -> String {
    s.chars().take(width as usize).collect()
}

fn rule(f: &mut Frame, x: u16, y: u16, width: u16) {
    f.render_widget(
        Paragraph::new("-".repeat(width as usize))
            .style(Style::default().fg(PANEL).add_modifier(Modifier::DIM)),
        Rect::new(x, y, width, 1),
    );
}

fn section_header(f: &mut Frame, y: u16, label: &str) {
    let width = f.area().width.saturating_sub(4);
    if width == 0 {
        return;
    }

    let label = format!("[ {label} ] ");
    let label_width = label.chars().count().min(width as usize);
    let fill_width = width as usize - label_width;
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                clip_chars(&label, width),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "-".repeat(fill_width),
                Style::default().fg(PANEL).add_modifier(Modifier::DIM),
            ),
        ])),
        Rect::new(2, y, width, 1),
    );
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn progress_bar(fraction: f64, width: usize) -> Line {
    let fraction = fraction.clamp(0.0, 1.0);
    let percent = (fraction * 100.0) as usize;
    if width < 7 {
        return Line::from(vec![Span::raw(clip_chars(
            &format!("{percent}%"),
            width as u16,
        ))]);
    }

    let bar_w = width.saturating_sub(7).min(40);

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

const SHELLS: &[(&str, &str)] = &[("bash", "/bin/bash"), ("zsh", "/bin/zsh")];

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

fn keymap_for_locale(locale: &str) -> &'static str {
    KEYMAPS
        .iter()
        .find(|(l, _)| *l == locale)
        .map_or("us", |(_, k)| *k)
}

fn language_label(idx: usize) -> String {
    let (name, locale) = LANGUAGES.get(idx).copied().unwrap_or(("?", ""));
    format!(
        "{} ({}, keymap {})",
        name,
        locale,
        keymap_for_locale(locale)
    )
}

/// Render the install screen: per-step and overall progress bars, elapsed
/// timer, spinner, and a scrolling log pane. The runtime loop drives install
/// progress into `state.install`; this only draws it.
#[allow(
    clippy::too_many_lines,
    reason = "one cohesive screen render; sections extract once the migration settles"
)]
fn render_install(installer: &Installer, now: Instant, frame: &mut Frame) {
    let state = &installer.state;
    let install = &state.install;
    let steps = pipeline::STEPS;
    let area = frame.area();
    let width = area.width;
    let content_w = width.saturating_sub(2);
    let bold = Style::default().add_modifier(Modifier::BOLD);

    let step_idx = install
        .current_step
        .unwrap_or(0)
        .min(steps.len().saturating_sub(1));
    let running = !install.complete && !install.failed;

    // Current step / status (row 2).
    let st_style = if install.failed {
        Style::default().fg(ERR)
    } else if install.complete {
        Style::default().fg(OK).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let current_line = if running {
        let title = steps.get(step_idx).map_or("Starting install", |s| s.title);
        let frame_n = install
            .started
            .map_or(0, |s| (now.duration_since(s).as_millis() / 80) as usize);
        format!(
            "{} Step {}/{} - {}",
            spinner(frame_n),
            step_idx + 1,
            steps.len(),
            title,
        )
    } else if install.complete {
        "Install complete.".to_string()
    } else {
        match install.failure.as_deref() {
            Some(msg) if !msg.is_empty() => format!("Install failed: {msg}"),
            _ => "Install failed.".to_string(),
        }
    };
    frame.render_widget(
        Paragraph::new(clip_chars(&current_line, content_w)).style(st_style),
        Rect::new(2, INSTALL_CURRENT_Y, content_w, 1),
    );

    // Progress bars (rows 3-4) once a step is known.
    if step_idx < steps.len() {
        let bar_x = 10u16;
        frame.render_widget(
            Paragraph::new("Step").style(bold),
            Rect::new(2, INSTALL_STEP_BAR_Y, 8, 1),
        );
        let step_bar_w = width.saturating_sub(bar_x + 2);
        frame.render_widget(
            Paragraph::new(progress_bar(
                f64::from(install.step_fraction) / 100.0,
                step_bar_w as usize,
            )),
            Rect::new(bar_x, INSTALL_STEP_BAR_Y, step_bar_w, 1),
        );

        let state_of = |i: usize| {
            install
                .step_states
                .get(i)
                .copied()
                .unwrap_or(InstallStepState::Pending)
        };
        let runnable_weight = |range: std::ops::Range<usize>| -> u32 {
            range
                .filter(|&i| state_of(i) != InstallStepState::Skipped)
                .map(|i| steps[i].weight)
                .sum()
        };
        let total = runnable_weight(0..steps.len()).max(1);
        let done = runnable_weight(0..step_idx);
        let active =
            f64::from(steps[step_idx].weight) * (f64::from(install.step_fraction) / 100.0).min(1.0);
        let overall = (f64::from(done) + active) / f64::from(total);

        let elapsed = match (install.started, install.elapsed_seconds) {
            (Some(s), None) if running => now.duration_since(s),
            _ => Duration::from_secs(install.elapsed_seconds.unwrap_or(0)),
        };
        let elapsed_str = format!("Elapsed {}", format_elapsed(elapsed));
        let elapsed_w = elapsed_str.chars().count() as u16;

        frame.render_widget(
            Paragraph::new("Overall").style(bold),
            Rect::new(2, INSTALL_OVERALL_Y, 8, 1),
        );
        let overall_bar_w = width.saturating_sub(bar_x + elapsed_w + 2);
        frame.render_widget(
            Paragraph::new(progress_bar(overall, overall_bar_w as usize)),
            Rect::new(bar_x, INSTALL_OVERALL_Y, overall_bar_w, 1),
        );
        frame.render_widget(
            Paragraph::new(elapsed_str).style(Style::default().fg(ACCENT)),
            Rect::new(
                width.saturating_sub(elapsed_w + 1),
                INSTALL_OVERALL_Y,
                elapsed_w,
                1,
            ),
        );
    }

    // Log header (row 5) + pane (row 6+).
    rule(frame, 0, INSTALL_LOG_HDR_Y, width);
    section_header(frame, INSTALL_LOG_HDR_Y, "Log");
    let log_scroll = state.scroll.install_log;
    if log_scroll > 0 {
        let scroll_label = format!("^{log_scroll} PgDn to follow");
        let scroll_width = scroll_label.chars().count().min(content_w as usize) as u16;
        frame.render_widget(
            Paragraph::new(clip_chars(&scroll_label, scroll_width))
                .style(Style::default().fg(WARN).add_modifier(Modifier::BOLD)),
            Rect::new(
                width.saturating_sub(scroll_width + 2),
                INSTALL_LOG_HDR_Y,
                scroll_width,
                1,
            ),
        );
    }
    // Log pane fills the rows below the header down to (but not overwriting)
    // the bottom footer row. At small terminals there may be no room at all;
    // skip the pane rather than drawing into the footer or past the frame.
    let log_h = area.height.saturating_sub(INSTALL_LOG_Y + 1) as usize;
    if log_h > 0 {
        frame.render_widget(
            Block::default().style(Style::default().fg(FG).bg(BG)),
            Rect::new(0, INSTALL_LOG_Y, width, log_h as u16),
        );
        let log_w = width.saturating_sub(1);
        let end = install.logs.len().saturating_sub(log_scroll);
        let start = end.saturating_sub(log_h);
        for (row, line) in install
            .logs
            .iter()
            .skip(start)
            .take(end.saturating_sub(start))
            .enumerate()
        {
            // row < log_h and log_h = area.height - INSTALL_LOG_Y - 1, so
            // INSTALL_LOG_Y + row stays inside the frame and clear of the
            // footer on the last row.
            frame.render_widget(
                Paragraph::new(clip_chars(line, log_w)),
                Rect::new(1, INSTALL_LOG_Y + row as u16, log_w, 1),
            );
        }
    }
}

/// Render the Secure Boot screen, mirroring the shipping enroll / fallback
/// split. When firmware Setup Mode is on the user can enroll the installer's
/// own keys; otherwise a fallback explains the situation and offers to
/// continue without Secure Boot or reboot.
fn render_secureboot(installer: &Installer, frame: &mut Frame) {
    let state = &installer.state;
    let status = state.firmware.as_ref();
    let sel = state.selection.secure_boot;
    let brand = &installer.ctx.config.branding.name;
    let bold_warn = Style::default().fg(WARN).add_modifier(Modifier::BOLD);

    section_header(frame, 3, "Secure Boot configuration");
    match status.and_then(|s| s.setup_mode) {
        Some(true) => {
            let sb = if status.and_then(|s| s.secure_boot) == Some(true) {
                "enabled"
            } else {
                "disabled"
            };
            render_lines(
                frame,
                [
                    (
                        4,
                        format!(
                            "{brand} can enroll its own Secure Boot keys when firmware Setup Mode is on."
                        ),
                        Style::default(),
                    ),
                    (
                        5,
                        format!("Setup Mode is on. Secure Boot is currently {sb}."),
                        Style::default(),
                    ),
                    (
                        7,
                        "This writes firmware Secure Boot variables.".to_string(),
                        bold_warn,
                    ),
                    (
                        9,
                        format!("{} [ Back ]", if sel == 0 { '>' } else { ' ' }),
                        selected_style(sel == 0),
                    ),
                    (
                        10,
                        format!("{} [ Enroll keys ]", if sel == 1 { '>' } else { ' ' }),
                        selected_style(sel == 1),
                    ),
                ],
            );
        }
        setup => {
            let (title, body) = if setup == Some(false) {
                (
                    "Setup Mode is off",
                    "Secure Boot keys can only be enrolled while firmware Setup Mode is on.\nReboot to firmware setup, or continue without Secure Boot.",
                )
            } else {
                (
                    "Setup Mode not detected",
                    "The installer could not read firmware Setup Mode.\nContinue without Secure Boot, or reboot and check firmware settings.",
                )
            };
            let mut lines: Vec<(u16, String, Style)> = vec![(4, title.to_string(), bold_warn)];
            for (i, line) in body.lines().enumerate() {
                lines.push((5 + i as u16, line.to_string(), bold_warn));
            }
            if let Some(status) = status {
                if !status.reason.is_empty() {
                    lines.push((
                        5 + body.lines().count() as u16,
                        status.reason.clone(),
                        bold_warn,
                    ));
                }
            }
            let y = 8 + (lines.len() - 1) as u16;
            lines.push((
                y,
                format!("{} [ Back ]", if sel == 0 { '>' } else { ' ' }),
                selected_style(sel == 0),
            ));
            lines.push((
                y + 1,
                format!("{} [ Continue ]", if sel == 1 { '>' } else { ' ' }),
                selected_style(sel == 1),
            ));
            lines.push((
                y + 2,
                format!("{} [ Reboot ]", if sel == 2 { '>' } else { ' ' }),
                selected_style(sel == 2),
            ));
            render_lines(frame, lines);
        }
    }
}

fn render_size_warning(installer: &Installer, frame: &mut Frame) {
    let area = frame.area();
    installer.draw_frame(frame, &installer.title(), "", "Resize terminal to continue");
    frame.render_widget(
        Paragraph::new(format!(
            "Current size {}x{}; minimum {}x{}",
            area.width, area.height, MIN_SCREEN_COLS, MIN_SCREEN_ROWS
        ))
        .style(Style::default().fg(WARN)),
        Rect::new(2, 3, area.width.saturating_sub(4), 1),
    );
}

#[allow(
    clippy::too_many_lines,
    reason = "per-route render dispatch; arms extract once the migration settles"
)]
fn render_view(installer: &Installer, now: Instant, frame: &mut Frame) {
    let area = frame.area();
    if screen_too_small(area.width, area.height) {
        render_size_warning(installer, frame);
        return;
    }
    let state = &installer.state;
    let title = match state.route {
        Route::Welcome => installer.title(),
        Route::Target => "Select Target Drive".to_string(),
        Route::Identity => "System Identity".to_string(),
        Route::Timezone => "Select Time Zone".to_string(),
        Route::SecureBoot => "Secure Boot".to_string(),
        Route::Review => "Does this look right?".to_string(),
        Route::Install => {
            if installer.ctx.image_version.is_empty() {
                format!("Installing {}", installer.ctx.config.branding.name)
            } else {
                format!(
                    "Installing {} {}",
                    installer.ctx.config.branding.name, installer.ctx.image_version
                )
            }
        }
        Route::Reboot => "Install Complete".to_string(),
    };
    let footer = if state.route == Route::Install {
        if state.install.complete {
            "Enter continue"
        } else if state.install.failed {
            "Enter back to menu"
        } else if state.install.committed {
            "PgUp/PgDn scroll"
        } else {
            "Esc cancel, PgUp/PgDn scroll"
        }
    } else {
        "Arrows/Tab select, Enter activates, Esc goes back"
    };
    installer.draw_frame(frame, &title, "", footer);

    match state.route {
        Route::Welcome => {
            section_header(frame, 4, "Actions");
            render_lines(
                frame,
                ["Install", "Shell", "Reboot"]
                    .into_iter()
                    .enumerate()
                    .map(|(index, label)| {
                        (
                            6 + index as u16,
                            format!(
                                "{} {label}",
                                if state.selection.welcome == index {
                                    '>'
                                } else {
                                    ' '
                                }
                            ),
                            selected_style(state.selection.welcome == index),
                        )
                    }),
            );
        }
        Route::Target => {
            section_header(frame, 3, "Target disks");
            if state.drives.is_empty() {
                render_lines(
                    frame,
                    [(5, "No target disks detected".to_string(), Style::default())],
                );
            } else {
                render_lines(
                    frame,
                    state.drives.iter().enumerate().map(|(index, drive)| {
                        let busy = if drive.busy_reasons.is_empty() {
                            "available"
                        } else {
                            "BUSY"
                        };
                        let model: String = drive.model.chars().take(20).collect();
                        (
                            4 + index as u16,
                            format!(
                                "{} {:<19} {:<10} {:<8} {model:<20} {busy}",
                                if state.selection.target == Some(index) {
                                    '>'
                                } else {
                                    ' '
                                },
                                drive.path,
                                drive.size,
                                drive.transport,
                            ),
                            selected_style(state.selection.target == Some(index)),
                        )
                    }),
                );
            }
        }
        Route::Identity => {
            section_header(frame, 3, "User account");
            if let Some(draft) = state.identity.as_ref() {
                let password = if draft.reveal_password {
                    draft.password.clone()
                } else {
                    "*".repeat(draft.password.len())
                };
                let repeated = if draft.reveal_password {
                    draft.repeated_password.clone()
                } else {
                    "*".repeat(draft.repeated_password.len())
                };
                let values = [
                    format!("Language       {}", language_label(draft.language_index)),
                    format!("Hostname       {}", draft.hostname),
                    format!("Username       {}", draft.username),
                    format!("Password       {password}"),
                    format!("Repeat         {repeated}"),
                    format!(
                        "Shell          {}",
                        SHELLS.get(draft.shell_index).map_or("?", |(s, _)| *s)
                    ),
                ];
                render_lines(
                    frame,
                    values.into_iter().enumerate().map(|(index, value)| {
                        (
                            4 + index as u16 * 2,
                            value,
                            selected_style(draft.field_index == index),
                        )
                    }),
                );
                if matches!(draft.field_index, 1..=4) {
                    // Count display characters, not bytes: a multi-byte value
                    // would otherwise push the cursor too far right.
                    let value_len = match draft.field_index {
                        1 => draft.hostname.chars().count(),
                        2 => draft.username.chars().count(),
                        3 => draft.password.chars().count(),
                        _ => draft.repeated_password.chars().count(),
                    };
                    frame.set_cursor_style(CursorStyle::BlinkingUnderline);
                    // Clamp to the field's visible edge instead of a hardcoded
                    // 40-column cap so the cursor stays in sync with long
                    // values that the paragraph clips.
                    let col = (IDENTITY_VALUE_X + value_len as u16)
                        .min(frame.area().width.saturating_sub(2));
                    frame.set_cursor_position((col, 4 + draft.field_index as u16 * 2));
                }
            }
            if state.identity.as_ref().is_some_and(|d| d.reveal_password) {
                render_lines(
                    frame,
                    [(
                        16,
                        "Password reveal is on".to_string(),
                        Style::default().fg(WARN),
                    )],
                );
            }
            if let Some(error) = state.validation_error.as_ref() {
                render_lines(
                    frame,
                    [(18, error.clone(), Style::default().fg(Color::Indexed(9)))],
                );
            }
        }
        Route::Timezone => timezone::render(installer, frame),
        Route::SecureBoot => render_secureboot(installer, frame),
        Route::Review => {
            section_header(frame, 3, "Installation summary");
            render_lines(
                frame,
                [
                    (
                        4,
                        format!("Target disk  {}", installer.ctx.drive_label),
                        Style::default(),
                    ),
                    (
                        5,
                        format!("Hostname     {}", installer.ctx.config.identity.hostname),
                        Style::default(),
                    ),
                    (
                        6,
                        format!("Timezone     {}", installer.ctx.config.identity.timezone),
                        Style::default(),
                    ),
                    (
                        9,
                        "[ Back ]".to_string(),
                        selected_style(state.selection.review == 0),
                    ),
                    (
                        10,
                        "[ Start install ]".to_string(),
                        selected_style(state.selection.review == 1),
                    ),
                ],
            );
        }
        Route::Install => render_install(installer, now, frame),
        Route::Reboot => {
            section_header(frame, 3, "Installation complete");
            render_lines(
                frame,
                [
                    (
                        4,
                        format!("Target: {}", installer.ctx.drive_label),
                        Style::default(),
                    ),
                    (
                        6,
                        format!(
                            "Auto-reboot in {}s",
                            state.reboot.remaining_seconds.unwrap_or(0)
                        ),
                        Style::default(),
                    ),
                    (
                        9,
                        "[ Shell ]".to_string(),
                        selected_style(state.selection.reboot == 0),
                    ),
                    (
                        10,
                        "[ Reboot ]".to_string(),
                        selected_style(state.selection.reboot == 1),
                    ),
                ],
            );
        }
    }

    if let Some(modal) = state.active_modal() {
        let (title, body) = match modal {
            Modal::WipeConfirmation => (
                "Wipe target disk?",
                "All data on this disk will be destroyed.",
            ),
            Modal::RebootConfirmation => ("Reboot now?", "The machine will reboot immediately."),
            Modal::Failure => (
                "Installation failed",
                state.install.failure.as_deref().unwrap_or("Unknown error"),
            ),
            Modal::Cancellation => (
                "Cancel install?",
                "The installer will stop and clean up mounts.",
            ),
            Modal::None => ("", ""),
        };
        let area = frame.area();
        let no_sel = state.selection.modal == 0;
        let yes_sel = state.selection.modal == 1;
        let buttons = format!(
            "{} [ No ]    {} [ Yes ]",
            if no_sel { '>' } else { ' ' },
            if yes_sel { '>' } else { ' ' },
        );
        // Size the dialog to its widest line so short confirmations are
        // compact instead of spanning the whole screen, then center it on
        // both axes. Keep at least one column/row clear of every edge and
        // never let the size fall below 1 so the arithmetic cannot underflow.
        let content_w = [title, body, buttons.as_str()]
            .into_iter()
            .map(|s| s.chars().count())
            .max()
            .unwrap_or(0) as u16;
        let modal_w = content_w
            .saturating_add(4)
            .min(area.width.saturating_sub(2).max(1));
        let modal_h = 6u16.min(area.height.saturating_sub(2).max(1));
        let x = area.width.saturating_sub(modal_w) / 2;
        let y = area.height.saturating_sub(modal_h) / 2;
        let modal_style = Style::default().fg(FG).bg(PANEL);
        frame.render_widget(
            Block::default().style(modal_style),
            Rect::new(x, y, modal_w, modal_h),
        );
        if modal_w >= 2 && modal_h >= 2 {
            let inner_w = modal_w.saturating_sub(2);
            let horizontal = "-".repeat(inner_w as usize);
            frame.render_widget(
                Paragraph::new(format!(
                    "+{horizontal}+\n|{}|",
                    " ".repeat(inner_w as usize)
                ))
                .style(modal_style),
                Rect::new(x, y, modal_w, 2),
            );
            frame.render_widget(
                Paragraph::new(format!(
                    "|{}|\n+{horizontal}+",
                    " ".repeat(inner_w as usize)
                ))
                .style(modal_style),
                Rect::new(x, y + modal_h.saturating_sub(2), modal_w, 2),
            );
        }
        let content_w = modal_w.saturating_sub(4);
        frame.render_widget(
            Paragraph::new(clip_chars(title, content_w))
                .style(modal_style.add_modifier(Modifier::BOLD)),
            Rect::new(x.saturating_add(2), y.saturating_add(1), content_w, 1),
        );
        frame.render_widget(
            Paragraph::new(clip_chars(body, content_w)).style(modal_style),
            Rect::new(x.saturating_add(2), y.saturating_add(3), content_w, 1),
        );
        frame.render_widget(
            Paragraph::new(clip_chars(&buttons, content_w)).style(modal_style),
            Rect::new(x.saturating_add(2), y.saturating_add(4), content_w, 1),
        );
    }
}

fn prepare_state(installer: &mut Installer) {
    installer.state.route = Route::Welcome;
    installer.state.selection.reboot = 1;
    installer.state.selection.secure_boot = 1;
    installer.state.selection.review = 1;
    installer.state.install.follow_logs = true;
    if installer.state.identity.is_none() {
        installer.state.identity = Some(IdentityDraft {
            language_index: 0,
            hostname: installer.ctx.config.identity.hostname.clone(),
            username: installer.ctx.username.clone(),
            password: installer.ctx.user_password.clone(),
            repeated_password: installer.ctx.user_password.clone(),
            shell_index: usize::from(installer.ctx.user_shell == "/bin/zsh"),
            field_index: 0,
            reveal_password: false,
        });
    }
    if installer.state.timezone.filtered.is_empty() {
        timezone::prepare(
            &mut installer.state,
            &installer.ctx.config.identity.timezone.clone(),
        );
    }
}

fn message_from_input(
    state: &AppState,
    terminal: &mut Terminal,
    input: &Input,
    terminal_too_small: &mut bool,
) -> Option<Message> {
    match input {
        Input::Key(key) if *terminal_too_small => {
            let escape = key.code == KeyCode::Esc
                || (key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL));
            if !escape || matches!(state.route, Route::Install | Route::Reboot) {
                return None;
            }
            Some(Message::code(KeyCode::Esc))
        }
        Input::Key(key) => Some(Message::Key(*key)),
        Input::Resize => {
            terminal.force_repaint();
            let size = tui_or_exit(screen_size(), "term.size");
            *terminal_too_small = screen_too_small(size.width, size.height);
            Some(Message::Resized {
                width: size.width,
                height: size.height,
            })
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "main event/render loop; helpers extract as it grows"
)]
pub(crate) fn run(installer: &mut Installer, terminal: &mut Terminal) -> Trap {
    prepare_state(installer);
    let mut scheduler = FrameScheduler::new(Instant::now(), 60);
    let mut pending = VecDeque::new();
    let mut task: Option<InstallTask> = None;
    let mut last_tick = Instant::now();
    let mut last_countdown = last_tick;
    let mut last_frame = Instant::now();
    let mut live = false;
    let mut terminal_too_small = false;

    loop {
        for _ in 0..MAX_EVENTS_PER_TURN {
            let event = task
                .as_ref()
                .and_then(|task| task.events_rx.try_recv().ok());
            let Some(event) = event else {
                break;
            };
            pending.push_back(event);
        }

        let now = Instant::now();
        if now.duration_since(last_tick) >= TICK_INTERVAL {
            pending.push_back(Message::Tick);
            last_tick = now;
        }
        if now.duration_since(last_countdown) >= Duration::from_secs(1) {
            pending.push_back(Message::CountdownTick);
            last_countdown = now;
        }

        if let Some(message) = pending.pop_front() {
            let effects = update(&mut installer.state, message);
            scheduler.request_render();
            for effect in effects {
                let (message, trap) = execute_effect(installer, &mut task, effect);
                if let Some(message) = message {
                    pending.push_back(message);
                }
                if let Some(trap) = trap {
                    if let Some(task) = task.as_mut() {
                        task.join();
                    }
                    return trap;
                }
            }
            continue;
        }

        let tz_animating =
            installer.state.route == Route::Timezone && timezone::is_animating(&installer.state);
        // The install screen's spinner and elapsed timer are derived from
        // `now` in render_install, so they only move while we keep
        // rendering. Run in live (60fps) mode while an install is in
        // progress; otherwise the scheduler idles between 100ms ticks and
        // the spinner jerks at ~10fps even though the frame interval is 16ms.
        let install_animating = installer.state.route == Route::Install
            && installer.state.install.started.is_some()
            && !installer.state.install.complete
            && !installer.state.install.failed;
        let animating = tz_animating || install_animating;
        if animating && !live {
            scheduler.acquire_live();
            live = true;
        } else if !animating && live {
            scheduler.release_live();
            live = false;
        }
        if live {
            timezone::tick(&mut installer.state, now.duration_since(last_frame));
        }
        last_frame = now;

        match scheduler.schedule(now) {
            Schedule::RenderNow => {
                tui_or_exit(
                    terminal.draw(|frame| {
                        let area = frame.area();
                        terminal_too_small = screen_too_small(area.width, area.height);
                        render_view(installer, now, frame);
                    }),
                    "draw",
                );
                scheduler.rendered(Instant::now());
            }
            Schedule::RenderAt(deadline) => {
                if let Some(input) = terminal.poll_input(deadline.saturating_duration_since(now))
                    && let Some(message) = message_from_input(
                        &installer.state,
                        terminal,
                        &input,
                        &mut terminal_too_small,
                    )
                {
                    pending.push_back(message);
                }
            }
            Schedule::Idle => {
                if let Some(input) = terminal.poll_input(TICK_INTERVAL)
                    && let Some(message) = message_from_input(
                        &installer.state,
                        terminal,
                        &input,
                        &mut terminal_too_small,
                    )
                {
                    pending.push_back(message);
                }
            }
        }
    }
}
