// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! Installer's navigation and transitional state.

use std::time::Instant;

use crate::tui::engine::NodeId;
use crate::tui::message::{DriveInfo, FirmwareStatus};

#[derive(Clone, Debug)]
pub(crate) struct IdentityDraft {
    pub(crate) language_index: usize,
    pub(crate) hostname: String,
    pub(crate) username: String,
    pub(crate) password: String,
    pub(crate) repeated_password: String,
    pub(crate) shell_index: usize,
    pub(crate) field_index: usize,
    pub(crate) reveal_password: bool,
}

/// Persistent state for the filterable timezone picker.
#[derive(Clone, Debug, Default)]
pub(crate) struct TimezoneDraft {
    pub(crate) filter: String,
    pub(crate) selected: usize,
    pub(crate) top: usize,
    pub(crate) filtered: Vec<usize>,
    pub(crate) map_center_lon_milli: i32,
    pub(crate) map_target_lon_milli: i32,
}

#[allow(dead_code, reason = "populated by the runtime-loop migration")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Modal {
    #[default]
    None,
    WipeConfirmation,
    RebootConfirmation,
    Failure,
    Cancellation,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SelectionState {
    pub(crate) welcome: usize,
    pub(crate) target: Option<usize>,
    pub(crate) secure_boot: usize,
    pub(crate) review: usize,
    pub(crate) reboot: usize,
    pub(crate) modal: usize,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ScrollState {
    pub(crate) install_log: usize,
}

#[derive(Clone, Debug, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "install-run state; flags fold into a status enum in a follow-up"
)]
pub(crate) struct InstallProgress {
    pub(crate) current_step: Option<usize>,
    pub(crate) step_fraction: u16,
    pub(crate) step_states: Vec<InstallStepState>,
    pub(crate) committed: bool,
    pub(crate) follow_logs: bool,
    pub(crate) logs: Vec<String>,
    pub(crate) elapsed_seconds: Option<u64>,
    pub(crate) started: Option<Instant>,
    pub(crate) failed: bool,
    pub(crate) complete: bool,
    pub(crate) failure: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RebootState {
    pub(crate) remaining_seconds: Option<u16>,
    pub(crate) automatic: bool,
    pub(crate) shell_active: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct AppState {
    /// The route rendered by the single runtime loop.
    pub(crate) route: Route,
    pub(crate) modal_stack: Vec<Modal>,
    pub(crate) identity: Option<IdentityDraft>,
    pub(crate) timezone: TimezoneDraft,
    pub(crate) validation_error: Option<String>,
    pub(crate) focused: Option<NodeId>,
    pub(crate) selection: SelectionState,
    pub(crate) scroll: ScrollState,
    pub(crate) drives: Vec<DriveInfo>,
    pub(crate) firmware: Option<FirmwareStatus>,
    pub(crate) install: InstallProgress,
    pub(crate) reboot: RebootState,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            route: Route::Welcome,
            modal_stack: Vec::new(),
            identity: None,
            timezone: TimezoneDraft::default(),
            validation_error: None,
            focused: None,
            selection: SelectionState::default(),
            scroll: ScrollState::default(),
            install: InstallProgress::default(),
            drives: Vec::new(),
            firmware: None,
            reboot: RebootState::default(),
        }
    }
}

impl AppState {
    pub(crate) fn reset_flow(&mut self) {
        self.route = Route::Target;
        self.modal_stack.clear();
        self.validation_error = None;
        self.timezone = TimezoneDraft::default();
        self.focused = None;
        self.selection = SelectionState::default();
        self.scroll = ScrollState::default();
        self.drives.clear();
        self.firmware = None;
        self.install = InstallProgress::default();
        self.reboot = RebootState::default();
    }

    pub(crate) fn push_modal(&mut self, modal: Modal) {
        if modal != Modal::None {
            self.modal_stack.push(modal);
        }
    }

    pub(crate) fn pop_modal(&mut self) -> Option<Modal> {
        self.modal_stack.pop()
    }

    pub(crate) fn active_modal(&self) -> Option<Modal> {
        self.modal_stack.last().copied()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    Welcome,
    Target,
    Identity,
    Timezone,
    SecureBoot,
    Review,
    Install,
    Reboot,
}

#[allow(dead_code, reason = "used by the runtime-loop migration")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstallStepState {
    Pending,
    Active,
    Done,
    Skipped,
    Failed,
}
