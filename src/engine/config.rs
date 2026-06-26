// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    pub branding: Branding,
    pub partitions: Partitions,
    pub labels: Labels,
    pub image: Image,
    pub identity: Identity,
    pub flatpak: Flatpak,
    pub k3s: K3s,
    pub homed: Homed,
    pub serial_console: HashMap<String, String>,
    pub kernel: Kernel,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Branding {
    pub name: String,
}

impl Default for Branding {
    fn default() -> Self {
        Self { name: "Puu".into() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Partitions {
    pub efi: String,
    pub root_min: String,
    pub home: String,
}

impl Default for Partitions {
    fn default() -> Self {
        Self {
            efi: "512M".into(),
            root_min: "64G".into(),
            home: "256G".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Labels {
    pub root: String,
    pub home: String,
}

impl Default for Labels {
    fn default() -> Self {
        Self {
            root: "puu-root".into(),
            home: "puu-home".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Image {
    pub target_img_ref: String,
    pub source_label: String,
    pub source_mount: String,
}

impl Default for Image {
    fn default() -> Self {
        Self {
            target_img_ref: "quay.io/puu/puu:latest".into(),
            source_label: "PAYLOAD".into(),
            source_mount: "/run/puu-installer/payload".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Identity {
    pub keymap: String,
    pub locale: String,
    pub timezone: String,
    pub hostname: String,
}

impl Default for Identity {
    fn default() -> Self {
        Self {
            keymap: "us".into(),
            locale: "C.UTF-8".into(),
            timezone: "UTC".into(),
            hostname: "puu".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Flatpak {
    pub manifest_path: String,
    pub sync_helper: String,
}

impl Default for Flatpak {
    fn default() -> Self {
        Self {
            manifest_path: "/usr/share/puu/flatpaks.list".into(),
            sync_helper: "/usr/libexec/puu/flatpak-sync".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct K3s {
    pub helper: String,
    pub role: String,
    pub server_url: String,
    pub node_name: String,
}

impl Default for K3s {
    fn default() -> Self {
        Self {
            helper: "/usr/libexec/puu/configure-k3s-cluster".into(),
            role: "server".into(),
            server_url: String::new(),
            node_name: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Homed {
    pub groups: Vec<String>,
}

impl Default for Homed {
    fn default() -> Self {
        Self {
            groups: vec![
                "adm".into(),
                "audio".into(),
                "cdrom".into(),
                "dip".into(),
                "input".into(),
                "lpadmin".into(),
                "netdev".into(),
                "plugdev".into(),
                "render".into(),
                "sudo".into(),
                "video".into(),
                "wheel".into(),
                "realtime".into(),
                "uucp".into(),
            ],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Kernel {
    pub extra_arm64_args: Vec<String>,
}

impl Default for Kernel {
    fn default() -> Self {
        Self {
            extra_arm64_args: vec!["cma=256M".into()],
        }
    }
}

/// Maximum byte length of an XFS volume label, enforced by `mkfs.xfs`.
const XFS_LABEL_MAX: usize = 12;
/// Maximum byte length of an ext4 volume label, enforced by `mke2fs`.
const EXT4_LABEL_MAX: usize = 16;

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let cfg: Config = serde_norway::from_str(&text)
            .with_context(|| format!("failed to parse config {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Reject config-derived identifiers that downstream tools or firmware would
    /// later refuse, so the installer fails at startup rather than mid-install.
    fn validate(&self) -> Result<()> {
        let name = &self.branding.name;
        if name.is_empty() {
            anyhow::bail!("branding.name must not be empty");
        }
        if name.chars().any(|c| c as u32 > 0xFFFF) {
            anyhow::bail!(
                "branding.name {name:?} must contain only Basic Multilingual Plane characters"
            );
        }
        validate_label("labels.root", &self.labels.root, XFS_LABEL_MAX)?;
        validate_label("labels.home", &self.labels.home, EXT4_LABEL_MAX)?;
        Ok(())
    }
}

fn validate_label(field: &str, value: &str, max: usize) -> Result<()> {
    if value.is_empty() {
        anyhow::bail!("{field} must not be empty");
    }
    if value.len() > max {
        anyhow::bail!(
            "{field} {value:?} is {} bytes, exceeding the {max}-byte filesystem label limit",
            value.len()
        );
    }
    Ok(())
}
