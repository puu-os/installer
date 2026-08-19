// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! Installer runtime: state transitions, input dispatch, and the offscreen-frame renderer.
//!
//! Each frame is painted into an offscreen [`Frame`] cell buffer and blitted to
//! the terminal by the diff renderer in [`crate::tui::render`]. There is no
//! widget tree: route bodies draw rectangles, borders, and styled text directly
//! into the buffer, so the whole screen is one coherent frame the blit owns.

use std::io;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use zeroize::Zeroize;

use crate::engine::firmware::SecureBootMode;
use crate::engine::{block, firmware, pipeline};
use crate::tui::app::{InstallTask, Installer, Trap};
use crate::tui::message::{DriveInfo, Effect, Message, MouseKind, sanitize_text};
use crate::tui::render::{
    self, Block, Border, CursorStyle, Frame, Input, Line, Paragraph, Rect, Span, Terminal,
    install_panic_hook, mouse_kind, poll_input, tui_or_exit,
};
use crate::tui::state::{
    AppState, Cyclic, IdentityDraft, IdentityField, InstallStatus, InstallStepState, LocaleChoice,
    Modal, ModalChoice, RebootChoice, ReviewChoice, Route, SecureBootChoice, WelcomeChoice,
};
use crate::tui::style::Style;
use crate::tui::theme::Theme;
use crate::tui::timezone;

const MAX_LOG_LINES: usize = 2000;
const REBOOT_DELAY_SECONDS: u16 = 30;
const MAP_FRAME_INTERVAL: Duration = Duration::from_nanos(16_666_667);
const ACTIVE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Shared budget for install spinner + coalesced log/progress redraws.
const INSTALL_RENDER_INTERVAL: Duration = Duration::from_millis(100);

fn cycle(index: &mut usize, count: usize, backwards: bool) {
    if count == 0 {
        *index = 0;
    } else if backwards {
        *index = index.checked_sub(1).unwrap_or(count - 1);
    } else {
        *index = (*index + 1) % count;
    }
}

/// Terminal families that implement the xterm mouse protocol.
const MOUSE_TERM_PREFIXES: &[&str] = &[
    "xterm",
    "screen",
    "tmux",
    "rxvt",
    "alacritty",
    "kitty",
    "wezterm",
    "foot",
    "ghostty",
    "st",
    "vte",
    "gnome",
    "konsole",
    "contour",
];

/// Decide whether mouse reporting should be enabled for this session.
///
/// Linux VTs and dumb terminals do not implement xterm mouse protocols cleanly.
/// Capability is inferred from `$TERM` (and optional override env vars) rather
/// than probing the terminal, which is unreliable under non-interactive smoke.
fn mouse_capture_enabled() -> bool {
    match std::env::var("PUU_INSTALLER_MOUSE").ok().as_deref() {
        Some("0" | "false" | "no" | "off") => return false,
        Some("1" | "true" | "yes" | "on") => return true,
        _ => {}
    }
    let term = std::env::var("TERM").unwrap_or_default();
    if term.is_empty() || term == "dumb" || term == "linux" || term.starts_with("vt") {
        return false;
    }
    MOUSE_TERM_PREFIXES
        .iter()
        .any(|prefix| term.starts_with(prefix) || term.contains(&format!("-{prefix}")))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MapPanDirection {
    Left,
    Right,
}

impl From<MapPanDirection> for f64 {
    fn from(direction: MapPanDirection) -> Self {
        match direction {
            MapPanDirection::Left => -1.0,
            MapPanDirection::Right => 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScrollDirection {
    Up,
    Down,
}

impl ScrollDirection {
    const fn is_backwards(self) -> bool {
        matches!(self, Self::Up)
    }
}

impl From<ScrollDirection> for f64 {
    fn from(direction: ScrollDirection) -> Self {
        match direction {
            ScrollDirection::Up => -1.0,
            ScrollDirection::Down => 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HitAction {
    /// Select list/button index for the current route; second click activates.
    Select(usize),
    /// Modal No/Yes button.
    ModalSelect(ModalChoice),
    /// World-map horizontal pan.
    MapPan(MapPanDirection),
    /// Install log region (click re-enables follow mode).
    InstallFollow,
}

#[derive(Clone, Copy, Debug)]
struct HitTarget {
    rect: Rect,
    action: HitAction,
}

/// Content frame matching `render_screen` geometry (headers + footer + padding).
fn content_frame(width: u16, height: u16) -> (u16, u16, u16, u16) {
    let header = 2_u16;
    let footer = 1_u16;
    let pad_y = 1_u16;
    let pad_x = 2_u16;
    let y0 = header.saturating_add(pad_y);
    let y1 = height.saturating_sub(footer.saturating_add(pad_y)).max(y0);
    let x0 = pad_x;
    // Content is centered with max_width 180; for hit-testing the usable band
    // is the full padded width, which is close enough for list selection.
    let x1 = width.saturating_sub(pad_x).max(x0);
    (x0, y0, x1, y1)
}

fn list_row_targets(
    x0: u16,
    x1: u16,
    first_row: u16,
    count: usize,
    start_index: usize,
) -> Vec<HitTarget> {
    (0..count)
        .map(|offset| {
            let y = first_row.saturating_add(u16::try_from(offset).unwrap_or(u16::MAX));
            HitTarget {
                rect: Rect::from_corners(x0, y, x1, y.saturating_add(1)),
                action: HitAction::Select(start_index.saturating_add(offset)),
            }
        })
        .collect()
}

fn secure_boot_setup_on(state: &AppState) -> bool {
    state
        .firmware
        .as_ref()
        .is_some_and(firmware::SetupMode::is_on)
}

fn modal_dialog_rect(width: u16, height: u16) -> Rect {
    let max_dialog_width = width.saturating_sub(2).max(1);
    let dialog_width = width
        .saturating_mul(2)
        .saturating_div(3)
        .max(36)
        .min(max_dialog_width);
    let dialog_height = 11_u16.min(height.saturating_sub(2).max(1));
    let left = width.saturating_sub(dialog_width) / 2;
    let top = height.saturating_sub(dialog_height) / 2;
    Rect::new(left, top, dialog_width, dialog_height)
}

fn modal_button_rects(dialog: Rect) -> [Rect; 2] {
    let inner = Rect::new(
        dialog.x.saturating_add(2),
        dialog.y.saturating_add(1),
        dialog.width.saturating_sub(4),
        dialog.height.saturating_sub(2),
    );
    let y = inner.y.saturating_add(inner.height.saturating_sub(1));
    let well = 8_u16.min(inner.width.saturating_div(2).saturating_sub(1).max(1));
    let gap = 2_u16.min(inner.width.saturating_sub(well.saturating_mul(2)));
    let total = well.saturating_mul(2).saturating_add(gap);
    let start = inner
        .x
        .saturating_add(inner.width.saturating_sub(total) / 2);
    [
        Rect::new(start, y, well, 1),
        Rect::new(start.saturating_add(well).saturating_add(gap), y, well, 1),
    ]
}

fn wrap_dialog_text(text: &str, width: u16) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let limit = usize::from(width);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let used = usize::from(render::rendered_width(&current));
        let word_width = usize::from(render::rendered_width(word));
        if !current.is_empty() && used.saturating_add(1).saturating_add(word_width) > limit {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn modal_hit_targets(width: u16, height: u16) -> Vec<HitTarget> {
    let buttons = modal_button_rects(modal_dialog_rect(width, height));
    buttons
        .into_iter()
        .zip([ModalChoice::No, ModalChoice::Yes])
        .map(|(rect, choice)| HitTarget {
            rect,
            action: HitAction::ModalSelect(choice),
        })
        .collect()
}

fn timezone_hit_targets(
    state: &AppState,
    layout: LayoutMode,
    content: (u16, u16, u16, u16),
    list_x: (u16, u16),
) -> Vec<HitTarget> {
    let (cx0, cy0, cx1, cy1) = content;
    let (list_x0, list_x1) = list_x;
    let mut targets = Vec::new();
    let visible = timezone::visible_zones(state).len();
    if layout == LayoutMode::Compact {
        // Map on top (40%), list below (60%).
        let split = cy0 + (cy1 - cy0) * 2 / 5;
        let map_mid = list_x0 + (list_x1 - list_x0) / 2;
        targets.push(HitTarget {
            rect: Rect::from_corners(list_x0, cy0.saturating_add(1), map_mid, split),
            action: HitAction::MapPan(MapPanDirection::Left),
        });
        targets.push(HitTarget {
            rect: Rect::from_corners(map_mid, cy0.saturating_add(1), list_x1, split),
            action: HitAction::MapPan(MapPanDirection::Right),
        });
        // Filter row + zone rows start under the list panel title.
        let zones_top = split.saturating_add(4);
        targets.extend(list_row_targets(
            list_x0,
            list_x1,
            zones_top,
            visible,
            state.timezone.top,
        ));
    } else {
        // Map left (~66%), list right (~34%).
        let split = cx0 + (cx1 - cx0) * 2 / 3;
        let map_mid = cx0 + (split - cx0) / 2;
        targets.push(HitTarget {
            rect: Rect::from_corners(
                cx0.saturating_add(1),
                cy0.saturating_add(1),
                map_mid,
                cy1.saturating_sub(1),
            ),
            action: HitAction::MapPan(MapPanDirection::Left),
        });
        targets.push(HitTarget {
            rect: Rect::from_corners(
                map_mid,
                cy0.saturating_add(1),
                split.saturating_sub(1),
                cy1.saturating_sub(1),
            ),
            action: HitAction::MapPan(MapPanDirection::Right),
        });
        let zones_left = split.saturating_add(2);
        let zones_top = cy0.saturating_add(4); // list panel title + filter row
        targets.extend(list_row_targets(
            zones_left,
            cx1.saturating_sub(1).max(zones_left.saturating_add(1)),
            zones_top,
            visible,
            state.timezone.top,
        ));
    }
    targets
}

fn build_hit_targets(state: &AppState, terminal_size: (u16, u16)) -> Vec<HitTarget> {
    let (width, height) = terminal_size;
    let (cx0, cy0, cx1, cy1) = content_frame(width, height);
    if cx1 <= cx0 || cy1 <= cy0 {
        return Vec::new();
    }

    // Modal dialogs sit on top; only their buttons are clickable.
    if state.active_modal().is_some() {
        return modal_hit_targets(width, height);
    }

    // Panel title occupies two rows inside the bordered content area.
    let list_x0 = cx0.saturating_add(1);
    let list_x1 = cx1.saturating_sub(1).max(list_x0.saturating_add(1));
    let rows_top = cy0.saturating_add(3); // border + title block

    match state.route {
        Route::Welcome => list_row_targets(list_x0, list_x1, rows_top.saturating_add(2), 3, 0),
        Route::Target => {
            let heading = u16::from(
                !state.drives.is_empty() && layout_mode((width, height)) != LayoutMode::Compact,
            );
            list_row_targets(
                list_x0,
                list_x1,
                rows_top.saturating_add(heading),
                state.drives.len(),
                0,
            )
        }
        Route::Identity => list_row_targets(list_x0, list_x1, rows_top, 6, 0),
        Route::Timezone => timezone_hit_targets(
            state,
            layout_mode((width, height)),
            (cx0, cy0, cx1, cy1),
            (list_x0, list_x1),
        ),
        Route::SecureBoot => {
            let setup_on = secure_boot_setup_on(state);
            let count = SecureBootChoice::choices(setup_on).len();
            let first = if setup_on {
                // message, warning, then two buttons
                rows_top.saturating_add(2)
            } else {
                // message (+ optional reason), then three buttons
                let reason = state
                    .firmware
                    .as_ref()
                    .and_then(firmware::SetupMode::reason);
                rows_top.saturating_add(1 + u16::from(reason.is_some()))
            };
            list_row_targets(list_x0, list_x1, first, count, 0)
        }
        Route::Review => {
            // warning, blank, 4 summary lines, then two buttons
            list_row_targets(list_x0, list_x1, rows_top.saturating_add(6), 2, 0)
        }
        Route::Install => {
            // Whole content region re-enables log follow on click.
            vec![HitTarget {
                rect: Rect::from_corners(cx0, cy0, cx1, cy1),
                action: HitAction::InstallFollow,
            }]
        }
        Route::Reboot => {
            // status lines then two buttons
            list_row_targets(list_x0, list_x1, rows_top.saturating_add(3), 2, 0)
        }
    }
}

fn hit_test(targets: &[HitTarget], column: u16, row: u16) -> Option<HitAction> {
    targets
        .iter()
        .rev()
        .find(|target| target.rect.contains(column, row))
        .map(|target| target.action)
}

fn activate_enter(state: &mut AppState) -> Vec<Effect> {
    update_key(state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
}

fn scroll_mouse(state: &mut AppState, direction: ScrollDirection) {
    match state.route {
        Route::Install => {
            if direction.is_backwards() {
                state.scroll.install_log =
                    (state.scroll.install_log + 10).min(state.install.logs.len().saturating_sub(1));
                state.install.follow_logs = false;
            } else {
                state.scroll.install_log = state.scroll.install_log.saturating_sub(10);
                state.install.follow_logs = state.scroll.install_log == 0;
            }
        }
        Route::Timezone => {
            // Scroll pans the map; list selection uses clicks.
            timezone::pan_map(state, direction.into(), true);
        }
        Route::Welcome => {
            state.selection.welcome = if direction.is_backwards() {
                state.selection.welcome.previous()
            } else {
                state.selection.welcome.next()
            };
        }
        Route::Target => {
            if state.drives.is_empty() {
                return;
            }
            let mut selected = state.selection.target.unwrap_or(0);
            cycle(&mut selected, state.drives.len(), direction.is_backwards());
            state.selection.target = Some(selected);
        }
        Route::Identity => {
            if let Some(draft) = state.identity.as_mut() {
                let next = if direction.is_backwards() {
                    if draft.field == IdentityField::Language {
                        IdentityField::Language
                    } else {
                        draft.field.previous()
                    }
                } else {
                    draft.field.next()
                };
                focus_identity_field(draft, next);
            }
        }
        Route::SecureBoot => {
            state.selection.secure_boot = state
                .selection
                .secure_boot
                .cycle(secure_boot_setup_on(state), direction.is_backwards());
        }
        Route::Review => state.selection.review = state.selection.review.toggle(),
        Route::Reboot => state.selection.reboot = state.selection.reboot.toggle(),
    }
}

fn click_select(state: &mut AppState, index: usize) -> Vec<Effect> {
    match state.route {
        Route::Welcome => {
            let choice = WelcomeChoice::from_hit_index(index)
                .or_else(|| WelcomeChoice::ALL.last().copied())
                .unwrap_or(WelcomeChoice::Reboot);
            if state.selection.welcome == choice {
                return activate_enter(state);
            }
            state.selection.welcome = choice;
        }
        Route::Target => {
            if state.drives.is_empty() || index >= state.drives.len() {
                return Vec::new();
            }
            if state.selection.target == Some(index) {
                return activate_enter(state);
            }
            state.selection.target = Some(index);
        }
        Route::Identity => {
            if let Some(draft) = state.identity.as_mut() {
                focus_identity_field(
                    draft,
                    IdentityField::from_hit_index(index)
                        .or_else(|| IdentityField::ALL.last().copied())
                        .unwrap_or(IdentityField::Shell),
                );
            }
        }
        Route::Timezone => {
            if index >= state.timezone.filtered.len() {
                return Vec::new();
            }
            if state.timezone.selected == index {
                return timezone::update_key(
                    state,
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                );
            }
            timezone::select_index(state, index);
        }
        Route::SecureBoot => {
            let Some(choice) = SecureBootChoice::from_hit_index(index, secure_boot_setup_on(state))
            else {
                return Vec::new();
            };
            if state.selection.secure_boot == choice {
                return activate_enter(state);
            }
            state.selection.secure_boot = choice;
        }
        Route::Review => {
            let Some(choice) = ReviewChoice::from_hit_index(index) else {
                return Vec::new();
            };
            if state.selection.review == choice {
                return activate_enter(state);
            }
            state.selection.review = choice;
        }
        Route::Reboot => {
            let Some(choice) = RebootChoice::from_hit_index(index) else {
                return Vec::new();
            };
            if state.selection.reboot == choice {
                return activate_enter(state);
            }
            state.selection.reboot = choice;
        }
        Route::Install => {}
    }
    Vec::new()
}

fn update_mouse(
    state: &mut AppState,
    terminal_size: (u16, u16),
    column: u16,
    row: u16,
    kind: MouseKind,
) -> Vec<Effect> {
    match kind {
        MouseKind::ScrollUp => {
            scroll_mouse(state, ScrollDirection::Up);
            return Vec::new();
        }
        MouseKind::ScrollDown => {
            scroll_mouse(state, ScrollDirection::Down);
            return Vec::new();
        }
        MouseKind::LeftDown => {}
    }

    let targets = build_hit_targets(state, terminal_size);
    let Some(action) = hit_test(&targets, column, row) else {
        return Vec::new();
    };
    match action {
        HitAction::ModalSelect(choice) => {
            state.selection.modal = choice;
            activate_enter(state)
        }
        HitAction::MapPan(direction) => {
            timezone::pan_map(state, direction.into(), false);
            Vec::new()
        }
        HitAction::InstallFollow => {
            state.scroll.install_log = 0;
            state.install.follow_logs = true;
            Vec::new()
        }
        HitAction::Select(index) => click_select(state, index),
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
            state.selection.modal = state.selection.modal.toggle();
        }
        KeyCode::Esc => {
            state.pop_modal();
            state.selection.modal = ModalChoice::No;
        }
        KeyCode::Enter => {
            let confirmed = state.selection.modal.is_yes();
            state.pop_modal();
            state.selection.modal = ModalChoice::No;
            match (modal, confirmed) {
                (Modal::WipeConfirmation, true) => state.route = Route::Identity,
                (Modal::RebootConfirmation | Modal::SetupModeRequired, true) => {
                    return vec![Effect::Reboot];
                }
                (Modal::Cancellation, true) => return vec![Effect::CancelPipeline],
                _ => {}
            }
        }
        _ => {}
    }
    Vec::new()
}

fn identity_field_mut(draft: &mut IdentityDraft) -> Option<&mut String> {
    match draft.field {
        IdentityField::Hostname => Some(&mut draft.hostname),
        IdentityField::Username => Some(&mut draft.username),
        IdentityField::Password => Some(&mut *draft.password),
        IdentityField::RepeatedPassword => Some(&mut *draft.repeated_password),
        IdentityField::Language | IdentityField::Shell => None,
    }
}

fn identity_field(draft: &IdentityDraft) -> Option<&str> {
    match draft.field {
        IdentityField::Hostname => Some(draft.hostname.as_str()),
        IdentityField::Username => Some(draft.username.as_str()),
        IdentityField::Password => Some(draft.password.as_str()),
        IdentityField::RepeatedPassword => Some(draft.repeated_password.as_str()),
        IdentityField::Language | IdentityField::Shell => None,
    }
}

fn identity_field_len(draft: &IdentityDraft) -> usize {
    identity_field(draft).map_or(0, |value| value.chars().count())
}

fn clamp_identity_cursor(draft: &mut IdentityDraft) {
    draft.cursor = draft.cursor.min(identity_field_len(draft));
}

fn focus_identity_field(draft: &mut IdentityDraft, field: IdentityField) {
    draft.field = field;
    draft.cursor = identity_field_len(draft);
}

fn update_identity_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    let Some(draft) = state.identity.as_mut() else {
        return Vec::new();
    };
    match key.code {
        KeyCode::Esc => state.route = Route::Target,
        KeyCode::Tab | KeyCode::Down => {
            focus_identity_field(draft, draft.field.next());
        }
        KeyCode::Up => {
            focus_identity_field(draft, draft.field.previous());
        }
        KeyCode::Home if identity_field(draft).is_some() => draft.cursor = 0,
        KeyCode::End if identity_field(draft).is_some() => {
            draft.cursor = identity_field_len(draft);
        }
        KeyCode::Left if draft.field == IdentityField::Language => {
            draft.language = draft.language.cycle(true);
        }
        KeyCode::Right if draft.field == IdentityField::Language => {
            draft.language = draft.language.cycle(false);
        }
        KeyCode::Left | KeyCode::Right if draft.field == IdentityField::Shell => {
            draft.shell = draft.shell.toggle();
        }
        KeyCode::Left if identity_field(draft).is_some() => {
            draft.cursor = draft.cursor.saturating_sub(1);
        }
        KeyCode::Right if identity_field(draft).is_some() => {
            let len = identity_field_len(draft);
            draft.cursor = (draft.cursor + 1).min(len);
        }
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            draft.reveal_password = !draft.reveal_password;
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(field) = identity_field_mut(draft) {
                field.clear();
            }
            draft.cursor = 0;
        }
        KeyCode::Delete => {
            let cursor = draft.cursor;
            if let Some(field) = identity_field_mut(draft) {
                let mut chars = field.chars().collect::<Vec<_>>();
                if cursor < chars.len() {
                    chars.remove(cursor);
                    *field = chars.into_iter().collect();
                }
            }
        }
        KeyCode::Backspace => {
            if draft.cursor > 0 {
                let cursor = draft.cursor - 1;
                if let Some(field) = identity_field_mut(draft) {
                    let mut chars = field.chars().collect::<Vec<_>>();
                    if cursor < chars.len() {
                        chars.remove(cursor);
                        *field = chars.into_iter().collect();
                        draft.cursor = cursor;
                    }
                }
            }
        }
        KeyCode::Char(ch)
            if ch.is_ascii_graphic() && !key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            let max = match draft.field {
                IdentityField::Hostname => 253,
                IdentityField::Username => 32,
                _ => usize::MAX,
            };
            let cursor = draft.cursor;
            if let Some(field) = identity_field_mut(draft) {
                let len = field.chars().count();
                if len < max {
                    let cursor = cursor.min(len);
                    let mut chars = field.chars().collect::<Vec<_>>();
                    chars.insert(cursor, ch);
                    *field = chars.into_iter().collect();
                    draft.cursor = cursor + 1;
                }
            }
        }
        KeyCode::Enter => {
            return vec![Effect::ValidateForm];
        }
        _ => {}
    }
    clamp_identity_cursor(draft);
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
        if state.route == Route::Install && state.install.status.is_running() {
            if !state.install.committed {
                state.push_modal(Modal::Cancellation);
            }
            return Vec::new();
        }
        return vec![Effect::Quit];
    }
    match state.route {
        Route::Welcome => match key.code {
            KeyCode::Up => {
                state.selection.welcome = state.selection.welcome.previous();
            }
            KeyCode::Down | KeyCode::Tab => {
                state.selection.welcome = state.selection.welcome.next();
            }
            KeyCode::Esc => return vec![Effect::Quit],
            KeyCode::Enter => match state.selection.welcome {
                WelcomeChoice::Install => {
                    state.reset_flow();
                    return vec![Effect::RefreshDrives];
                }
                WelcomeChoice::Shell => {
                    return vec![Effect::RunShell];
                }
                WelcomeChoice::Reboot => state.push_modal(Modal::RebootConfirmation),
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
                    && drive.busy_reason.is_none()
                {
                    return vec![Effect::SelectDrive { index: drive.index }];
                }
            }
            _ => {}
        },
        Route::Identity => return update_identity_key(state, key),
        Route::Timezone => return timezone::update_key(state, key),
        Route::SecureBoot => {
            let setup_on = secure_boot_setup_on(state);
            match key.code {
                KeyCode::Esc => state.route = Route::Timezone,
                KeyCode::Left => {
                    state.selection.secure_boot = state.selection.secure_boot.cycle(setup_on, true);
                }
                KeyCode::Right | KeyCode::Tab => {
                    state.selection.secure_boot =
                        state.selection.secure_boot.cycle(setup_on, false);
                }
                KeyCode::Enter => {
                    if !SecureBootChoice::choices(setup_on).contains(&state.selection.secure_boot) {
                        return Vec::new();
                    }
                    match state.selection.secure_boot {
                        SecureBootChoice::Back => state.route = Route::Timezone,
                        SecureBootChoice::Enable => {
                            if setup_on {
                                state.route = Route::Review;
                                return vec![Effect::SetSecureBoot(SecureBootMode::Enroll)];
                            }
                            state.push_modal(Modal::SetupModeRequired);
                        }
                        SecureBootChoice::Continue => {
                            state.route = Route::Review;
                            return vec![Effect::SetSecureBoot(SecureBootMode::Skip)];
                        }
                        SecureBootChoice::Reboot => return vec![Effect::Reboot],
                    }
                }
                _ => {}
            }
        }
        Route::Review => match key.code {
            KeyCode::Esc => state.route = Route::SecureBoot,
            KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                state.selection.review = state.selection.review.toggle();
            }
            KeyCode::Enter if state.selection.review == ReviewChoice::Back => {
                state.route = Route::SecureBoot;
            }
            KeyCode::Enter => {
                state.route = Route::Install;
                state.install.step_states = vec![InstallStepState::Pending; pipeline::STEPS.len()];
                return vec![Effect::StartInstall];
            }
            _ => {}
        },
        Route::Install => match key.code {
            KeyCode::Esc if !state.install.committed && !state.install.status.is_succeeded() => {
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
            KeyCode::Enter if state.install.status.is_succeeded() => {
                state.route = Route::Reboot;
                state.reboot.remaining_seconds = Some(REBOOT_DELAY_SECONDS);
                state.reboot.automatic = true;
            }
            KeyCode::Enter if state.install.status.is_failed() => state.route = Route::Welcome,
            _ => {}
        },
        Route::Reboot => {
            state.reboot.automatic = false;
            match key.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                    state.selection.reboot = state.selection.reboot.toggle();
                }
                KeyCode::Enter if state.selection.reboot == RebootChoice::Shell => {
                    return vec![Effect::RunShell];
                }
                KeyCode::Enter => return vec![Effect::Reboot],
                _ => {}
            }
        }
    }
    Vec::new()
}

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
                .any(|drive| drive.index == index && drive.busy_reason.is_none())
            {
                state.push_modal(Modal::WipeConfirmation);
            }
        }
        Message::FormValidated { error } => {
            state.validation_error = error.map(|text| sanitize_text(&text));
            if state.validation_error.is_none() {
                state.route = Route::Timezone;
            }
        }
        // Mouse is handled in dispatch where terminal geometry is available.
        Message::Mouse { .. } => {}
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
            state.install.status = match error {
                Some(reason) => InstallStatus::Failed { reason },
                None => InstallStatus::Succeeded,
            };
            if let Some(step) = at.and_then(|index| state.install.step_states.get_mut(index)) {
                *step = InstallStepState::Failed;
            }
            if state.install.status.is_failed() {
                state.push_modal(Modal::Failure);
            }
        }
        Message::FirmwareReady { status } => {
            state.selection.secure_boot = SecureBootChoice::default_for(status.is_on());
            state.firmware = Some(status);
        }
    }
    Vec::new()
}

fn validate_identity(draft: &IdentityDraft) -> Option<String> {
    if draft
        .hostname
        .trim()
        .parse::<crate::engine::config::Hostname>()
        .is_err()
    {
        return Some("Invalid hostname".to_string());
    }
    let username = draft.username.trim();
    if username.is_empty() {
        return Some("Username must not be empty".to_string());
    }
    if username.parse::<crate::engine::config::Username>().is_err() {
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
                .map(|(index, device)| {
                    let path_text = sanitize_text(&device.path.to_string_lossy());
                    let model = sanitize_text(device.model.as_deref().unwrap_or("(unknown)"));
                    let size = sanitize_text(
                        &device
                            .size
                            .map_or_else(|| "-".into(), crate::engine::util::human_size),
                    );
                    let transport = sanitize_text(device.transport.as_deref().unwrap_or("-"));
                    let label = sanitize_text(&format!("{path_text} - {model}, {size}"));
                    DriveInfo {
                        index,
                        path: device.path,
                        label,
                        size,
                        model,
                        transport,
                        busy_reason: device.busy_reason,
                    }
                })
                .collect()
        },
    )
}

fn terminal_size() -> (u16, u16) {
    render::screen_size().map_or((80, 24), |size| (size.width, size.height))
}

struct UiController {
    ctx: crate::engine::pipeline::Context,
    state: AppState,
    theme: Theme,
    task: Option<InstallTask>,
    drive_rx: Option<mpsc::Receiver<Message>>,
    trap: Option<Trap>,
    last_countdown: Instant,
    last_animation: Instant,
    last_visual_pulse: Instant,
    /// Last solar minute baked into the timezone map terminator.
    last_map_sun_minute: Option<u64>,
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
            let status = match firmware::detect_setup_mode() {
                firmware::SetupMode::Unknown { reason } => firmware::SetupMode::Unknown {
                    reason: sanitize_text(&reason),
                },
                status => status,
            };
            Some(Message::FirmwareReady { status })
        }
        Effect::SelectDrive { index } => {
            if let Some(drive) = controller
                .state
                .drives
                .iter()
                .find(|drive| drive.index == index && drive.busy_reason.is_none())
            {
                controller.ctx.drive.clone_from(&drive.path);
                controller.ctx.drive_label = Some(drive.label.clone());
            }
            Some(Message::DriveSelected { index })
        }
        Effect::SelectTimezone { name } => {
            if let Ok(timezone) = name.parse() {
                controller.ctx.config.identity.timezone = timezone;
            }
            None
        }
        Effect::SetSecureBoot(mode) => {
            controller.ctx.secure_boot = mode;
            None
        }
        Effect::ValidateForm => {
            let error = controller
                .state
                .identity
                .as_ref()
                .and_then(validate_identity);
            if error.is_none()
                && let Some(draft) = controller.state.identity.as_ref()
            {
                if let Ok(hostname) = draft.hostname.trim().parse() {
                    controller.ctx.config.identity.hostname = hostname;
                }
                if let Ok(locale) = draft.language.locale().parse() {
                    controller.ctx.config.identity.locale = locale;
                }
                if let Ok(username) = draft.username.trim().parse() {
                    controller.ctx.username = Some(username);
                }
                controller.ctx.user_password.clone_from(&draft.password);
                controller.ctx.user_shell = draft.shell;
            }
            Some(Message::FormValidated { error })
        }
        Effect::StartInstall => {
            controller.state.install.started = Some(Instant::now());
            controller.task = Some(InstallTask::start(&controller.ctx));
            controller.ctx.user_password.zeroize();
            controller.ctx.k3s_token.zeroize();
            if let Some(draft) = controller.state.identity.as_mut() {
                draft.password.zeroize();
                draft.repeated_password.zeroize();
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
        Effect::RunShell => {
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
    }
}

fn dispatch(controller: &mut UiController, message: Message) {
    let mut messages = vec![message];
    while let Some(message) = messages.pop() {
        let effects = match message {
            Message::Mouse { column, row, kind } => update_mouse(
                &mut controller.state,
                controller.terminal_size,
                column,
                row,
                kind,
            ),
            other => update(&mut controller.state, other),
        };
        for effect in effects {
            if let Some(message) = execute_effect(controller, effect) {
                messages.push(message);
            }
        }
    }
}

fn prepare_state(controller: &mut UiController) {
    controller.state.route = Route::Welcome;
    controller.state.selection.reboot = RebootChoice::Now;
    controller.state.selection.secure_boot = SecureBootChoice::Continue;
    controller.state.selection.review = ReviewChoice::Start;
    controller.state.install.follow_logs = true;
    if controller.state.identity.is_none() {
        controller.state.identity = Some(IdentityDraft {
            language: LocaleChoice::default(),
            hostname: controller.ctx.config.identity.hostname.to_string(),
            username: controller
                .ctx
                .username
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            password: controller.ctx.user_password.clone(),
            repeated_password: controller.ctx.user_password.clone(),
            shell: controller.ctx.user_shell,
            field: IdentityField::Language,
            cursor: 0,
            reveal_password: false,
        });
    }
    if controller.state.timezone.filtered.is_empty() {
        timezone::prepare(
            &mut controller.state,
            controller.ctx.config.identity.timezone.as_str(),
        );
    }
}

/// High-frequency install stream messages that may be batched into one frame.
fn is_coalesced_install_message(message: &Message) -> bool {
    matches!(message, Message::Log(_) | Message::InstallProgress { .. })
}

fn service(controller: &mut UiController) -> bool {
    if controller.trap.is_some() {
        return false;
    }
    let now = Instant::now();
    let animation_elapsed = now.saturating_duration_since(controller.last_animation);
    controller.last_animation = now;
    let mut dirty = false;
    let mut coalesced_pending = false;
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
    for message in messages {
        if is_coalesced_install_message(&message) {
            coalesced_pending = true;
        } else {
            dirty = true;
        }
        dispatch(controller, message);
    }
    let install_running =
        controller.state.route == Route::Install && controller.state.install.status.is_running();
    if install_running {
        // Spinner and coalesced log/progress share one redraw budget.
        // Immediate messages already set dirty above and redraw now;
        // pure coalesced traffic waits for the pulse.
        let pulse_due =
            now.saturating_duration_since(controller.last_visual_pulse) >= INSTALL_RENDER_INTERVAL;
        if pulse_due {
            dirty = true;
        }
        if dirty {
            // Any install-frame paint restarts the shared budget so we do not
            // double-paint immediately after an urgent event.
            controller.last_visual_pulse = now;
        }
    } else if coalesced_pending {
        dirty = true;
    }
    if controller.state.route == Route::Timezone {
        dirty |= timezone::tick(&mut controller.state, animation_elapsed);
        let sun_minute = timezone::map_sun_minute();
        if controller.last_map_sun_minute != Some(sun_minute) {
            controller.last_map_sun_minute = Some(sun_minute);
            dirty = true;
        }
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

fn poll_interval(controller: &UiController) -> Duration {
    if controller.state.route == Route::Timezone && timezone::map_is_panning(&controller.state) {
        MAP_FRAME_INTERVAL
    } else if controller.state.route == Route::Install
        && controller.state.install.status.is_running()
    {
        // Drain the install event queue frequently; redraws are budgeted
        // separately at INSTALL_RENDER_INTERVAL.
        ACTIVE_POLL_INTERVAL
    } else if controller.task.is_some()
        || controller.drive_rx.is_some()
        || (controller.state.route == Route::Reboot && controller.state.reboot.automatic)
    {
        ACTIVE_POLL_INTERVAL
    } else {
        IDLE_POLL_INTERVAL
    }
}

fn overall_install_progress(state: &AppState) -> u16 {
    if state.install.status.is_succeeded() {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LayoutMode {
    Compact,
    Standard,
    Wide,
}

fn layout_mode((width, height): (u16, u16)) -> LayoutMode {
    if width < 80 || height < 24 {
        LayoutMode::Compact
    } else if width >= 120 && height >= 28 {
        LayoutMode::Wide
    } else {
        LayoutMode::Standard
    }
}

// ---------------------------------------------------------------------------
// Frame painters. Every route draws directly into the offscreen cell buffer;
// the diff terminal in `render` blits the result.
// ---------------------------------------------------------------------------

/// Interior body rect of a panel: below the border and two title rows.
fn panel_body(rect: Rect) -> Rect {
    Rect::new(
        rect.x.saturating_add(1),
        rect.y.saturating_add(3),
        rect.width.saturating_sub(2),
        rect.height.saturating_sub(4),
    )
}

fn draw_panel(frame: &mut Frame, theme: Theme, rect: Rect, title: &str) {
    if rect.width >= 2 && rect.height >= 2 {
        frame.render_widget(Border::style(theme.border()), rect);
    }
    if !title.is_empty() && rect.width > 2 && rect.height > 1 {
        let label = render::rendered_width(title);
        let width = label.min(rect.width.saturating_sub(2));
        frame.render_widget(
            Paragraph::new(title.to_string()).style(theme.accent().bold()),
            Rect::new(rect.x + 1, rect.y + 1, width, 1),
        );
    }
}

fn draw_selected_row(
    frame: &mut Frame,
    theme: Theme,
    x: u16,
    y: u16,
    width: u16,
    content: &str,
    selected: bool,
) {
    let marker = if selected { ">" } else { " " };
    let marker_style = if selected {
        theme.accent().bold()
    } else {
        theme.muted()
    };
    let text_style = if selected {
        theme.text().bold()
    } else {
        theme.muted()
    };
    frame.render_widget(
        Paragraph::new(Line::new(vec![
            Span::styled(format!(" {marker} "), marker_style),
            Span::styled(content.to_string(), text_style),
        ])),
        Rect::new(x, y, width, 1),
    );
}

fn draw_text_row(
    frame: &mut Frame,
    x: u16,
    y: u16,
    width: u16,
    content: &str,
    style: Style,
    weight: Weight,
) {
    let style = match weight {
        Weight::Bold => style.bold(),
        Weight::Light => style.dim(),
        Weight::Normal => style,
    };
    frame.render_widget(
        Paragraph::new(content.to_string()).style(style),
        Rect::new(x, y, width, 1),
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Weight {
    Normal,
    Light,
    Bold,
}

#[expect(
    clippy::too_many_arguments,
    reason = "row painter mirrors the field row arguments"
)]
fn draw_choice_row(
    frame: &mut Frame,
    theme: Theme,
    x: u16,
    y: u16,
    width: u16,
    label: &str,
    value: &str,
    selected: bool,
) {
    let body = if selected {
        format!("{label:<14}< {value} >")
    } else {
        format!("{label:<14}{value}")
    };
    draw_selected_row(frame, theme, x, y, width, &body, selected);
}

fn draw_value_well(
    frame: &mut Frame,
    theme: Theme,
    well: Rect,
    value: &str,
    cursor: usize,
    focused: bool,
    placeholder: &str,
) {
    if well.width == 0 {
        return;
    }
    frame.render_widget(Block::default().style(theme.well()), well);
    let cols = usize::from(well.width);
    let cursor = cursor.min(value.chars().count());
    let usable = if focused {
        cols.saturating_sub(1).max(1)
    } else {
        cols
    };
    let scroll = if focused {
        cursor.saturating_sub(usable)
    } else {
        0
    };
    let visible: String = value.chars().skip(scroll).take(usable).collect();
    if visible.is_empty() {
        if focused && !placeholder.is_empty() {
            frame.render_widget(
                Paragraph::new(placeholder).style(theme.well_muted().dim()),
                well,
            );
        }
    } else {
        let style = if focused {
            theme.well_text().bold()
        } else {
            theme.well_text()
        };
        frame.render_widget(Paragraph::new(visible).style(style), well);
    }
    if focused && let Ok(col) = u16::try_from(cursor.saturating_sub(scroll)) {
        let cursor_x = well.x.saturating_add(col);
        if cursor_x < well.x.saturating_add(well.width) {
            frame.set_cursor_position((cursor_x, well.y));
            frame.set_cursor_style(CursorStyle::SteadyBar);
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "row painter mirrors the field row arguments"
)]
fn draw_text_input(
    frame: &mut Frame,
    theme: Theme,
    x: u16,
    y: u16,
    width: u16,
    label: &str,
    value: &str,
    selected: bool,
    cursor: usize,
    empty_hint: &str,
) {
    let marker = if selected { ">" } else { " " };
    let prefix = format!(" {marker} {label:<14}");
    let prefix_cols = render::rendered_width(&prefix);
    let prefix_style = if selected {
        theme.accent().bold()
    } else {
        theme.muted()
    };
    let prefix_width = prefix_cols.min(width);
    if prefix_width > 0 {
        frame.render_widget(
            Paragraph::new(prefix).style(prefix_style),
            Rect::new(x, y, prefix_width, 1),
        );
    }
    let well_width = width.saturating_sub(prefix_cols).min(32);
    if well_width > 0 {
        draw_value_well(
            frame,
            theme,
            Rect::new(x.saturating_add(prefix_cols), y, well_width, 1),
            value,
            cursor,
            selected,
            empty_hint,
        );
    }
}

fn draw_filter_input(
    frame: &mut Frame,
    theme: Theme,
    x: u16,
    y: u16,
    width: u16,
    value: &str,
    focused: bool,
) {
    let prefix = "  Filter  ";
    let prefix_cols = render::rendered_width(prefix);
    let prefix_width = prefix_cols.min(width);
    if prefix_width > 0 {
        frame.render_widget(
            Paragraph::new(prefix).style(theme.accent().bold()),
            Rect::new(x, y, prefix_width, 1),
        );
    }
    let well_width = width.saturating_sub(prefix_cols).min(32);
    if well_width > 0 {
        draw_value_well(
            frame,
            theme,
            Rect::new(x.saturating_add(prefix_cols), y, well_width, 1),
            value,
            value.chars().count(),
            focused,
            "Type to filter...",
        );
    }
}

fn draw_zone_row(
    frame: &mut Frame,
    theme: Theme,
    x: u16,
    y: u16,
    width: u16,
    zone: &timezone::VisibleZone,
) {
    let marker = if zone.selected { ">" } else { " " };
    let marker_style = if zone.selected {
        theme.accent().bold()
    } else {
        theme.muted()
    };
    let base = if zone.selected {
        theme.text().bold()
    } else {
        theme.muted()
    };
    let match_style = theme.accent().bold();
    let chars = zone.name.chars().collect::<Vec<_>>();
    let mut spans = vec![Span::styled(format!(" {marker} "), marker_style)];
    if let Some((start, end)) = zone.match_range {
        let start = start.min(chars.len());
        let end = end.min(chars.len()).max(start);
        let before = chars[..start].iter().collect::<String>();
        let matched = chars[start..end].iter().collect::<String>();
        let after = chars[end..].iter().collect::<String>();
        if !before.is_empty() {
            spans.push(Span::styled(before, base));
        }
        if !matched.is_empty() {
            spans.push(Span::styled(matched, match_style));
        }
        if !after.is_empty() {
            spans.push(Span::styled(after, base));
        }
    } else {
        spans.push(Span::styled(zone.name.to_string(), base));
    }
    frame.render_widget(Paragraph::new(Line::new(spans)), Rect::new(x, y, width, 1));
}

#[expect(
    clippy::too_many_arguments,
    reason = "bar painter keeps geometry and fill colour together"
)]
fn draw_progress(
    frame: &mut Frame,
    theme: Theme,
    x: u16,
    y: u16,
    width: u16,
    label: &str,
    percent: u16,
    fill: Style,
) {
    let bar_width = usize::from(width.saturating_sub(24)).clamp(8, 52);
    let percent = percent.min(100);
    let filled = usize::from(percent) * bar_width / 100;
    let spans = vec![
        Span::styled(format!("{label:<8} ["), theme.text().bold()),
        Span::styled("█".repeat(filled), fill),
        Span::styled("░".repeat(bar_width - filled), theme.track()),
        Span::styled(format!("] {percent:3}%"), theme.text()),
    ];
    frame.render_widget(
        Paragraph::new(Line::new(spans)).style(theme.canvas()),
        Rect::new(x, y, width, 1),
    );
}

fn truncate_display(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    if max_chars <= 3 {
        return text.chars().take(max_chars).collect();
    }
    let mut out: String = text.chars().take(max_chars - 3).collect();
    out.push_str("...");
    out
}

fn draw_zone_details(
    frame: &mut Frame,
    theme: Theme,
    x: u16,
    y: u16,
    width: u16,
    details: &timezone::ZoneDetails,
    filtering: bool,
) {
    let mut row = y;
    let count = if filtering {
        format!("{}/{} matches", details.match_index, details.match_total)
    } else {
        format!("{}/{}", details.match_index, details.match_total)
    };
    row = row.saturating_add(1);
    draw_text_row(
        frame,
        x,
        row,
        width,
        &truncate_display(&sanitize_text(&details.name), 24),
        theme.accent(),
        Weight::Bold,
    );
    row = row.saturating_add(1);
    draw_text_row(frame, x, row, width, &count, theme.muted(), Weight::Normal);
    row = row.saturating_add(1);
    if let Some((lat, lon)) = details.coords {
        draw_text_row(
            frame,
            x,
            row,
            width,
            &format!("{lat:+07.2} {lon:+08.2}"),
            theme.text(),
            Weight::Normal,
        );
    } else {
        draw_text_row(
            frame,
            x,
            row,
            width,
            "No map coordinates",
            theme.muted(),
            Weight::Normal,
        );
    }
    row = row.saturating_add(1);
    if !details.countries.is_empty() {
        draw_text_row(
            frame,
            x,
            row,
            width,
            &truncate_display(&sanitize_text(&format!("CC {}", details.countries)), 24),
            theme.muted(),
            Weight::Normal,
        );
        row = row.saturating_add(1);
    }
    if !details.comment.is_empty() {
        draw_text_row(
            frame,
            x,
            row,
            width,
            &truncate_display(&sanitize_text(&details.comment), 24),
            theme.muted(),
            Weight::Normal,
        );
    }
}

fn welcome_content(frame: &mut Frame, theme: Theme, rect: Rect, state: &AppState) {
    draw_panel(frame, theme, rect, "Get started");
    let body = panel_body(rect);
    draw_text_row(
        frame,
        body.x,
        body.y,
        body.width,
        "Install a bootc system with guided disk, identity, and security setup.",
        theme.muted(),
        Weight::Normal,
    );
    let labels = [
        (WelcomeChoice::Install, "Install system"),
        (WelcomeChoice::Shell, "Open recovery shell"),
        (WelcomeChoice::Reboot, "Reboot machine"),
    ];
    for (offset, (choice, label)) in labels.into_iter().enumerate() {
        draw_selected_row(
            frame,
            theme,
            body.x,
            body.y + 2 + offset as u16,
            body.width,
            label,
            state.selection.welcome == choice,
        );
    }
}

fn target_content(frame: &mut Frame, rect: Rect, controller: &UiController, layout: LayoutMode) {
    let state = &controller.state;
    let theme = controller.theme;
    draw_panel(frame, theme, rect, "Target disks");
    let body = panel_body(rect);
    let mut row = body.y;
    let scanning = controller.drive_rx.is_some() && state.drives.is_empty();
    if scanning {
        draw_text_row(
            frame,
            body.x,
            row,
            body.width,
            "Scanning for target disks...",
            theme.text(),
            Weight::Bold,
        );
        return;
    }
    if state.drives.is_empty() {
        draw_text_row(
            frame,
            body.x,
            row,
            body.width,
            "No target disks detected",
            theme.text(),
            Weight::Bold,
        );
        return;
    }
    if layout != LayoutMode::Compact {
        draw_text_row(
            frame,
            body.x,
            row,
            body.width,
            &format!(
                "{:<18} {:<10} {:<8} {:<20} STATUS",
                "DEVICE", "SIZE", "BUS", "MODEL"
            ),
            theme.muted(),
            Weight::Bold,
        );
        row = row.saturating_add(1);
    }
    for (index, drive) in state.drives.iter().enumerate() {
        let status = if drive.busy_reason.is_none() {
            "available"
        } else {
            "BUSY"
        };
        let label = if layout == LayoutMode::Compact {
            format!(
                "{:<18} {:<10} {status}",
                sanitize_text(&drive.path.to_string_lossy()),
                drive.size
            )
        } else {
            format!(
                "{:<18} {:<10} {:<8} {:<20} {status}",
                sanitize_text(&drive.path.to_string_lossy()),
                drive.size,
                drive.transport,
                drive.model.chars().take(20).collect::<String>()
            )
        };
        draw_selected_row(
            frame,
            theme,
            body.x,
            row,
            body.width,
            &label,
            state.selection.target == Some(index),
        );
        row = row.saturating_add(1);
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one painter per identity field keeps layout local"
)]
fn identity_content(frame: &mut Frame, theme: Theme, rect: Rect, state: &AppState) {
    draw_panel(frame, theme, rect, "User account");
    let body = panel_body(rect);
    let Some(draft) = state.identity.as_ref() else {
        draw_text_row(
            frame,
            body.x,
            body.y,
            body.width,
            "Loading identity...",
            theme.text(),
            Weight::Normal,
        );
        return;
    };
    let password = if draft.reveal_password {
        sanitize_text(&draft.password)
    } else {
        "*".repeat(draft.password.chars().count())
    };
    let repeated = if draft.reveal_password {
        sanitize_text(&draft.repeated_password)
    } else {
        "*".repeat(draft.repeated_password.chars().count())
    };
    let language = draft.language.label();
    let shell = draft.shell.label();
    let mut row = body.y;
    draw_choice_row(
        frame,
        theme,
        body.x,
        row,
        body.width,
        "Language",
        language,
        draft.field == IdentityField::Language,
    );
    row = row.saturating_add(1);
    draw_text_input(
        frame,
        theme,
        body.x,
        row,
        body.width,
        "Hostname",
        &draft.hostname,
        draft.field == IdentityField::Hostname && state.active_modal().is_none(),
        draft.cursor,
        "required",
    );
    row = row.saturating_add(1);
    draw_text_input(
        frame,
        theme,
        body.x,
        row,
        body.width,
        "Username",
        &draft.username,
        draft.field == IdentityField::Username && state.active_modal().is_none(),
        draft.cursor,
        "required",
    );
    row = row.saturating_add(1);
    draw_text_input(
        frame,
        theme,
        body.x,
        row,
        body.width,
        "Password",
        &password,
        draft.field == IdentityField::Password && state.active_modal().is_none(),
        draft.cursor,
        "required",
    );
    row = row.saturating_add(1);
    draw_text_input(
        frame,
        theme,
        body.x,
        row,
        body.width,
        "Repeat",
        &repeated,
        draft.field == IdentityField::RepeatedPassword && state.active_modal().is_none(),
        draft.cursor,
        "required",
    );
    row = row.saturating_add(1);
    draw_choice_row(
        frame,
        theme,
        body.x,
        row,
        body.width,
        "Shell",
        shell,
        draft.field == IdentityField::Shell,
    );
    row = row.saturating_add(1);
    if draft.reveal_password {
        draw_text_row(
            frame,
            body.x,
            row,
            body.width,
            "Password reveal is on",
            theme.warning(),
            Weight::Bold,
        );
        row = row.saturating_add(1);
    }
    if let Some(error) = state.validation_error.as_ref() {
        draw_text_row(
            frame,
            body.x,
            row,
            body.width,
            &format!("Error: {error}"),
            theme.danger(),
            Weight::Bold,
        );
    }
}

fn draw_world_map_panel(
    frame: &mut Frame,
    rect: Rect,
    controller: &UiController,
    map_cache: &mut Option<timezone::MapCache>,
) {
    let theme = controller.theme;
    if rect.width >= 2 && rect.height >= 2 {
        frame.render_widget(Border::style(theme.border()), rect);
    }
    let inner = Rect::new(
        rect.x + 1,
        rect.y + 1,
        rect.width.saturating_sub(2),
        rect.height.saturating_sub(2),
    );
    let center_lon = controller.state.timezone.map_center_lon;
    let selected = timezone::selected_zone(&controller.state);
    timezone::paint_world_map(
        frame,
        inner,
        map_cache,
        center_lon,
        selected.as_ref(),
        theme.map(),
        render::color_mode(),
    );
}

fn draw_timezone_list_panel(frame: &mut Frame, theme: Theme, rect: Rect, state: &AppState) {
    draw_panel(frame, theme, rect, "Time zones");
    let body = panel_body(rect);
    draw_filter_input(
        frame,
        theme,
        body.x,
        body.y,
        body.width,
        &state.timezone.filter,
        state.active_modal().is_none(),
    );
    let zones = timezone::visible_zones(state);
    if zones.is_empty() {
        let message = if state.timezone.filter.is_empty() {
            "No time zones available".to_string()
        } else {
            format!("No matches for \"{}\"", state.timezone.filter)
        };
        draw_text_row(
            frame,
            body.x,
            body.y.saturating_add(1),
            body.width,
            &message,
            theme.muted(),
            Weight::Normal,
        );
    } else {
        for (i, zone) in zones.iter().enumerate() {
            draw_zone_row(
                frame,
                theme,
                body.x,
                body.y + 1 + i as u16,
                body.width,
                zone,
            );
        }
    }
    if let Some(details) = timezone::selected_details(state) {
        draw_zone_details(
            frame,
            theme,
            body.x,
            body.y + 1 + timezone::VISIBLE_ZONE_ROWS as u16,
            body.width,
            &details,
            !state.timezone.filter.is_empty(),
        );
    }
}

fn timezone_content(
    frame: &mut Frame,
    content: Rect,
    controller: &UiController,
    layout: LayoutMode,
    map_cache: &mut Option<timezone::MapCache>,
) {
    let theme = controller.theme;
    let (cx0, cy0, cx1, cy1) = (
        content.x,
        content.y,
        content.x + content.width,
        content.y + content.height,
    );
    if layout == LayoutMode::Compact {
        let split = cy0 + (cy1 - cy0) * 2 / 5;
        let map_rect = Rect::new(cx0, cy0, content.width, split - cy0);
        let list_rect = Rect::new(cx0, split, content.width, cy1 - split);
        draw_world_map_panel(frame, map_rect, controller, map_cache);
        draw_timezone_list_panel(frame, theme, list_rect, &controller.state);
    } else {
        let split = cx0 + (cx1 - cx0) * 2 / 3;
        let map_rect = Rect::new(cx0, cy0, split - cx0, content.height);
        let list_rect = Rect::new(split, cy0, cx1 - split, content.height);
        draw_world_map_panel(frame, map_rect, controller, map_cache);
        draw_timezone_list_panel(frame, theme, list_rect, &controller.state);
    }
}

fn secure_boot_content(frame: &mut Frame, rect: Rect, controller: &UiController) {
    let state = &controller.state;
    let theme = controller.theme;
    draw_panel(frame, theme, rect, "Secure Boot configuration");
    let body = panel_body(rect);
    let mut row = body.y;
    if state
        .firmware
        .as_ref()
        .is_some_and(firmware::SetupMode::is_on)
    {
        draw_text_row(
            frame,
            body.x,
            row,
            body.width,
            &format!(
                "{} can enroll its own Secure Boot keys.",
                controller.ctx.config.branding.name
            ),
            theme.text(),
            Weight::Normal,
        );
        row = row.saturating_add(1);
        draw_text_row(
            frame,
            body.x,
            row,
            body.width,
            "This writes firmware Secure Boot variables.",
            theme.warning(),
            Weight::Bold,
        );
        row = row.saturating_add(1);
        for choice in SecureBootChoice::choices(true) {
            draw_selected_row(
                frame,
                theme,
                body.x,
                row,
                body.width,
                choice.label(),
                state.selection.secure_boot == *choice,
            );
            row = row.saturating_add(1);
        }
    } else {
        let message = match state.firmware.as_ref() {
            Some(firmware::SetupMode::Off) => "Setup Mode is off; keys cannot be enrolled.",
            _ => "Firmware Setup Mode could not be detected.",
        };
        draw_text_row(
            frame,
            body.x,
            row,
            body.width,
            message,
            theme.warning(),
            Weight::Bold,
        );
        row = row.saturating_add(1);
        if let Some(reason) = state
            .firmware
            .as_ref()
            .and_then(firmware::SetupMode::reason)
        {
            draw_text_row(
                frame,
                body.x,
                row,
                body.width,
                reason,
                theme.danger(),
                Weight::Bold,
            );
            row = row.saturating_add(1);
        }
        for choice in SecureBootChoice::choices(false) {
            draw_selected_row(
                frame,
                theme,
                body.x,
                row,
                body.width,
                choice.label(),
                state.selection.secure_boot == *choice,
            );
            row = row.saturating_add(1);
        }
    }
}

fn review_content(frame: &mut Frame, rect: Rect, controller: &UiController) {
    let state = &controller.state;
    let theme = controller.theme;
    draw_panel(frame, theme, rect, "Installation summary");
    let body = panel_body(rect);
    let mut row = body.y;
    draw_text_row(
        frame,
        body.x,
        row,
        body.width,
        "Review these settings carefully. Starting installation will erase the target disk.",
        theme.warning(),
        Weight::Bold,
    );
    row = row.saturating_add(2);
    draw_text_row(
        frame,
        body.x,
        row,
        body.width,
        &format!(
            "Target disk  {}",
            controller.ctx.drive_label.as_deref().unwrap_or("-")
        ),
        theme.text(),
        Weight::Normal,
    );
    row = row.saturating_add(1);
    draw_text_row(
        frame,
        body.x,
        row,
        body.width,
        &format!("Hostname     {}", controller.ctx.config.identity.hostname),
        theme.text(),
        Weight::Normal,
    );
    row = row.saturating_add(1);
    draw_text_row(
        frame,
        body.x,
        row,
        body.width,
        &format!("Timezone     {}", controller.ctx.config.identity.timezone),
        theme.text(),
        Weight::Normal,
    );
    row = row.saturating_add(1);
    draw_text_row(
        frame,
        body.x,
        row,
        body.width,
        &format!(
            "Secure Boot  {}",
            if controller.ctx.secure_boot == SecureBootMode::Enroll {
                "Enabled"
            } else {
                "Disabled"
            }
        ),
        theme.text(),
        Weight::Normal,
    );
    row = row.saturating_add(2);
    draw_selected_row(
        frame,
        theme,
        body.x,
        row,
        body.width,
        "[ Back ]",
        state.selection.review == ReviewChoice::Back,
    );
    row = row.saturating_add(1);
    draw_selected_row(
        frame,
        theme,
        body.x,
        row,
        body.width,
        "[ Start installation ]",
        state.selection.review == ReviewChoice::Start,
    );
}

fn format_elapsed(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

fn draw_install_steps(frame: &mut Frame, theme: Theme, rect: Rect, state: &AppState) {
    if rect.width < 2 || rect.height < 4 {
        return;
    }
    draw_panel(frame, theme, rect, "Installation steps");
    let body = panel_body(rect);
    let max_rows = usize::from(body.height);
    let current = state.install.current_step.unwrap_or(0);
    let start = current
        .saturating_sub(max_rows / 2)
        .min(pipeline::STEPS.len().saturating_sub(max_rows));
    for (offset, (index, step)) in pipeline::STEPS
        .iter()
        .enumerate()
        .skip(start)
        .take(max_rows)
        .enumerate()
    {
        let step_state = state
            .install
            .step_states
            .get(index)
            .copied()
            .unwrap_or(InstallStepState::Pending);
        let (marker, style, weight) = match step_state {
            InstallStepState::Done => ("[x]", theme.success(), Weight::Bold),
            InstallStepState::Active => ("[>]", theme.accent(), Weight::Bold),
            InstallStepState::Skipped => ("[-]", theme.muted(), Weight::Light),
            InstallStepState::Failed => ("[!]", theme.danger(), Weight::Bold),
            InstallStepState::Pending => ("[ ]", theme.muted(), Weight::Normal),
        };
        draw_text_row(
            frame,
            body.x,
            body.y + offset as u16,
            body.width,
            &format!("{marker} {:02} {}", index + 1, step.title),
            style,
            weight,
        );
    }
}

fn draw_install_log(frame: &mut Frame, theme: Theme, rect: Rect, logs: &str, scroll: usize) {
    draw_panel(frame, theme, rect, "Live installation log");
    let title = if scroll == 0 {
        "Live installation log".to_string()
    } else {
        format!("Live installation log  [^{scroll} lines - PgDn to follow]")
    };
    if rect.width > 2 && rect.height > 1 {
        let width = render::rendered_width(&title).min(rect.width.saturating_sub(2));
        frame.render_widget(
            Paragraph::new(title).style(theme.accent().bold()),
            Rect::new(rect.x + 1, rect.y + 1, width, 1),
        );
    }
    let body = panel_body(rect);
    frame.render_widget(Paragraph::new(logs.to_string()).style(theme.muted()), body);
}

#[expect(
    clippy::too_many_lines,
    reason = "install overview + split panels laid out together"
)]
fn install_content(
    frame: &mut Frame,
    content: Rect,
    controller: &UiController,
    layout: LayoutMode,
) {
    let state = &controller.state;
    let theme = controller.theme;
    let step_index = state.install.current_step.unwrap_or(0);
    let step = state
        .install
        .current_step
        .and_then(|index| pipeline::STEPS.get(index))
        .map_or("Preparing installation", |step| step.title);
    let running = state.install.status.is_running();
    let spinner_frame = state.install.started.map_or(0, |started| {
        (Instant::now()
            .saturating_duration_since(started)
            .as_millis() as usize
            / 100)
            % 4
    });
    let spinner = ["|", "/", "-", "\\"][spinner_frame];
    let (status, status_style) = if state.install.status.is_succeeded() {
        ("Installation complete".to_string(), theme.success())
    } else if state.install.status.is_failed() {
        ("Installation failed".to_string(), theme.danger())
    } else {
        (
            format!(
                "{spinner} Step {} of {}  {step}",
                step_index.saturating_add(1).min(pipeline::STEPS.len()),
                pipeline::STEPS.len()
            ),
            theme.accent(),
        )
    };
    let elapsed = match (state.install.started, state.install.elapsed_seconds) {
        (_, Some(seconds)) => Duration::from_secs(seconds),
        (Some(started), None) if running => started.elapsed(),
        _ => Duration::ZERO,
    };

    let overview = content.y;
    let progress_width = content.width;
    let status_line = format!("{status}  Elapsed {}", format_elapsed(elapsed));
    draw_text_row(
        frame,
        content.x,
        overview,
        progress_width,
        &status_line,
        status_style,
        Weight::Bold,
    );
    draw_progress(
        frame,
        theme,
        content.x,
        overview + 1,
        progress_width,
        "Overall",
        overall_install_progress(state),
        theme.success(),
    );
    draw_progress(
        frame,
        theme,
        content.x,
        overview + 2,
        progress_width,
        "Step",
        state.install.step_fraction,
        theme.accent(),
    );
    let committed_line = if state.install.committed {
        "Point of no return reached"
    } else {
        "Installation can still be cancelled safely"
    };
    let committed_style = if state.install.committed {
        theme.warning()
    } else {
        theme.muted()
    };
    draw_text_row(
        frame,
        content.x,
        overview + 3,
        progress_width,
        committed_line,
        committed_style,
        Weight::Light,
    );

    let panels_top = overview + 5;
    let panels_h = content.height.saturating_sub(5);

    let visible_lines = usize::from(panels_h.saturating_sub(4)).max(3);
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

    if layout == LayoutMode::Wide {
        let steps_w = content.width * 37 / 100;
        let steps_rect = Rect::new(content.x, panels_top, steps_w, panels_h);
        let log_rect = Rect::new(
            content.x + steps_w + 1,
            panels_top,
            content.width.saturating_sub(steps_w + 1),
            panels_h,
        );
        draw_install_steps(frame, theme, steps_rect, state);
        draw_install_log(frame, theme, log_rect, &logs, state.scroll.install_log);
    } else {
        let log_rect = Rect::new(content.x, panels_top, content.width, panels_h);
        draw_install_log(frame, theme, log_rect, &logs, state.scroll.install_log);
    }
}

fn reboot_content(frame: &mut Frame, rect: Rect, controller: &UiController) {
    let state = &controller.state;
    let theme = controller.theme;
    draw_panel(frame, theme, rect, "Installation complete");
    let body = panel_body(rect);
    draw_text_row(
        frame,
        body.x,
        body.y,
        body.width,
        &format!(
            "Target  {}",
            controller.ctx.drive_label.as_deref().unwrap_or("-")
        ),
        theme.text(),
        Weight::Normal,
    );
    draw_text_row(
        frame,
        body.x,
        body.y + 1,
        body.width,
        &format!(
            "Automatic reboot in {} seconds",
            state.reboot.remaining_seconds.unwrap_or(0)
        ),
        theme.accent(),
        Weight::Bold,
    );
    draw_selected_row(
        frame,
        theme,
        body.x,
        body.y + 3,
        body.width,
        "[ Open shell ]",
        state.selection.reboot == RebootChoice::Shell,
    );
    draw_selected_row(
        frame,
        theme,
        body.x,
        body.y + 4,
        body.width,
        "[ Reboot now ]",
        state.selection.reboot == RebootChoice::Now,
    );
}

fn draw_modal(frame: &mut Frame, theme: Theme, state: &AppState, width: u16, height: u16) {
    let Some(modal) = state.active_modal() else {
        return;
    };
    let (title, body) = match modal {
        Modal::WipeConfirmation => (
            "Wipe target disk?",
            "All data on this disk will be destroyed.".to_string(),
        ),
        Modal::RebootConfirmation => (
            "Reboot now?",
            "The machine will reboot immediately.".to_string(),
        ),
        Modal::SetupModeRequired => (
            "Enable Secure Boot?",
            "Custom keys can only be enrolled while firmware Setup Mode is on. Reboot into firmware settings and enable Setup Mode (typically by clearing Secure Boot keys), then run the installer again."
                .to_string(),
        ),
        Modal::Failure => (
            "Installation failed",
            state
                .install
                .status
                .reason()
                .unwrap_or("Unknown error")
                .to_string(),
        ),
        Modal::Cancellation => (
            "Cancel installation?",
            "The installer will stop and clean up mounts.".to_string(),
        ),
    };
    let (modal_border, modal_title) = if modal == Modal::Failure {
        (theme.danger(), theme.well_danger())
    } else {
        (theme.warning(), theme.well_warning())
    };
    let dialog = modal_dialog_rect(width, height);
    if dialog.width < 2 || dialog.height < 2 {
        return;
    }
    frame.render_widget(Border::style(modal_border), dialog);
    let fill = Rect::new(
        dialog.x.saturating_add(1),
        dialog.y.saturating_add(1),
        dialog.width.saturating_sub(2),
        dialog.height.saturating_sub(2),
    );
    if fill.width == 0 || fill.height == 0 {
        return;
    }
    frame.render_widget(Block::default().style(theme.well()), fill);
    let inner = Rect::new(
        fill.x.saturating_add(1),
        fill.y,
        fill.width.saturating_sub(2),
        fill.height,
    );
    if inner.width == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(title).style(modal_title.bold()),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    if inner.height > 3 {
        let wrapped = wrap_dialog_text(&body, inner.width);
        let body_height = inner.height.saturating_sub(3);
        let shown = wrapped
            .into_iter()
            .take(usize::from(body_height))
            .collect::<Vec<_>>();
        if !shown.is_empty() {
            frame.render_widget(
                Paragraph::new(shown.join("\n")).style(theme.well_text()),
                Rect::new(inner.x, inner.y.saturating_add(2), inner.width, body_height),
            );
        }
    }
    let buttons = modal_button_rects(dialog);
    let labels = if modal == Modal::SetupModeRequired {
        [(ModalChoice::No, "Back"), (ModalChoice::Yes, "Reboot")]
    } else {
        [(ModalChoice::No, "No"), (ModalChoice::Yes, "Yes")]
    };
    for (rect, (choice, label)) in buttons.into_iter().zip(labels) {
        if rect.width == 0 {
            continue;
        }
        let focused = state.selection.modal == choice;
        let style = if focused {
            theme.inverse().bold()
        } else {
            theme.well_muted()
        };
        let pad = rect.width.saturating_sub(render::rendered_width(label)) / 2;
        let text = format!("{:>width$}{label}", "", width = usize::from(pad));
        frame.render_widget(Paragraph::new(text).style(style), rect);
    }
}

#[derive(Clone, Copy, Debug)]
struct KeyChip {
    key: &'static str,
    action: &'static str,
}

impl KeyChip {
    const fn new(key: &'static str, action: &'static str) -> Self {
        Self { key, action }
    }

    fn width(self) -> usize {
        1 + 1 + self.key.chars().count() + 1 + 1 + self.action.chars().count()
    }
}

fn route_footer_chips(state: &AppState) -> Vec<KeyChip> {
    match state.route {
        Route::Welcome => vec![
            KeyChip::new("Up/Dn", "select"),
            KeyChip::new("Enter", "activate"),
            KeyChip::new("Esc", "quit"),
        ],
        Route::Target => vec![
            KeyChip::new("Up/Dn", "select"),
            KeyChip::new("Ctrl-R", "rescan"),
            KeyChip::new("Enter", "continue"),
            KeyChip::new("Esc", "back"),
        ],
        Route::Identity => vec![
            KeyChip::new("Up/Dn/Tab", "field"),
            KeyChip::new("Left/Right", "move"),
            KeyChip::new("Home/End", "jump"),
            KeyChip::new("Ctrl-U", "clear"),
            KeyChip::new("Ctrl-R", "reveal"),
            KeyChip::new("Enter", "continue"),
            KeyChip::new("Esc", "back"),
        ],
        Route::Timezone => vec![
            KeyChip::new("Type", "filter"),
            KeyChip::new("Up/Dn", "choose"),
            KeyChip::new("Left/Right", "pan"),
            KeyChip::new("Enter", "continue"),
            KeyChip::new("Esc", "back"),
        ],
        Route::SecureBoot | Route::Review | Route::Reboot => vec![
            KeyChip::new("Up/Dn/Tab", "select"),
            KeyChip::new("Enter", "activate"),
            KeyChip::new("Esc", "back"),
        ],
        Route::Install if state.install.status.is_succeeded() => {
            vec![KeyChip::new("Enter", "continue")]
        }
        Route::Install if state.install.status.is_failed() => {
            vec![KeyChip::new("Enter", "menu")]
        }
        Route::Install if state.install.committed => vec![KeyChip::new("PgUp/PgDn", "scroll")],
        Route::Install => vec![
            KeyChip::new("PgUp/PgDn", "scroll"),
            KeyChip::new("Esc", "cancel"),
        ],
    }
}

fn is_essential_chip(chip: KeyChip) -> bool {
    matches!(chip.key, "Enter" | "Esc")
}

fn fit_footer_chips(chips: &[KeyChip], max_width: usize) -> Vec<KeyChip> {
    if chips.is_empty() || max_width == 0 {
        return Vec::new();
    }

    let essentials = chips
        .iter()
        .copied()
        .filter(|chip| is_essential_chip(*chip))
        .collect::<Vec<_>>();
    let others = chips
        .iter()
        .copied()
        .filter(|chip| !is_essential_chip(*chip))
        .collect::<Vec<_>>();

    let essential_width = essentials.iter().map(|chip| chip.width()).sum::<usize>();
    if essential_width > max_width {
        let mut used = 0;
        let mut out = Vec::new();
        for chip in essentials {
            let width = chip.width();
            if used + width > max_width {
                break;
            }
            used += width;
            out.push(chip);
        }
        return out;
    }

    let mut remaining = max_width - essential_width;
    let mut body = Vec::new();
    for chip in others {
        let width = chip.width();
        if width > remaining {
            break;
        }
        remaining -= width;
        body.push(chip);
    }
    body.extend(essentials);
    body
}

fn draw_footer(frame: &mut Frame, theme: Theme, chips: &[KeyChip], width: u16, height: u16) {
    let max_width = usize::from(width.saturating_sub(1));
    let fitted = fit_footer_chips(chips, max_width);
    let mut spans = vec![Span::raw(" ")];
    for chip in &fitted {
        spans.push(Span::styled(
            format!(" [{}]", chip.key),
            theme.well_accent().bold(),
        ));
        spans.push(Span::styled(
            format!(" {}", chip.action),
            theme.well_muted(),
        ));
    }
    if fitted.is_empty() {
        spans.push(Span::styled(" ", theme.well_muted()));
    }
    frame.render_widget(
        Paragraph::new(Line::new(spans)).style(theme.well()),
        Rect::new(0, height.saturating_sub(1), width, 1),
    );
}

fn screen_title(controller: &UiController) -> String {
    match controller.state.route {
        Route::Welcome => match controller.ctx.image_version.as_deref() {
            Some(version) => format!(
                "{} {version} Installer",
                controller.ctx.config.branding.name
            ),
            None => format!("{} Installer", controller.ctx.config.branding.name),
        },
        Route::Target => "Select Target Drive".to_string(),
        Route::Identity => "System Identity".to_string(),
        Route::Timezone => "Select Time Zone".to_string(),
        Route::SecureBoot => "Secure Boot".to_string(),
        Route::Review => "Does this look right?".to_string(),
        Route::Install => format!("Installing {}", controller.ctx.config.branding.name),
        Route::Reboot => "Install Complete".to_string(),
    }
}

fn screen_subtitle(route: Route) -> &'static str {
    match route {
        Route::Welcome => "READY  /  GUIDED SYSTEM INSTALLATION",
        Route::Target => "SETUP  1 / 5  /  TARGET DISK",
        Route::Identity => "SETUP  2 / 5  /  SYSTEM IDENTITY",
        Route::Timezone => "SETUP  3 / 5  /  TIME ZONE",
        Route::SecureBoot => "SETUP  4 / 5  /  PLATFORM SECURITY",
        Route::Review => "SETUP  5 / 5  /  REVIEW",
        Route::Install => "INSTALLATION IN PROGRESS",
        Route::Reboot => "INSTALLATION COMPLETE",
    }
}

/// Paint the whole screen into the offscreen frame.
fn render_screen(
    frame: &mut Frame,
    controller: &UiController,
    map_cache: &mut Option<timezone::MapCache>,
) {
    let area = frame.area();
    let (width, height) = (area.width, area.height);
    if width == 0 || height == 0 {
        return;
    }
    let state = &controller.state;
    let theme = controller.theme;

    // Background fill so blank cells and borders carry the installer background.
    frame.render_widget(Block::default().style(theme.canvas()), area);

    // Title + subtitle bars.
    frame.render_widget(
        Paragraph::new(screen_title(controller)).style(theme.title().bold()),
        Rect::new(0, 0, width, 1),
    );
    frame.render_widget(
        Paragraph::new(screen_subtitle(state.route)).style(theme.subtitle().bold()),
        Rect::new(0, 1, width, 1),
    );

    // Footer chip bar.
    let chips = route_footer_chips(state);
    draw_footer(frame, theme, &chips, width, height);

    // Content panel area (matches hit-testing geometry in content_frame).
    let (cx0, cy0, cx1, cy1) = content_frame(width, height);
    let content = Rect::new(cx0, cy0, cx1.saturating_sub(cx0), cy1.saturating_sub(cy0));
    let layout = layout_mode((width, height));
    match state.route {
        Route::Welcome => welcome_content(frame, theme, content, state),
        Route::Target => target_content(frame, content, controller, layout),
        Route::Identity => identity_content(frame, theme, content, state),
        Route::Timezone => timezone_content(frame, content, controller, layout, map_cache),
        Route::SecureBoot => secure_boot_content(frame, content, controller),
        Route::Review => review_content(frame, content, controller),
        Route::Install => install_content(frame, content, controller, layout),
        Route::Reboot => reboot_content(frame, content, controller),
    }

    // Modal overlay sits on top of everything.
    if state.active_modal().is_some() {
        draw_modal(frame, theme, state, width, height);
    }
}

impl Installer {
    /// Run the terminal UI: paint offscreen frames and blit them until a trap fires.
    pub fn run(&mut self) -> Trap {
        let _panic_guard = install_panic_hook();
        let mouse = mouse_capture_enabled();
        let mut terminal = Terminal::new(io::stdout());
        tui_or_exit(terminal.activate(), "activate");
        if mouse {
            let _ = crossterm::execute!(io::stdout(), crossterm::event::EnableMouseCapture);
        }

        let mut controller = UiController {
            ctx: std::mem::take(&mut self.ctx),
            state: std::mem::take(&mut self.state),
            theme: Theme::INSTALLER,
            task: None,
            drive_rx: None,
            trap: None,
            last_countdown: Instant::now(),
            last_animation: Instant::now(),
            last_visual_pulse: Instant::now(),
            last_map_sun_minute: None,
            terminal_size: terminal_size(),
        };
        prepare_state(&mut controller);
        let mut map_cache: Option<timezone::MapCache> = None;

        loop {
            controller.terminal_size = terminal_size();
            if controller.trap.is_some() {
                break;
            }
            let timeout = poll_interval(&controller);
            let mut dirty = false;
            match poll_input(timeout) {
                Some(Input::Key(key)) => {
                    dispatch(&mut controller, Message::Key(key));
                    dirty = true;
                }
                Some(Input::Mouse(mouse)) => {
                    if let Some(kind) = mouse_kind(mouse.kind) {
                        dispatch(
                            &mut controller,
                            Message::Mouse {
                                column: mouse.column,
                                row: mouse.row,
                                kind,
                            },
                        );
                        dirty = true;
                    }
                }
                Some(Input::Resize) => {
                    controller.terminal_size = terminal_size();
                    dirty = true;
                }
                None => {}
            }
            dirty |= service(&mut controller);
            if controller.trap.is_some() {
                break;
            }
            if dirty {
                let controller_ref: &UiController = &controller;
                tui_or_exit(
                    terminal.draw(|frame| render_screen(frame, controller_ref, &mut map_cache)),
                    "draw",
                );
            }
        }

        if mouse {
            let _ = crossterm::execute!(io::stdout(), crossterm::event::DisableMouseCapture);
        }
        tui_or_exit(terminal.deactivate(), "deactivate");
        if let Some(task) = controller.task.take() {
            let _ = task.cancel_and_join();
        }
        self.ctx = std::mem::take(&mut controller.ctx);
        self.state = std::mem::take(&mut controller.state);
        controller.trap.take().unwrap_or(Trap::Quit)
    }
}
