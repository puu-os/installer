// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::path::{Path, PathBuf};

const EFI_GLOBAL_VARIABLE: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";
const EFIVARS: &str = "/sys/firmware/efi/efivars";

pub struct SetupModeStatus {
    pub setup_mode: Option<bool>,
    pub secure_boot: Option<bool>,
    pub reason: String,
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

pub fn detect_setup_mode() -> SetupModeStatus {
    let efi_sys = Path::new("/sys/firmware/efi");
    if !efi_sys.exists() {
        return SetupModeStatus {
            setup_mode: None,
            secure_boot: None,
            reason: "UEFI firmware variables are unavailable".into(),
        };
    }

    let root = Path::new(EFIVARS);
    if !root.exists() {
        return SetupModeStatus {
            setup_mode: None,
            secure_boot: None,
            reason: "efivarfs is not mounted".into(),
        };
    }

    let setup_mode = efi_bool("SetupMode", root);
    let secure_boot = efi_bool("SecureBoot", root);

    if setup_mode.is_none() {
        return SetupModeStatus {
            setup_mode: None,
            secure_boot,
            reason: "SetupMode EFI variable is missing".into(),
        };
    }

    SetupModeStatus {
        setup_mode,
        secure_boot,
        reason: String::new(),
    }
}
