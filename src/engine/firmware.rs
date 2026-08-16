// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::path::{Path, PathBuf};

const EFI_GLOBAL_VARIABLE: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";
const EFIVARS: &str = "/sys/firmware/efi/efivars";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupMode {
    On { secure_boot: Option<bool> },
    Off,
    Unknown { reason: String },
}

impl SetupMode {
    pub const fn is_on(&self) -> bool {
        matches!(self, Self::On { .. })
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Unknown { reason } if !reason.is_empty() => Some(reason.as_str()),
            _ => None,
        }
    }
}

fn efi_variable_path(name: &str, root: &Path) -> Option<PathBuf> {
    let exact = root.join(format!("{name}-{EFI_GLOBAL_VARIABLE}"));
    if exact.exists() {
        return Some(exact);
    }
    let prefix = format!("{name}-");
    std::fs::read_dir(root)
        .ok()?
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix))
        })
        .min()
}

fn efi_bool(name: &str, root: &Path) -> Option<bool> {
    let path = efi_variable_path(name, root)?;
    let data = std::fs::read(&path).ok()?;
    if data.is_empty() {
        return None;
    }
    let value = if data.len() > 4 {
        data[4]
    } else {
        data[data.len() - 1]
    };
    Some(value != 0)
}

pub fn detect_setup_mode() -> SetupMode {
    let efi_sys = Path::new("/sys/firmware/efi");
    if !efi_sys.exists() {
        return SetupMode::Unknown {
            reason: "UEFI firmware variables are unavailable".into(),
        };
    }

    let root = Path::new(EFIVARS);
    if !root.exists() {
        return SetupMode::Unknown {
            reason: "efivarfs is not mounted".into(),
        };
    }

    match efi_bool("SetupMode", root) {
        None => SetupMode::Unknown {
            reason: "SetupMode EFI variable is missing".into(),
        },
        Some(true) => SetupMode::On {
            secure_boot: efi_bool("SecureBoot", root),
        },
        Some(false) => SetupMode::Off,
    }
}
