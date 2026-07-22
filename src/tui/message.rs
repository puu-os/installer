// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#![allow(
    dead_code,
    reason = "engine contracts are adopted by the runtime-loop migration"
)]

//! Messages represent everything that can change the installer's state.
//!
//! Inputs (keys, resize, timers) and install-pipeline callbacks are translated
//! into messages before reaching the update function. This keeps
//! [`crate::tui::state::AppState`] pure and keeps disk, firmware, command,
//! rollback, and reboot work out of render nodes and update logic.

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Maximum UTF-8 byte length of text crossing the worker/UI queue.
pub(crate) const MAX_MESSAGE_TEXT_BYTES: usize = 1024;

pub(crate) fn bounded_text(text: &str) -> String {
    if text.len() <= MAX_MESSAGE_TEXT_BYTES {
        return text.to_string();
    }

    let mut end = MAX_MESSAGE_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Message {
    Resized {
        width: u16,
        height: u16,
    },
    Key(KeyEvent),
    Paste(String),
    Tick,
    CountdownTick,
    DrivesRefreshed(Vec<DriveInfo>),
    DriveSelected {
        index: usize,
    },
    FieldEdit {
        form: FormId,
        field: FieldId,
        value: String,
    },
    FieldFocused {
        form: FormId,
        field: FieldId,
    },
    Toggle {
        id: ToggleId,
    },
    FormValidated {
        form: FormId,
        error: Option<String>,
    },
    Submit,
    Back,
    CancelInstall,
    Log(String),
    InstallProgress {
        step_index: usize,
        fraction_pct: u16,
    },
    InstallSkipped {
        step_index: usize,
    },
    InstallFinished {
        at: Option<usize>,
        elapsed: Option<Duration>,
        error: Option<String>,
    },
    ShellReturned {
        status: Result<i32, String>,
    },
    FirmwareReady {
        status: FirmwareStatus,
    },
    EnrollFinished {
        error: Option<String>,
    },
    Reboot,
    Quit,
}

impl Message {
    pub(crate) const fn code(key: KeyCode) -> Self {
        Self::Key(KeyEvent::new(key, KeyModifiers::NONE))
    }

    pub(crate) fn ctrl(key: char) -> Self {
        Self::Key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::CONTROL))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FormId {
    Identity,
    TimezoneFilter,
    TimezoneSearch,
    ReviewNote,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FieldId {
    Language,
    Hostname,
    Username,
    Password,
    RepeatedPassword,
    Shell,
    TextFilter,
    SearchQuery,
    Note,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ToggleId {
    RevealPassword,
    WipeConfirm,
    FollowLogs,
    SecureBootEnroll,
    RebootAuto,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DriveInfo {
    pub(crate) index: usize,
    pub(crate) path: String,
    pub(crate) label: String,
    pub(crate) size: String,
    pub(crate) model: String,
    pub(crate) transport: String,
    pub(crate) busy_reasons: Vec<String>,
    pub(crate) selected: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FirmwareStatus {
    pub(crate) setup_mode: Option<bool>,
    pub(crate) secure_boot: Option<bool>,
    pub(crate) reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    RefreshDrives,
    DetectFirmware,
    SelectDrive { index: usize },
    SelectTimezone { name: String },
    SetSecureBoot(bool),
    ValidateForm { form: FormId },
    StartInstall,
    CancelPipeline,
    RunShell { command: Vec<String> },
    EnrollKeys,
    Reboot,
    Quit,
    None,
}
