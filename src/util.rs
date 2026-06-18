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

/// Whether the controlling terminal's locale advertises UTF-8.
pub fn terminal_is_utf8() -> bool {
    for var in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Ok(value) = std::env::var(var) {
            if !value.is_empty() {
                let value = value.to_ascii_uppercase();
                return value.contains("UTF-8") || value.contains("UTF8");
            }
        }
    }
    false
}

/// Check whether `path` points to an executable regular file.
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Check whether a command exists in `$PATH`, by searching `$PATH` directly
/// instead of shelling out to `sh -c 'command -v'`.
pub fn which_exists(cmd: &str) -> bool {
    if cmd.contains('/') {
        return is_executable(std::path::Path::new(cmd));
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| is_executable(&dir.join(cmd))))
}
