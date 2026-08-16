// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use anyhow::Result;

/// Fill an `N`-byte array with cryptographically secure random bytes.
pub(crate) fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut buf = [0u8; N];
    openssl::rand::rand_bytes(&mut buf)?;
    Ok(buf)
}

/// Flush all filesystem buffers to stable storage.
pub(crate) fn sync_all() {
    unsafe { libc::sync() };
}

/// Whether the current process has superuser privileges.
pub(crate) fn running_as_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// Format a byte count as a human-readable string (e.g. "1.5 GiB").
pub fn human_size(size_b: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut size = size_b as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

/// Sanitize a line of subprocess output for display: collapse carriage-return
/// overwrites, strip ANSI CSI escape sequences and other control characters,
/// turn tabs into spaces, and drop trailing whitespace.
pub fn clean_log_line(line: &str) -> String {
    let visible = line.rsplit('\r').next().unwrap_or(line);
    let mut out = String::new();
    let mut chars = visible.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        match c {
            '\t' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }

    out.truncate(out.trim_end().len());
    out
}

/// The user's login shell from `$SHELL`, falling back to `/bin/sh`.
pub fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

/// Validate a hostname accepted by the installer.
pub fn valid_hostname(name: &str) -> bool {
    if name.is_empty() || name.len() > 253 {
        return false;
    }

    name.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

/// Check whether `path` points to an executable regular file.
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Decode the octal escape sequences (`\NNN`) that `/proc/self/mountinfo`
/// uses for spaces and other special characters in mount paths.
pub(crate) fn decode_mountinfo_path(path: &str) -> String {
    let mut out = String::new();
    let mut chars = path.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            let octal: String = chars.by_ref().take(3).collect();
            if octal.len() == 3 {
                if let Ok(value) = u8::from_str_radix(&octal, 8) {
                    out.push(value as char);
                    continue;
                }
            }
            out.push('\\');
            out.push_str(&octal);
        } else {
            out.push(c);
        }
    }

    out
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MountInfo {
    pub mountpoint: String,
    pub source: String,
    pub fstype: String,
}

impl std::str::FromStr for MountInfo {
    type Err = ();

    fn from_str(line: &str) -> Result<Self, Self::Err> {
        let mut fields = line.split_whitespace();
        fields.next().ok_or(())?;
        fields.next().ok_or(())?;
        fields.next().ok_or(())?;
        fields.next().ok_or(())?;
        let mountpoint = fields.next().ok_or(())?;
        fields.next().ok_or(())?;
        loop {
            if fields.next().ok_or(())? == "-" {
                break;
            }
        }
        let fstype = fields.next().ok_or(())?;
        let source = fields.next().ok_or(())?;
        Ok(Self {
            mountpoint: decode_mountinfo_path(mountpoint),
            source: decode_mountinfo_path(source),
            fstype: fstype.to_string(),
        })
    }
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
