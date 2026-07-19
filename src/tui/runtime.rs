// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! iocraft application runtime and state transitions.

use std::sync::{Arc, Mutex};
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
const TICK_INTERVAL: Duration = Duration::from_millis(100);

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

struct UiController {
    ctx: crate::engine::pipeline::Context,
    state: AppState,
    task: Option<InstallTask>,
    trap: Option<Trap>,
    last_countdown: Instant,
}

fn execute_effect(controller: &mut UiController, effect: Effect) -> Option<Message> {
    match effect {
        Effect::RefreshDrives => Some(Message::DrivesRefreshed(drive_messages())),
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
        Effect::EnrollKeys | Effect::Log(_) | Effect::None => None,
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

fn service(controller: &mut UiController) {
    if controller.trap.is_some() {
        return;
    }
    let mut messages = Vec::new();
    if let Some(task) = controller.task.as_ref() {
        while let Ok(step_index) = task.commit_rx.try_recv() {
            if pipeline::STEPS
                .get(step_index)
                .is_some_and(|step| step.point_of_no_return)
            {
                controller.state.install.committed = true;
            }
        }
        while let Ok(message) = task.events_rx.try_recv() {
            messages.push(message);
        }
    }
    for message in messages {
        dispatch(controller, message);
    }
    dispatch(controller, Message::Tick);
    timezone::tick(&mut controller.state, TICK_INTERVAL);
    if controller.last_countdown.elapsed() >= Duration::from_secs(1) {
        dispatch(controller, Message::CountdownTick);
        controller.last_countdown = Instant::now();
    }
}

fn modal_text(state: &AppState) -> Option<String> {
    let modal = state.active_modal()?;
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
        Modal::None => return None,
    };
    let selected = if state.selection.modal == 0 {
        "[ No ]   Yes"
    } else {
        "No   [ Yes ]"
    };
    Some(format!("\n{title}\n{body}\n{selected}"))
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

fn screen_text(controller: &UiController) -> String {
    let state = &controller.state;
    let body = match state.route {
        Route::Welcome => ["Install", "Shell", "Reboot"].iter().enumerate().map(|(i, label)| format!("{} {label}", if i == state.selection.welcome { ">" } else { " " })).collect::<Vec<_>>().join("\n"),
        Route::Target => state.drives.iter().enumerate().map(|(i, drive)| format!("{} {}  {}  {}  {}", if state.selection.target == Some(i) { ">" } else { " " }, drive.path, drive.size, drive.transport, if drive.busy_reasons.is_empty() { "available" } else { "BUSY" })).collect::<Vec<_>>().join("\n"),
        Route::Identity => state.identity.as_ref().map_or_else(|| "Loading identity...".to_string(), |draft| {
            let password = if draft.reveal_password { draft.password.clone() } else { "*".repeat(draft.password.len()) };
            let repeated = if draft.reveal_password { draft.repeated_password.clone() } else { "*".repeat(draft.repeated_password.len()) };
            format!("{} Language: {}\n{} Hostname: {}\n{} Username: {}\n{} Password: {}\n{} Repeat: {}\n{} Shell: {}{}", if draft.field_index == 0 { ">" } else { " " }, LANGUAGES[draft.language_index].0, if draft.field_index == 1 { ">" } else { " " }, draft.hostname, if draft.field_index == 2 { ">" } else { " " }, draft.username, if draft.field_index == 3 { ">" } else { " " }, password, if draft.field_index == 4 { ">" } else { " " }, repeated, if draft.field_index == 5 { ">" } else { " " }, if draft.shell_index == 0 { "bash" } else { "zsh" }, state.validation_error.as_ref().map_or_else(String::new, |error| format!("\nError: {error}")))
        }),
        Route::Timezone => format!("Filter: {}\n{}", state.timezone.filter, timezone::visible_names(state).join("\n")),
        Route::SecureBoot => format!("Secure Boot setup mode: {:?}\n{} [ Back ]\n{} [ Continue ]\n{} [ Reboot ]", state.firmware.as_ref().and_then(|status| status.setup_mode), if state.selection.secure_boot == 0 { ">" } else { " " }, if state.selection.secure_boot == 1 { ">" } else { " " }, if state.selection.secure_boot == 2 { ">" } else { " " }),
        Route::Review => format!("Target disk: {}\nHostname: {}\nTimezone: {}\n\n{} [ Back ]\n{} [ Start install ]", controller.ctx.drive_label, controller.ctx.config.identity.hostname, controller.ctx.config.identity.timezone, if state.selection.review == 0 { ">" } else { " " }, if state.selection.review == 1 { ">" } else { " " }),
        Route::Install => {
            let step = state.install.current_step.and_then(|index| pipeline::STEPS.get(index)).map_or("Starting install", |step| step.title);
            let status = if state.install.complete { "Install complete" } else if state.install.failed { "Install failed" } else { step };
            let logs = state.install.logs.iter().rev().take(12).collect::<Vec<_>>().into_iter().rev().map(String::as_str).collect::<Vec<_>>().join("\n");
            format!(
                "{status}\nOverall progress: {}%\nStep progress: {}%\n\nLog\n{logs}",
                overall_install_progress(state),
                state.install.step_fraction
            )
        }
        Route::Reboot => format!("Target: {}\nAuto-reboot in {}s\n\n{} [ Shell ]\n{} [ Reboot ]", controller.ctx.drive_label, state.reboot.remaining_seconds.unwrap_or(0), if state.selection.reboot == 0 { ">" } else { " " }, if state.selection.reboot == 1 { ">" } else { " " }),
    };
    let title = match state.route {
        Route::Welcome => format!("{} Installer", controller.ctx.config.branding.name),
        Route::Target => "Select Target Drive".to_string(),
        Route::Identity => "System Identity".to_string(),
        Route::Timezone => "Select Time Zone".to_string(),
        Route::SecureBoot => "Secure Boot".to_string(),
        Route::Review => "Does this look right?".to_string(),
        Route::Install => format!("Installing {}", controller.ctx.config.branding.name),
        Route::Reboot => "Install Complete".to_string(),
    };
    format!(
        "{title}\n\n{body}{}\n\nArrows/Tab select, Enter activates, Esc goes back",
        modal_text(state).unwrap_or_default()
    )
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
                trap: None,
                last_countdown: Instant::now(),
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
        if let TerminalEvent::Key(key) = event
            && key.kind != KeyEventKind::Release
            && let Ok(mut controller) = controller.lock()
            && controller.trap.is_none()
        {
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
    });
    let controller = props.controller.clone();
    hooks.use_future(async move {
        loop {
            smol::Timer::after(TICK_INTERVAL).await;
            if let Ok(mut controller) = controller.lock() {
                if controller.trap.is_some() {
                    break;
                }
                service(&mut controller);
                revision.set(revision.get().wrapping_add(1));
                if controller.trap.is_some() {
                    break;
                }
            }
        }
    });

    let (text, timezone, center_lon_milli, selected_zone, should_exit) =
        props.controller.lock().map_or_else(
            |_| {
                (
                    "Installer state unavailable".to_string(),
                    false,
                    0,
                    None,
                    true,
                )
            },
            |controller| {
                (
                    screen_text(&controller),
                    controller.state.route == Route::Timezone,
                    controller.state.timezone.map_center_lon_milli,
                    timezone::selected_zone(&controller.state),
                    controller.trap.is_some(),
                )
            },
        );
    if should_exit {
        system_context.exit();
    }
    element! {
        View(width: 100pct, height: 100pct, flex_direction: FlexDirection::Column, padding: 1, background_color: Color::Black) {
            #(if timezone {
                element! {
                    View(height: 55pct, border_style: BorderStyle::Single, border_color: Color::Blue) {
                        WorldMap(center_lon_milli, selected_zone, width: 100pct, height: 100pct)
                    }
                }
            } else {
                element! { View }
            })
            Text(content: text, color: Color::White)
        }
    }
}

impl Installer {
    pub(crate) fn run_iocraft(&mut self) -> Trap {
        let controller = Arc::new(Mutex::new(UiController {
            ctx: std::mem::take(&mut self.ctx),
            state: std::mem::take(&mut self.state),
            task: None,
            trap: None,
            last_countdown: Instant::now(),
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
