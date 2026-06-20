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
}

/// Highest fraction a [`ProgressSource`] may report before the step actually
/// finishes, so an inexact estimate never paints a full bar prematurely.
const PROGRESS_CAP: f64 = 0.98;

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

/// Bytes currently available to unprivileged users on the filesystem holding
/// `path`.
fn available_bytes(path: &Path) -> Result<u64> {
    let stat = nix::sys::statvfs::statvfs(path)
        .map_err(|e| anyhow::anyhow!("failed to statvfs {}: {e}", path.display()))?;
    Ok(stat.blocks_available().saturating_mul(stat.fragment_size()))
}
