// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

/// Format a byte count as a human-readable string (e.g. "1.5 GiB").
pub fn human_size(size_b: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut f = size_b as f64;
    for (i, unit) in units.iter().enumerate() {
        if f < 1024.0 || i == units.len() - 1 {
            return format!("{f:.1} {unit}");
        }
        f /= 1024.0;
    }
    // Compiler cannot prove the loop always returns; use last unit as safety net.
    format!("{f:.1} PiB")
}

/// Check whether a command exists in `$PATH`.
pub fn which_exists(cmd: &str) -> bool {
    std::process::Command::new("sh")
        .args(["-c", &format!("command -v {cmd}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}
