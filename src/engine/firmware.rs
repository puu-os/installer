// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

const EFIVARS: &str = "/sys/firmware/efi/efivars";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EfiGuid([u8; 16]);

impl EfiGuid {
    pub(crate) const NIL: Self = Self([0; 16]);
    pub(crate) const GLOBAL_VARIABLE: Self = Self::from_bytes_le([
        0x61, 0xdf, 0xe4, 0x8b, 0xca, 0x93, 0xd2, 0x11, 0xaa, 0x0d, 0x00, 0xe0, 0x98, 0x03, 0x2b,
        0x8c,
    ]);

    pub(crate) const fn from_bytes_le(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
}

impl Default for EfiGuid {
    fn default() -> Self {
        Self::NIL
    }
}

impl From<uuid::Uuid> for EfiGuid {
    fn from(uuid: uuid::Uuid) -> Self {
        Self::from_bytes_le(uuid.to_bytes_le())
    }
}

impl From<EfiGuid> for uuid::Uuid {
    fn from(guid: EfiGuid) -> Self {
        Self::from_bytes_le(guid.0)
    }
}

impl From<EfiGuid> for [u8; 16] {
    fn from(guid: EfiGuid) -> Self {
        guid.0
    }
}

impl FromStr for EfiGuid {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(uuid::Uuid::parse_str(s)?.into())
    }
}

impl fmt::Display for EfiGuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        uuid::Uuid::from_bytes_le(self.0).hyphenated().fmt(f)
    }
}

impl AsRef<[u8; 16]> for EfiGuid {
    fn as_ref(&self) -> &[u8; 16] {
        &self.0
    }
}

/// Whether the install should enroll Secure Boot keys.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SecureBootMode {
    #[default]
    Enroll,
    Skip,
}

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
    let exact = root.join(format!("{name}-{}", EfiGuid::GLOBAL_VARIABLE));
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
