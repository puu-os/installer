// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::collections::HashMap;
use std::path::Path;

use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Branding {
    pub name: String,
}

impl Default for Branding {
    fn default() -> Self {
        Self { name: "Puu".into() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Partitions {
    pub efi: PartitionSize,
    pub root_min: PartitionSize,
    pub home: PartitionSize,
}

impl Default for Partitions {
    fn default() -> Self {
        Self {
            efi: "512M".parse().expect("valid efi size"),
            root_min: "64G".parse().expect("valid root_min size"),
            home: "256G".parse().expect("valid home size"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Image {
    pub target_img_ref: String,
    pub expected_digest: String,
    pub source_label: String,
    pub source_mount: String,
}

impl Default for Image {
    fn default() -> Self {
        Self {
            target_img_ref: "quay.io/puu/puu:latest".into(),
            expected_digest: String::new(),
            source_label: "PAYLOAD".into(),
            source_mount: "/run/puu-installer/payload".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Identity {
    pub keymap: String,
    pub locale: String,
    pub timezone: String,
    pub hostname: String,
}

impl Default for Identity {
    fn default() -> Self {
        let timezone = match iana_time_zone::get_timezone() {
            Ok(timezone) => timezone,
            Err(_) => "UTC".into(),
        };
        Self {
            keymap: "us".into(),
            locale: "C.UTF-8".into(),
            timezone,
            hostname: "puu".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum K3sRole {
    #[default]
    Server,
    Agent,
}

impl K3sRole {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Agent => "agent",
        }
    }
}

impl fmt::Display for K3sRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for K3sRole {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for K3sRole {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "server" => Ok(Self::Server),
            "agent" => Ok(Self::Agent),
            _ => anyhow::bail!("k3s.role must be \"server\" or \"agent\", got {s:?}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PartitionSize {
    raw: String,
    bytes: u64,
}

impl PartitionSize {
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl FromStr for PartitionSize {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = crate::engine::partition::part_size_bytes(s)?;
        if bytes == 0 {
            anyhow::bail!("partition size must be greater than zero");
        }
        Ok(Self {
            raw: s.strip_prefix('+').unwrap_or(s).to_string(),
            bytes,
        })
    }
}

impl<'de> Deserialize<'de> for PartitionSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for PartitionSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for PartitionSize {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct K3s {
    pub helper: String,
    pub role: K3sRole,
    pub server_url: String,
    pub node_name: String,
}

impl Default for K3s {
    fn default() -> Self {
        Self {
            helper: "/usr/libexec/puu/configure-k3s-cluster".into(),
            role: K3sRole::Server,
            server_url: String::new(),
            node_name: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
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
        validate_digest(&self.image.expected_digest)?;
        validate_single_line("identity.keymap", &self.identity.keymap)?;
        validate_single_line("identity.locale", &self.identity.locale)?;
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

fn validate_digest(value: &str) -> Result<()> {
    if value.is_empty() {
        return Ok(());
    }
    let Some(hex) = value.strip_prefix("sha256:") else {
        anyhow::bail!("image.expectedDigest must use the sha256:<hex> format");
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("image.expectedDigest must contain exactly 64 hexadecimal digits");
    }
    Ok(())
}

fn validate_single_line(field: &str, value: &str) -> Result<()> {
    if value.contains(['\n', '\r']) {
        anyhow::bail!("{field} must not contain newline characters");
    }
    Ok(())
}
