// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! iocraft application runtime and state transitions.

use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use iocraft::prelude::*;

use crate::engine::{block, firmware, pipeline};
use crate::tui::app::{InstallTask, Installer, Trap};
use crate::tui::message::{DriveInfo, Effect, FieldId, FormId, Message, ToggleId};
use crate::tui::state::{AppState, IdentityDraft, InstallStepState, Modal, Route};
use crate::tui::timezone;
use crate::tui::timezone::WorldMap;

const MAX_LOG_LINES: usize = 2000;
const REBOOT_DELAY_SECONDS: u16 = 30;
const TICK_INTERVAL: Duration = Duration::from_micros(16_667);

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

fn identity_field_mut(draft: &mut IdentityDraft) -> Option<&mut String> {
    match draft.field_index {
        1 => Some(&mut draft.hostname),
        2 => Some(&mut draft.username),
        3 => Some(&mut draft.password),
        4 => Some(&mut draft.repeated_password),
        _ => None,
    }
}

fn update_identity_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    let Some(draft) = state.identity.as_mut() else {
        return Vec::new();
    };
    match key.code {
        KeyCode::Esc => state.route = Route::Target,
        KeyCode::Tab | KeyCode::Down => draft.field_index = (draft.field_index + 1) % 6,
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
            let max = match draft.field_index {
                1 => 253,
                2 => 32,
                _ => usize::MAX,
            };
            if let Some(field) = identity_field_mut(draft)
                && field.len() < max
            {
                field.push(ch);
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

#[expect(
    clippy::too_many_lines,
    reason = "keyboard routing keeps navigation behavior grouped by screen"
)]
fn update_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    if state.active_modal().is_some() {
        return update_modal(state, key);
    }
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if state.route == Route::Install && !state.install.complete && !state.install.failed {
            if !state.install.committed {
                state.push_modal(Modal::Cancellation);
            }
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
            let setup = state.firmware.as_ref().and_then(|status| status.setup_mode);
            let count = if setup == Some(true) { 2 } else { 3 };
            match key.code {
                KeyCode::Esc => state.route = Route::Timezone,
                KeyCode::Left => cycle(&mut state.selection.secure_boot, count, true),
                KeyCode::Right | KeyCode::Tab => {
                    cycle(&mut state.selection.secure_boot, count, false);
                }
                KeyCode::Enter => match (setup, state.selection.secure_boot) {
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
                },
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

#[expect(
    clippy::too_many_lines,
    reason = "message handling keeps state transitions in one dispatch function"
)]
pub(crate) fn update(state: &mut AppState, message: Message) -> Vec<Effect> {
    match message {
        Message::Key(key) => return update_key(state, key),
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
            if state
                .drives
                .iter()
                .any(|drive| drive.index == index && drive.busy_reasons.is_empty())
            {
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
        Message::FormValidated {
            form: FormId::Identity,
            error,
        } => {
            state.validation_error = error;
            if state.validation_error.is_none() {
                state.route = Route::Timezone;
            }
        }
        Message::Resized { .. }
        | Message::Tick
        | Message::Paste(_)
        | Message::FieldFocused { .. }
        | Message::FormValidated { .. } => {}
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
            state.install.logs.push_back(line);
            if state.install.logs.len() > MAX_LOG_LINES {
                state.install.logs.pop_front();
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
                    model: device.model,
                    transport: device.transport,
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

fn terminal_size() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((80, 24))
}

struct UiController {
    ctx: crate::engine::pipeline::Context,
    state: AppState,
    task: Option<InstallTask>,
    drive_rx: Option<mpsc::Receiver<Message>>,
    trap: Option<Trap>,
    last_countdown: Instant,
    last_animation: Instant,
    terminal_size: (u16, u16),
}

fn execute_effect(controller: &mut UiController, effect: Effect) -> Option<Message> {
    match effect {
        Effect::RefreshDrives => {
            if controller.drive_rx.is_none() {
                let (tx, rx) = mpsc::sync_channel(1);
                controller.drive_rx = Some(rx);
                std::thread::spawn(move || {
                    let _ = tx.send(Message::DrivesRefreshed(drive_messages()));
                });
            }
            None
        }
        Effect::DetectFirmware => {
            let status = firmware::detect_setup_mode();
            Some(Message::FirmwareReady {
                status: crate::tui::message::FirmwareStatus {
                    setup_mode: status.setup_mode,
                    secure_boot: status.secure_boot,
                    reason: status.reason,
                },
            })
        }
        Effect::SelectDrive { index } => {
            if let Some(drive) = controller
                .state
                .drives
                .iter()
                .find(|drive| drive.index == index && drive.busy_reasons.is_empty())
            {
                controller.ctx.drive.clone_from(&drive.path);
                controller.ctx.drive_label.clone_from(&drive.label);
            }
            Some(Message::DriveSelected { index })
        }
        Effect::SelectTimezone { name } => {
            controller.ctx.config.identity.timezone = name;
            None
        }
        Effect::SetSecureBoot(enable) => {
            controller.ctx.secure_boot = enable;
            None
        }
        Effect::ValidateForm {
            form: FormId::Identity,
        } => {
            let error = controller
                .state
                .identity
                .as_ref()
                .and_then(validate_identity);
            if error.is_none()
                && let Some(draft) = controller.state.identity.as_ref()
            {
                controller.ctx.config.identity.hostname = draft.hostname.trim().to_string();
                controller.ctx.username = draft.username.trim().to_string();
                controller.ctx.user_password.clone_from(&draft.password);
                controller.ctx.user_shell = if draft.shell_index == 0 {
                    "/bin/bash".to_string()
                } else {
                    "/bin/zsh".to_string()
                };
            }
            Some(Message::FormValidated {
                form: FormId::Identity,
                error,
            })
        }
        Effect::ValidateForm { form } => Some(Message::FormValidated { form, error: None }),
        Effect::StartInstall => {
            controller.state.install.started = Some(Instant::now());
            controller.task = Some(InstallTask::start(&controller.ctx));
            crate::engine::util::clear_secret(&mut controller.ctx.user_password);
            crate::engine::util::clear_secret(&mut controller.ctx.k3s_token);
            if let Some(draft) = controller.state.identity.as_mut() {
                crate::engine::util::clear_secret(&mut draft.password);
                crate::engine::util::clear_secret(&mut draft.repeated_password);
                draft.reveal_password = false;
            }
            None
        }
        Effect::CancelPipeline => {
            if let Some(task) = controller.task.as_ref() {
                task.cancel();
            }
            None
        }
        Effect::RunShell { .. } => {
            controller.trap = Some(Trap::Shell);
            None
        }
        Effect::Reboot => {
            controller.trap = Some(Trap::Reboot);
            None
        }
        Effect::Quit => {
            controller.trap = Some(Trap::Quit);
            None
        }
        Effect::EnrollKeys | Effect::None => None,
    }
}

fn dispatch(controller: &mut UiController, message: Message) {
    let mut messages = vec![message];
    while let Some(message) = messages.pop() {
        for effect in update(&mut controller.state, message) {
            if let Some(message) = execute_effect(controller, effect) {
                messages.push(message);
            }
        }
    }
}

fn prepare_state(controller: &mut UiController) {
    controller.state.route = Route::Welcome;
    controller.state.selection.reboot = 1;
    controller.state.selection.secure_boot = 1;
    controller.state.selection.review = 1;
    controller.state.install.follow_logs = true;
    if controller.state.identity.is_none() {
        controller.state.identity = Some(IdentityDraft {
            language_index: 0,
            hostname: controller.ctx.config.identity.hostname.clone(),
            username: controller.ctx.username.clone(),
            password: controller.ctx.user_password.clone(),
            repeated_password: controller.ctx.user_password.clone(),
            shell_index: usize::from(controller.ctx.user_shell == "/bin/zsh"),
            field_index: 0,
            reveal_password: false,
        });
    }
    if controller.state.timezone.filtered.is_empty() {
        timezone::prepare(
            &mut controller.state,
            &controller.ctx.config.identity.timezone.clone(),
        );
    }
}

fn service(controller: &mut UiController) -> bool {
    if controller.trap.is_some() {
        return false;
    }
    let now = Instant::now();
    let animation_elapsed = now.saturating_duration_since(controller.last_animation);
    controller.last_animation = now;
    let mut dirty = false;
    let mut messages = Vec::new();
    let drive_message = controller
        .drive_rx
        .as_ref()
        .and_then(|rx| match rx.try_recv() {
            Ok(message) => Some(message),
            Err(mpsc::TryRecvError::Disconnected) => Some(Message::DrivesRefreshed(Vec::new())),
            Err(mpsc::TryRecvError::Empty) => None,
        });
    if let Some(message) = drive_message {
        controller.drive_rx = None;
        messages.push(message);
    }
    if let Some(task) = controller.task.as_ref() {
        while let Ok(step_index) = task.commit_rx.try_recv() {
            if pipeline::STEPS
                .get(step_index)
                .is_some_and(|step| step.point_of_no_return)
                && !controller.state.install.committed
            {
                controller.state.install.committed = true;
                dirty = true;
            }
        }
        while let Ok(message) = task.events_rx.try_recv() {
            messages.push(message);
        }
    }
    if !messages.is_empty() {
        dirty = true;
    }
    for message in messages {
        dispatch(controller, message);
    }
    if controller.state.route == Route::Timezone {
        dirty |= timezone::tick(&mut controller.state, animation_elapsed);
    }
    if controller.last_countdown.elapsed() >= Duration::from_secs(1) {
        controller.last_countdown = Instant::now();
        if controller.state.route == Route::Reboot && controller.state.reboot.automatic {
            dispatch(controller, Message::CountdownTick);
            dirty = true;
        }
    }
    dirty
}

fn overall_install_progress(state: &AppState) -> u16 {
    if state.install.complete {
        return 100;
    }
    let total_weight = pipeline::STEPS
        .iter()
        .map(|step| u64::from(step.weight))
        .sum::<u64>();
    let Some(step_index) = state.install.current_step else {
        return 0;
    };
    let completed_weight = pipeline::STEPS
        .iter()
        .take(step_index)
        .map(|step| u64::from(step.weight))
        .sum::<u64>();
    let current_weight = pipeline::STEPS
        .get(step_index)
        .map_or(0, |step| u64::from(step.weight));
    let progressed_weight =
        completed_weight + current_weight * u64::from(state.install.step_fraction.min(100)) / 100;
    u16::try_from(progressed_weight * 100 / total_weight.max(1)).unwrap_or(100)
}

fn selected_row(content: impl Into<String>, selected: bool) -> AnyElement<'static> {
    let marker = if selected { ">" } else { " " };
    let weight = if selected {
        Weight::Bold
    } else {
        Weight::Normal
    };
    let content = content.into();
    element! {
        View(width: 100pct, height: 1, padding_left: 1) {
            Text(content: format!("{marker} {content}"), weight: weight, invert: selected, wrap: TextWrap::NoWrap)
        }
    }
    .into_any()
}

fn text_row(content: String, weight: Weight) -> AnyElement<'static> {
    element! {
        View(width: 100pct, min_height: 1) {
            Text(content: content, weight: weight)
        }
    }
    .into_any()
}

fn panel(label: &str, rows: Vec<AnyElement<'static>>) -> AnyElement<'static> {
    element! {
        View(width: 100pct, height: 100pct, flex_direction: FlexDirection::Column, overflow: Overflow::Hidden) {
            View(width: 100pct, height: 2) {
                Text(content: label.to_string(), weight: Weight::Light)
            }
            #(rows)
        }
    }
    .into_any()
}

fn progress_row(label: &str, percent: u16, terminal_width: u16) -> AnyElement<'static> {
    let bar_width = usize::from(terminal_width.saturating_sub(24)).clamp(8, 40);
    let filled = usize::from(percent.min(100)) * bar_width / 100;
    let bar = format!(
        "{label:<8} [{}{}] {percent:3}%",
        "#".repeat(filled),
        "-".repeat(bar_width - filled)
    );
    text_row(bar, Weight::Bold)
}

fn welcome_content(state: &AppState) -> AnyElement<'static> {
    let rows = ["Install system", "Open shell", "Reboot"]
        .into_iter()
        .enumerate()
        .map(|(index, label)| selected_row(label.to_string(), state.selection.welcome == index))
        .collect();
    panel("Actions", rows)
}

fn target_content(controller: &UiController) -> AnyElement<'static> {
    let state = &controller.state;
    let rows = if controller.drive_rx.is_some() && state.drives.is_empty() {
        vec![text_row(
            "Scanning for target disks…".to_string(),
            Weight::Bold,
        )]
    } else if state.drives.is_empty() {
        vec![text_row(
            "No target disks detected".to_string(),
            Weight::Bold,
        )]
    } else {
        state
            .drives
            .iter()
            .enumerate()
            .map(|(index, drive)| {
                let status = if drive.busy_reasons.is_empty() {
                    "available"
                } else {
                    "BUSY"
                };
                selected_row(
                    format!(
                        "{:<18} {:<10} {:<8} {:<20} {status}",
                        drive.path,
                        drive.size,
                        drive.transport,
                        drive.model.chars().take(20).collect::<String>()
                    ),
                    state.selection.target == Some(index),
                )
            })
            .collect()
    };
    panel("Target disks", rows)
}

fn identity_content(state: &AppState) -> AnyElement<'static> {
    let Some(draft) = state.identity.as_ref() else {
        return panel(
            "User account",
            vec![text_row("Loading identity…".to_string(), Weight::Normal)],
        );
    };
    let password = if draft.reveal_password {
        draft.password.clone()
    } else {
        "*".repeat(draft.password.chars().count())
    };
    let repeated = if draft.reveal_password {
        draft.repeated_password.clone()
    } else {
        "*".repeat(draft.repeated_password.chars().count())
    };
    let language = LANGUAGES
        .get(draft.language_index)
        .map_or("?", |(label, _)| *label);
    let shell = if draft.shell_index == 0 {
        "bash"
    } else {
        "zsh"
    };
    let values = [
        format!("Language       {language}"),
        format!("Hostname       {}", draft.hostname),
        format!("Username       {}", draft.username),
        format!("Password       {password}"),
        format!("Repeat         {repeated}"),
        format!("Shell          {shell}"),
    ];
    let mut rows = values
        .into_iter()
        .enumerate()
        .map(|(index, value)| selected_row(value, draft.field_index == index))
        .collect::<Vec<_>>();
    if draft.reveal_password {
        rows.push(text_row("Password reveal is on".to_string(), Weight::Bold));
    }
    if let Some(error) = state.validation_error.as_ref() {
        rows.push(text_row(format!("Error: {error}"), Weight::Bold));
    }
    panel("User account", rows)
}

fn timezone_content(state: &AppState) -> AnyElement<'static> {
    let zone_rows = timezone::visible_names(state)
        .into_iter()
        .map(|(name, selected)| selected_row(name, selected))
        .collect::<Vec<_>>();
    let filter = if state.timezone.filter.is_empty() {
        "Type to filter…".to_string()
    } else {
        state.timezone.filter.clone()
    };
    let selected_zone = timezone::selected_zone(state);
    element! {
        View(width: 100pct, height: 100pct, flex_direction: FlexDirection::Row) {
            View(width: 64pct, height: 100pct) {
                WorldMap(
                    center_lon_milli: state.timezone.map_center_lon_milli,
                    selected_zone,
                    width: 100pct,
                    height: 100pct,
                )
            }
            View(width: 36pct, height: 100pct, padding_left: 2, flex_direction: FlexDirection::Column, overflow: Overflow::Hidden) {
                View(width: 100pct, height: 2) {
                    Text(content: "Time zones", weight: Weight::Light)
                }
                View(width: 100pct, height: 2) {
                    Text(content: format!("Filter  {filter}"), wrap: TextWrap::NoWrap)
                }
                #(zone_rows)
            }
        }
    }
    .into_any()
}

fn secure_boot_content(controller: &UiController) -> AnyElement<'static> {
    let state = &controller.state;
    let setup_mode = state.firmware.as_ref().and_then(|status| status.setup_mode);
    let mut rows = Vec::new();
    if setup_mode == Some(true) {
        rows.push(text_row(
            format!(
                "{} can enroll its own Secure Boot keys.",
                controller.ctx.config.branding.name
            ),
            Weight::Normal,
        ));
        rows.push(text_row(
            "This writes firmware Secure Boot variables.".to_string(),
            Weight::Bold,
        ));
        rows.push(selected_row(
            "[ Back ]".to_string(),
            state.selection.secure_boot == 0,
        ));
        rows.push(selected_row(
            "[ Enroll keys ]".to_string(),
            state.selection.secure_boot == 1,
        ));
    } else {
        let message = if setup_mode == Some(false) {
            "Setup Mode is off; keys cannot be enrolled."
        } else {
            "Firmware Setup Mode could not be detected."
        };
        rows.push(text_row(message.to_string(), Weight::Bold));
        if let Some(status) = state
            .firmware
            .as_ref()
            .filter(|status| !status.reason.is_empty())
        {
            rows.push(text_row(status.reason.clone(), Weight::Bold));
        }
        for (index, label) in ["[ Back ]", "[ Continue without Secure Boot ]", "[ Reboot ]"]
            .into_iter()
            .enumerate()
        {
            rows.push(selected_row(
                label.to_string(),
                state.selection.secure_boot == index,
            ));
        }
    }
    panel("Secure Boot configuration", rows)
}

fn review_content(controller: &UiController) -> AnyElement<'static> {
    let state = &controller.state;
    let rows = vec![
        text_row(
            format!("Target disk  {}", controller.ctx.drive_label),
            Weight::Normal,
        ),
        text_row(
            format!("Hostname     {}", controller.ctx.config.identity.hostname),
            Weight::Normal,
        ),
        text_row(
            format!("Timezone     {}", controller.ctx.config.identity.timezone),
            Weight::Normal,
        ),
        selected_row("[ Back ]".to_string(), state.selection.review == 0),
        selected_row(
            "[ Start installation ]".to_string(),
            state.selection.review == 1,
        ),
    ];
    panel("Installation summary", rows)
}

fn install_content(controller: &UiController) -> AnyElement<'static> {
    let state = &controller.state;
    let step = state
        .install
        .current_step
        .and_then(|index| pipeline::STEPS.get(index))
        .map_or("Starting installation", |step| step.title);
    let status = if state.install.complete {
        "Installation complete"
    } else if state.install.failed {
        "Installation failed"
    } else {
        step
    };
    let visible_lines = usize::from(controller.terminal_size.1.saturating_sub(13)).max(3);
    let end = state
        .install
        .logs
        .len()
        .saturating_sub(state.scroll.install_log.min(state.install.logs.len()));
    let start = end.saturating_sub(visible_lines);
    let logs = state
        .install
        .logs
        .iter()
        .skip(start)
        .take(end - start)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    element! {
        View(width: 100pct, height: 100pct, flex_direction: FlexDirection::Column, row_gap: 1, overflow: Overflow::Hidden) {
            Text(content: status, weight: Weight::Bold)
            #(progress_row("Overall", overall_install_progress(state), controller.terminal_size.0))
            #(progress_row("Step", state.install.step_fraction, controller.terminal_size.0))
            View(width: 100pct, flex_grow: 1.0_f32, padding: 1, overflow: Overflow::Hidden) {
                Text(content: logs)
            }
        }
    }
    .into_any()
}

fn reboot_content(controller: &UiController) -> AnyElement<'static> {
    let state = &controller.state;
    panel(
        "Installation complete",
        vec![
            text_row(
                format!("Target  {}", controller.ctx.drive_label),
                Weight::Normal,
            ),
            text_row(
                format!(
                    "Automatic reboot in {} seconds",
                    state.reboot.remaining_seconds.unwrap_or(0)
                ),
                Weight::Bold,
            ),
            selected_row("[ Open shell ]".to_string(), state.selection.reboot == 0),
            selected_row("[ Reboot now ]".to_string(), state.selection.reboot == 1),
        ],
    )
}

fn modal_element(state: &AppState, width: u16, height: u16) -> Option<AnyElement<'static>> {
    let modal = state.active_modal()?;
    let (title, body) = match modal {
        Modal::WipeConfirmation => (
            "Wipe target disk?".to_string(),
            "All data on this disk will be destroyed.".to_string(),
        ),
        Modal::RebootConfirmation => (
            "Reboot now?".to_string(),
            "The machine will reboot immediately.".to_string(),
        ),
        Modal::Failure => (
            "Installation failed".to_string(),
            state
                .install
                .failure
                .clone()
                .unwrap_or_else(|| "Unknown error".to_string()),
        ),
        Modal::Cancellation => (
            "Cancel installation?".to_string(),
            "The installer will stop and clean up mounts.".to_string(),
        ),
        Modal::None => return None,
    };
    let max_dialog_width = width.saturating_sub(2).max(1);
    let dialog_width = width
        .saturating_mul(2)
        .saturating_div(3)
        .max(36)
        .min(max_dialog_width);
    let body_width = dialog_width.saturating_sub(4).max(1);
    let body_lines = u16::try_from(body.chars().count())
        .unwrap_or(u16::MAX)
        .div_ceil(body_width);
    let dialog_height = body_lines
        .saturating_add(7)
        .max(9)
        .min(height.saturating_sub(2).max(1));
    let left = width.saturating_sub(dialog_width) / 2;
    let top = height.saturating_sub(dialog_height) / 2;
    let buttons = if state.selection.modal == 0 {
        "[ No ]     Yes"
    } else {
        "No     [ Yes ]"
    };
    Some(
        element! {
            View(
                position: Position::Absolute,
                left,
                top,
                width: dialog_width,
                height: dialog_height,
                padding: 1,
                flex_direction: FlexDirection::Column,
                row_gap: 1,
                border_style: BorderStyle::Single,
                overflow: Overflow::Hidden,
            ) {
                Text(content: title, weight: Weight::Bold, align: TextAlign::Center)
                Text(content: body, weight: Weight::Bold, align: TextAlign::Center)
                Text(content: buttons, weight: Weight::Bold, align: TextAlign::Center)
            }
        }
        .into_any(),
    )
}

struct Screen {
    title: String,
    content: AnyElement<'static>,
    footer: &'static str,
    modal: Option<AnyElement<'static>>,
}

fn screen(controller: &UiController) -> Screen {
    let state = &controller.state;
    let title = match state.route {
        Route::Welcome => {
            if controller.ctx.image_version.is_empty() {
                format!("{} Installer", controller.ctx.config.branding.name)
            } else {
                format!(
                    "{} {} Installer",
                    controller.ctx.config.branding.name, controller.ctx.image_version
                )
            }
        }
        Route::Target => "Select Target Drive".to_string(),
        Route::Identity => "System Identity".to_string(),
        Route::Timezone => "Select Time Zone".to_string(),
        Route::SecureBoot => "Secure Boot".to_string(),
        Route::Review => "Does this look right?".to_string(),
        Route::Install => format!("Installing {}", controller.ctx.config.branding.name),
        Route::Reboot => "Install Complete".to_string(),
    };
    let content = match state.route {
        Route::Welcome => welcome_content(state),
        Route::Target => target_content(controller),
        Route::Identity => identity_content(state),
        Route::Timezone => timezone_content(state),
        Route::SecureBoot => secure_boot_content(controller),
        Route::Review => review_content(controller),
        Route::Install => install_content(controller),
        Route::Reboot => reboot_content(controller),
    };
    let footer = match state.route {
        Route::Welcome => " Up/Down select  Enter activate  Esc quit",
        Route::Target => " Up/Down select  Enter continue  Ctrl-R rescan  Esc back",
        Route::Identity => " Up/Down/Tab field  Left/Right choose  Enter continue  Esc back",
        Route::Timezone => " Type filter  Up/Down choose  Left/Right pan  Enter continue  Esc back",
        Route::SecureBoot | Route::Review | Route::Reboot => {
            " Up/Down/Tab select  Enter activate  Esc back"
        }
        Route::Install if state.install.complete => " Enter continue",
        Route::Install if state.install.failed => " Enter back to menu",
        Route::Install if state.install.committed => " PgUp/PgDn scroll",
        Route::Install => " Esc cancel  PgUp/PgDn scroll",
    };
    Screen {
        title,
        content,
        footer,
        modal: modal_element(
            state,
            controller.terminal_size.0,
            controller.terminal_size.1,
        ),
    }
}

#[derive(Props)]
struct InstallerTuiProps {
    controller: Arc<Mutex<UiController>>,
}

impl Default for InstallerTuiProps {
    fn default() -> Self {
        Self {
            controller: Arc::new(Mutex::new(UiController {
                ctx: crate::engine::pipeline::Context::default(),
                state: AppState::default(),
                task: None,
                drive_rx: None,
                trap: None,
                last_countdown: Instant::now(),
                last_animation: Instant::now(),
                terminal_size: terminal_size(),
            })),
        }
    }
}

#[component]
fn InstallerTui(props: &InstallerTuiProps, mut hooks: Hooks) -> impl Into<AnyElement<'static>> {
    let mut revision = hooks.use_state(|| 0_u64);
    let mut system_context = hooks.use_context_mut::<SystemContext>();
    let controller = props.controller.clone();
    hooks.use_terminal_events(move |event| {
        let Ok(mut controller) = controller.lock() else {
            return;
        };
        if controller.trap.is_some() {
            return;
        }
        match event {
            TerminalEvent::Key(key) if key.kind != KeyEventKind::Release => {
                dispatch(
                    &mut controller,
                    Message::Key(KeyEvent {
                        code: key.code,
                        modifiers: key.modifiers,
                        kind: key.kind,
                        state: KeyEventState::NONE,
                    }),
                );
                revision.set(revision.get().wrapping_add(1));
            }
            TerminalEvent::Resize(width, height) => {
                controller.terminal_size = (width, height);
                revision.set(revision.get().wrapping_add(1));
            }
            _ => {}
        }
    });
    let controller = props.controller.clone();
    hooks.use_future(async move {
        loop {
            smol::Timer::after(TICK_INTERVAL).await;
            if let Ok(mut controller) = controller.lock() {
                if controller.trap.is_some() {
                    break;
                }
                if service(&mut controller) {
                    revision.set(revision.get().wrapping_add(1));
                }
                if controller.trap.is_some() {
                    break;
                }
            }
        }
    });

    let (screen_view, terminal_width, terminal_height, should_exit) =
        props.controller.lock().map_or_else(
            |_| {
                (
                    Screen {
                        title: "Installer unavailable".to_string(),
                        content: text_row("Installer state unavailable".to_string(), Weight::Bold),
                        footer: "",
                        modal: None,
                    },
                    80,
                    24,
                    true,
                )
            },
            |controller| {
                let terminal_width = controller.terminal_size.0;
                let terminal_height = controller.terminal_size.1;
                (
                    screen(&controller),
                    terminal_width,
                    terminal_height,
                    controller.trap.is_some(),
                )
            },
        );
    if should_exit {
        system_context.exit();
    }
    let Screen {
        title,
        content,
        footer,
        modal,
    } = screen_view;
    element! {
        View(
            width: terminal_width,
            height: terminal_height,
            position: Position::Relative,
            flex_direction: FlexDirection::Column,
        ) {
            View(width: 100pct, height: 1, padding_left: 2) {
                Text(content: title, weight: Weight::Bold, wrap: TextWrap::NoWrap)
            }
            View(width: 100pct, flex_grow: 1.0_f32, padding_top: 1, padding_bottom: 1, padding_left: 2, padding_right: 2, overflow: Overflow::Hidden) {
                #(content)
            }
            View(width: 100pct, height: 1, padding_left: 1) {
                Text(content: footer, weight: Weight::Light, wrap: TextWrap::NoWrap)
            }
            #(modal)
        }
    }
}

impl Installer {
    pub(crate) fn run_iocraft(&mut self) -> Trap {
        let controller = Arc::new(Mutex::new(UiController {
            ctx: std::mem::take(&mut self.ctx),
            state: std::mem::take(&mut self.state),
            task: None,
            drive_rx: None,
            trap: None,
            last_countdown: Instant::now(),
            last_animation: Instant::now(),
            terminal_size: terminal_size(),
        }));
        if let Ok(mut state) = controller.lock() {
            prepare_state(&mut state);
        }
        let _ = smol::block_on(element!(InstallerTui(controller: controller.clone())).fullscreen());
        let mut controller = controller
            .lock()
            .unwrap_or_else(|_| panic!("installer state lock poisoned"));
        if let Some(task) = controller.task.take() {
            let _ = task.cancel_and_join();
        }
        self.ctx = std::mem::take(&mut controller.ctx);
        self.state = std::mem::take(&mut controller.state);
        controller.trap.take().unwrap_or(Trap::Quit)
    }
}
