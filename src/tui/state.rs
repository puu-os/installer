// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! Installer's navigation and transitional state.

use std::time::Instant;

use zeroize::Zeroizing;

use crate::engine::config::UserShell;
use crate::engine::firmware::SetupMode;
use crate::tui::message::DriveInfo;

pub(crate) trait Cyclic: Copy + PartialEq + Sized + 'static {
    const ALL: &'static [Self];

    fn next(self) -> Self {
        wrap_choice(Self::ALL, self, false)
    }

    fn previous(self) -> Self {
        wrap_choice(Self::ALL, self, true)
    }

    fn cycle(self, backwards: bool) -> Self {
        wrap_choice(Self::ALL, self, backwards)
    }

    fn toggle(self) -> Self {
        self.next()
    }

    fn from_hit_index(index: usize) -> Option<Self> {
        Self::ALL.get(index).copied()
    }
}

fn wrap_choice<T: Copy + PartialEq>(all: &[T], current: T, backwards: bool) -> T {
    let Some(index) = all.iter().position(|&item| item == current) else {
        return current;
    };
    let next = if backwards {
        index.checked_sub(1).unwrap_or(all.len().saturating_sub(1))
    } else if index + 1 < all.len() {
        index + 1
    } else {
        0
    };
    all.get(next).copied().unwrap_or(current)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum IdentityField {
    #[default]
    Language,
    Hostname,
    Username,
    Password,
    RepeatedPassword,
    Shell,
}

impl Cyclic for IdentityField {
    const ALL: &'static [Self] = &[
        Self::Language,
        Self::Hostname,
        Self::Username,
        Self::Password,
        Self::RepeatedPassword,
        Self::Shell,
    ];
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LocaleChoice {
    #[default]
    EnUs,
    EnGb,
    Fi,
    Sv,
    Nb,
    Da,
    De,
    Fr,
    Es,
    It,
}

impl Cyclic for LocaleChoice {
    const ALL: &'static [Self] = &[
        Self::EnUs,
        Self::EnGb,
        Self::Fi,
        Self::Sv,
        Self::Nb,
        Self::Da,
        Self::De,
        Self::Fr,
        Self::Es,
        Self::It,
    ];
}

impl LocaleChoice {
    pub(crate) const fn label(self) -> &'static str {
        self.entry().0
    }

    pub(crate) const fn locale(self) -> &'static str {
        self.entry().1
    }

    const fn entry(self) -> (&'static str, &'static str) {
        match self {
            Self::EnUs => ("English (US)", "en_US.UTF-8"),
            Self::EnGb => ("English (UK)", "en_GB.UTF-8"),
            Self::Fi => ("Finnish", "fi_FI.UTF-8"),
            Self::Sv => ("Swedish", "sv_SE.UTF-8"),
            Self::Nb => ("Norwegian", "nb_NO.UTF-8"),
            Self::Da => ("Danish", "da_DK.UTF-8"),
            Self::De => ("German", "de_DE.UTF-8"),
            Self::Fr => ("French", "fr_FR.UTF-8"),
            Self::Es => ("Spanish", "es_ES.UTF-8"),
            Self::It => ("Italian", "it_IT.UTF-8"),
        }
    }
}

#[derive(Default)]
pub(crate) struct IdentityDraft {
    pub(crate) language: LocaleChoice,
    pub(crate) hostname: String,
    pub(crate) username: String,
    pub(crate) password: Zeroizing<String>,
    pub(crate) repeated_password: Zeroizing<String>,
    pub(crate) shell: UserShell,
    pub(crate) field: IdentityField,
    /// Cursor position within the focused text field, in Unicode scalar values.
    pub(crate) cursor: usize,
    pub(crate) reveal_password: bool,
}

impl Cyclic for UserShell {
    const ALL: &'static [Self] = &[Self::Bash, Self::Zsh];
}

/// Persistent state for the filterable timezone picker.
#[derive(Clone, Debug, Default)]
pub(crate) struct TimezoneDraft {
    pub(crate) filter: String,
    pub(crate) selected: usize,
    pub(crate) top: usize,
    pub(crate) filtered: Vec<usize>,
    pub(crate) map_center_lon: f64,
    pub(crate) map_target_lon: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Modal {
    WipeConfirmation,
    RebootConfirmation,
    SetupModeRequired,
    Failure,
    Cancellation,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum WelcomeChoice {
    #[default]
    Install,
    Shell,
    Reboot,
}

impl Cyclic for WelcomeChoice {
    const ALL: &'static [Self] = &[Self::Install, Self::Shell, Self::Reboot];
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ModalChoice {
    #[default]
    No,
    Yes,
}

impl Cyclic for ModalChoice {
    const ALL: &'static [Self] = &[Self::No, Self::Yes];
}

impl ModalChoice {
    pub(crate) const fn is_yes(self) -> bool {
        matches!(self, Self::Yes)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ReviewChoice {
    Back,
    #[default]
    Start,
}

impl Cyclic for ReviewChoice {
    const ALL: &'static [Self] = &[Self::Back, Self::Start];
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum RebootChoice {
    Shell,
    #[default]
    Now,
}

impl Cyclic for RebootChoice {
    const ALL: &'static [Self] = &[Self::Shell, Self::Now];
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SecureBootChoice {
    Back,
    Enable,
    #[default]
    Continue,
    Reboot,
}

impl SecureBootChoice {
    pub(crate) const fn default_for(setup_mode_on: bool) -> Self {
        if setup_mode_on {
            Self::Enable
        } else {
            Self::Continue
        }
    }

    pub(crate) fn choices(setup_mode_on: bool) -> &'static [Self] {
        if setup_mode_on {
            &[Self::Back, Self::Enable]
        } else {
            &[Self::Back, Self::Enable, Self::Continue, Self::Reboot]
        }
    }

    pub(crate) fn cycle(self, setup_mode_on: bool, backwards: bool) -> Self {
        let choices = Self::choices(setup_mode_on);
        let Some(pos) = choices.iter().position(|&choice| choice == self) else {
            return Self::default_for(setup_mode_on);
        };
        if backwards {
            choices[pos.checked_sub(1).unwrap_or(choices.len() - 1)]
        } else {
            choices[(pos + 1) % choices.len()]
        }
    }

    /// Map a click index onto a choice for the current firmware menu.
    pub(crate) fn from_hit_index(index: usize, setup_mode_on: bool) -> Option<Self> {
        Self::choices(setup_mode_on).get(index).copied()
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Back => "[ Back ]",
            Self::Enable => "[ Enable Secure Boot ]",
            Self::Continue => "[ Continue without Secure Boot ]",
            Self::Reboot => "[ Reboot ]",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SelectionState {
    pub(crate) welcome: WelcomeChoice,
    pub(crate) target: Option<usize>,
    pub(crate) secure_boot: SecureBootChoice,
    pub(crate) review: ReviewChoice,
    pub(crate) reboot: RebootChoice,
    pub(crate) modal: ModalChoice,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ScrollState {
    pub(crate) install_log: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum InstallStatus {
    #[default]
    Running,
    Succeeded,
    Failed {
        reason: String,
    },
}

impl InstallStatus {
    pub(crate) const fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }

    pub(crate) const fn is_succeeded(&self) -> bool {
        matches!(self, Self::Succeeded)
    }

    pub(crate) const fn is_failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }

    pub(crate) fn reason(&self) -> Option<&str> {
        match self {
            Self::Failed { reason } => Some(reason.as_str()),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct InstallProgress {
    pub(crate) current_step: Option<usize>,
    pub(crate) step_fraction: u16,
    pub(crate) step_states: Vec<InstallStepState>,
    pub(crate) committed: bool,
    pub(crate) follow_logs: bool,
    pub(crate) logs: std::collections::VecDeque<String>,
    pub(crate) elapsed_seconds: Option<u64>,
    pub(crate) started: Option<Instant>,
    pub(crate) status: InstallStatus,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RebootState {
    pub(crate) remaining_seconds: Option<u16>,
    pub(crate) automatic: bool,
}

pub(crate) struct AppState {
    /// The route rendered by the single runtime loop.
    pub(crate) route: Route,
    pub(crate) modal_stack: Vec<Modal>,
    pub(crate) identity: Option<IdentityDraft>,
    pub(crate) timezone: TimezoneDraft,
    pub(crate) validation_error: Option<String>,
    pub(crate) selection: SelectionState,
    pub(crate) scroll: ScrollState,
    pub(crate) drives: Vec<DriveInfo>,
    pub(crate) firmware: Option<SetupMode>,
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
            selection: SelectionState::default(),
            scroll: ScrollState::default(),
            drives: Vec::new(),
            firmware: None,
            install: InstallProgress::default(),
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
        self.selection = SelectionState::default();
        self.scroll = ScrollState::default();
        self.drives.clear();
        self.firmware = None;
        self.install = InstallProgress::default();
        self.reboot = RebootState::default();
    }

    pub(crate) fn push_modal(&mut self, modal: Modal) {
        self.modal_stack.push(modal);
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstallStepState {
    Pending,
    Active,
    Done,
    Skipped,
    Failed,
}
