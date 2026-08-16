// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! Messages represent everything that can change the installer's state.
//!
//! Inputs (keys, resize, timers) and install-pipeline callbacks are translated
//! into messages before reaching the update function. This keeps
//! [`crate::tui::state::AppState`] pure and keeps disk, firmware, command,
//! rollback, and reboot work out of render nodes and update logic.

use std::time::Duration;

use crossterm::event::KeyEvent;

use crate::engine::firmware::{SecureBootMode, SetupMode};

/// Maximum UTF-8 byte length of text crossing the worker/UI queue.
pub(crate) const MAX_MESSAGE_TEXT_BYTES: usize = 1024;

/// Non-ASCII glyphs a cell is allowed to hold on a Linux VT.
///
/// Every entry is a CP437 glyph present in VGA / Linux console fonts. Quadrant
/// blocks and Braille are deliberately excluded: they are not in console fonts,
/// and the installer's primary target is a Linux VT.
const fn is_console_glyph(ch: char) -> bool {
    matches!(
        ch,
        '\u{00b0}' // degree sign
            | '\u{00b7}' // middle dot
            | '\u{2500}' // box drawings light horizontal
            | '\u{2502}' // box drawings light vertical
            | '\u{250c}'
            | '\u{2510}'
            | '\u{2514}'
            | '\u{2518}' // light corners
            | '\u{251c}'
            | '\u{2524}'
            | '\u{252c}'
            | '\u{2534}' // light tees
            | '\u{253c}' // box drawings light vertical and horizontal
            | '\u{2580}' // upper half block
            | '\u{2584}' // lower half block
            | '\u{2588}' // full block
            | '\u{258c}'
            | '\u{2590}' // left / right half block
            | '\u{2591}'
            | '\u{2592}'
            | '\u{2593}' // light / medium / dark shade
            | '\u{25a0}' // black square
    )
}

/// Fold one source character into a single narrow console-safe glyph.
pub(crate) fn sanitize_char(ch: char) -> char {
    match ch {
        ' ' | '\t' | '\n' | '\r' => ' ',
        c if c.is_ascii_graphic() => c,
        '\u{2014}' | '\u{2013}' | '\u{2212}' => '-', // em dash, en dash, minus
        '\u{2018}' | '\u{2019}' => '\'',             // curly single quotes
        '\u{201c}' | '\u{201d}' => '"',              // curly double quotes
        c if is_console_glyph(c) => c,
        _ => '?',
    }
}

/// Characters that fold to more than one cell.
pub(crate) fn expand_char(ch: char) -> Option<&'static str> {
    match ch {
        '\u{2192}' => Some("->"),
        '\u{2190}' => Some("<-"),
        '\u{2265}' => Some(">="),
        '\u{2264}' => Some("<="),
        '\u{2026}' => Some("..."),
        _ => None,
    }
}

/// Sanitize dynamic text for Linux VT / CP437 glyph safety.
///
/// Wide, combining, control, and non-console glyphs are replaced so disk
/// models, firmware reasons, subprocess logs, and error messages cannot break
/// the cell grid on a plain Linux console.
pub(crate) fn sanitize_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if let Some(expanded) = expand_char(ch) {
            out.push_str(expanded);
        } else {
            out.push(sanitize_char(ch));
        }
    }
    out
}

/// Bound and sanitize text crossing the worker/UI queue.
pub(crate) fn bounded_text(text: &str) -> String {
    let sanitized = sanitize_text(text);
    if sanitized.len() <= MAX_MESSAGE_TEXT_BYTES {
        return sanitized;
    }
    let mut end = MAX_MESSAGE_TEXT_BYTES;
    while !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    sanitized[..end].to_string()
}

/// Subset of mouse events the installer acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MouseKind {
    LeftDown,
    ScrollUp,
    ScrollDown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Message {
    Key(KeyEvent),
    /// Mouse interaction (only delivered when the terminal supports capture).
    Mouse {
        column: u16,
        row: u16,
        kind: MouseKind,
    },
    CountdownTick,
    DrivesRefreshed(Vec<DriveInfo>),
    DriveSelected {
        index: usize,
    },
    FormValidated {
        error: Option<String>,
    },
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
    FirmwareReady {
        status: SetupMode,
    },
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    RefreshDrives,
    DetectFirmware,
    SelectDrive { index: usize },
    SelectTimezone { name: String },
    SetSecureBoot(SecureBootMode),
    ValidateForm,
    StartInstall,
    CancelPipeline,
    RunShell,
    Reboot,
    Quit,
}
