// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! Real, observed progress for long-running steps that emit no native progress
//! of their own.

use std::path::{Path, PathBuf};

use anyhow::Result;

/// A source of real, monotonic completion for a long-running step, sampled
/// periodically while the step's external command runs.
pub trait ProgressSource {
    /// Best estimate of completion in `[0.0, 1.0]`, or `None` while it cannot
    /// yet be determined.
    fn fraction(&mut self) -> Option<f64>;

    /// Offer a line of the command's merged output. Sources that read disk
    /// state ignore it; sources driven by the command's own reporting parse it.
    fn observe_line(&mut self, _line: &str) {}
}

/// Highest fraction a [`ProgressSource`] may report before the step actually
/// finishes, so an inexact estimate never paints a full bar prematurely.
const PROGRESS_CAP: f64 = 0.98;

/// Token a cooperating helper prints, followed by `<done>/<total>`, to report
/// its own progress, e.g. `PUU_PROGRESS 3/7`.
const PROGRESS_MARKER: &str = "PUU_PROGRESS";

/// Completion measured from the bytes that have landed on a filesystem since
/// the step began, relative to an expected total. Follows large writes such as
/// the bootc image deployment, whose container pull and ostree checkout both
/// land on the target filesystem.
pub struct DiskWriteProgress {
    path: PathBuf,
    start_avail: u64,
    expected: u64,
    peak: f64,
}

impl DiskWriteProgress {
    /// Capture the filesystem's current free space as the baseline against
    /// which `expected` further bytes count as full completion.
    pub fn new(path: impl Into<PathBuf>, expected: u64) -> Result<Self> {
        let path = path.into();
        let start_avail = available_bytes(&path)?;
        Ok(Self {
            path,
            start_avail,
            expected: expected.max(1),
            peak: 0.0,
        })
    }
}

impl ProgressSource for DiskWriteProgress {
    fn fraction(&mut self) -> Option<f64> {
        let avail = available_bytes(&self.path).ok()?;
        let written = self.start_avail.saturating_sub(avail);
        let fraction = (written as f64 / self.expected as f64).clamp(0.0, PROGRESS_CAP);
        if fraction > self.peak {
            self.peak = fraction;
        }
        Some(self.peak)
    }
}

/// Completion parsed from a helper that prints [`PROGRESS_MARKER`] `<done>/<total>`
/// lines as it works, such as the Flatpak pre-seed. Until such a line appears
/// the bar is left untouched, so a non-cooperating helper behaves as before.
#[derive(Default)]
pub struct MarkerProgress {
    fraction: Option<f64>,
}

impl ProgressSource for MarkerProgress {
    fn fraction(&mut self) -> Option<f64> {
        self.fraction
    }

    fn observe_line(&mut self, line: &str) {
        if let Some(parsed) = parse_marker(line) {
            let capped = parsed.min(PROGRESS_CAP);
            if self.fraction.is_none_or(|prev| capped > prev) {
                self.fraction = Some(capped);
            }
        }
    }
}

/// Parse the `<done>/<total>` fraction following a [`PROGRESS_MARKER`] token.
fn parse_marker(line: &str) -> Option<f64> {
    let token = line
        .split(PROGRESS_MARKER)
        .nth(1)?
        .split_whitespace()
        .next()?;
    let (done, total) = token.split_once('/')?;
    let done: f64 = done.parse().ok()?;
    let total: f64 = total.parse().ok()?;
    (total > 0.0).then(|| (done / total).clamp(0.0, 1.0))
}

/// Bytes currently available to unprivileged users on the filesystem holding
/// `path`.
fn available_bytes(path: &Path) -> Result<u64> {
    let stat = nix::sys::statvfs::statvfs(path)
        .map_err(|e| anyhow::anyhow!("failed to statvfs {}: {e}", path.display()))?;
    Ok(stat.blocks_available().saturating_mul(stat.fragment_size()))
}
