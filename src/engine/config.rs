// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
    pub serial_console: HashMap<Arch, KernelArg>,
    pub kernel: Kernel,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Branding {
    pub name: BrandName,
}

impl Default for Branding {
    fn default() -> Self {
        Self {
            name: BrandName {
                value: "Puu".into(),
            },
        }
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
    pub root: VolumeLabel<XFS_LABEL_MAX>,
    pub home: VolumeLabel<EXT4_LABEL_MAX>,
}

impl Default for Labels {
    fn default() -> Self {
        Self {
            root: VolumeLabel {
                value: "puu-root".into(),
            },
            home: VolumeLabel {
                value: "puu-home".into(),
            },
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Image {
    pub target_img_ref: ImageRef,
    #[serde(default, deserialize_with = "deserialize_optional_parsed")]
    pub expected_digest: Option<Digest>,
    pub source_label: VolumeLabel<EXT4_LABEL_MAX>,
    pub source_mount: PathBuf,
}

impl Default for Image {
    fn default() -> Self {
        Self {
            target_img_ref: ImageRef::default(),
            expected_digest: None,
            source_label: VolumeLabel {
                value: "PAYLOAD".into(),
            },
            source_mount: "/run/puu-installer/payload".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Identity {
    pub keymap: SingleLine,
    pub locale: SingleLine,
    pub timezone: Timezone,
    pub hostname: Hostname,
}

impl Default for Identity {
    fn default() -> Self {
        let timezone = iana_time_zone::get_timezone()
            .ok()
            .and_then(|timezone| timezone.parse().ok())
            .unwrap_or(Timezone {
                value: "UTC".into(),
            });
        Self {
            keymap: SingleLine { value: "us".into() },
            locale: SingleLine {
                value: "C.UTF-8".into(),
            },
            timezone,
            hostname: Hostname::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UserShell {
    #[default]
    Bash,
    Zsh,
}

impl UserShell {
    pub fn path(self) -> &'static Path {
        match self {
            Self::Bash => Path::new("/bin/bash"),
            Self::Zsh => Path::new("/bin/zsh"),
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Flatpak {
    pub manifest_path: PathBuf,
    pub sync_helper: PathBuf,
}

impl Default for Flatpak {
    fn default() -> Self {
        Self {
            manifest_path: "/usr/share/puu/flatpaks.list".into(),
            sync_helper: "/usr/libexec/puu/flatpak-sync".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
pub enum Arch {
    #[serde(rename = "x86_64")]
    X86_64,
    #[serde(rename = "aarch64")]
    Aarch64,
}

impl Arch {
    pub fn host() -> Result<Self> {
        std::env::consts::ARCH.parse()
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }

    #[must_use]
    pub fn efi_suffix(self) -> &'static str {
        match self {
            Self::X86_64 => "x64",
            Self::Aarch64 => "aa64",
        }
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Arch {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "x86_64" => Ok(Self::X86_64),
            "aarch64" => Ok(Self::Aarch64),
            _ => anyhow::bail!("unsupported architecture: {s}"),
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

impl FromStr for K3sRole {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "server" => Ok(Self::Server),
            "agent" => Ok(Self::Agent),
            _ => anyhow::bail!("k3s role must be \"server\" or \"agent\", got {s:?}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hostname {
    value: String,
}

impl Hostname {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl Default for Hostname {
    fn default() -> Self {
        Self {
            value: "puu".into(),
        }
    }
}

impl AsRef<str> for Hostname {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for Hostname {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Hostname {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() || s.len() > 253 {
            anyhow::bail!("invalid hostname: {s}");
        }
        let valid = s.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        });
        if !valid {
            anyhow::bail!("invalid hostname: {s}");
        }
        Ok(Self {
            value: s.to_string(),
        })
    }
}

impl<'de> Deserialize<'de> for Hostname {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Username {
    value: String,
}

impl Username {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl AsRef<str> for Username {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for Username {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Username {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut chars = s.chars();
        let Some(first) = chars.next() else {
            anyhow::bail!("username must not be empty");
        };
        if (!first.is_ascii_lowercase() && first != '_')
            || chars.any(|ch| {
                !ch.is_ascii_lowercase() && !ch.is_ascii_digit() && ch != '_' && ch != '-'
            })
        {
            anyhow::bail!("invalid username: {s}");
        }
        Ok(Self {
            value: s.to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupName {
    value: String,
}

impl GroupName {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl AsRef<str> for GroupName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for GroupName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for GroupName {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut chars = s.chars();
        let Some(first) = chars.next() else {
            anyhow::bail!("group name must not be empty");
        };
        if (!first.is_ascii_lowercase() && first != '_')
            || chars.any(|ch| {
                !ch.is_ascii_lowercase() && !ch.is_ascii_digit() && ch != '_' && ch != '-'
            })
        {
            anyhow::bail!("invalid group name: {s}");
        }
        Ok(Self {
            value: s.to_string(),
        })
    }
}

impl<'de> Deserialize<'de> for GroupName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl Serialize for GroupName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest {
    value: String,
}

impl Digest {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Path of this digest's blob inside an OCI archive (`blobs/sha256/<hex>`).
    #[must_use]
    pub fn oci_blob_path(&self) -> PathBuf {
        let hex = self.value.strip_prefix("sha256:").unwrap_or(&self.value);
        Path::new("blobs").join("sha256").join(hex)
    }
}

impl AsRef<str> for Digest {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    value: String,
}

impl ImageRef {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }

    #[must_use]
    pub fn dir(path: &Path) -> Self {
        Self {
            value: format!("dir:{}", path.display()),
        }
    }

    #[must_use]
    pub fn oci_archive(path: &Path, name: Option<&str>) -> Self {
        Self {
            value: match name {
                Some(name) => format!("oci-archive:{}:{}", path.display(), name),
                None => format!("oci-archive:{}", path.display()),
            },
        }
    }
}

impl Default for ImageRef {
    fn default() -> Self {
        Self {
            value: "quay.io/puu/puu:latest".into(),
        }
    }
}

impl AsRef<str> for ImageRef {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ImageRef {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() || s.chars().any(|c| c.is_whitespace() || c.is_control()) {
            anyhow::bail!("invalid image ref: {s}");
        }
        if let Some(path) = s.strip_prefix("dir:") {
            if path.is_empty() {
                anyhow::bail!("dir: image ref is missing a path");
            }
            return Ok(Self {
                value: s.to_string(),
            });
        }
        if let Some(rest) = s.strip_prefix("oci-archive:") {
            if rest.is_empty() {
                anyhow::bail!("oci-archive: image ref is missing a path");
            }
            return Ok(Self {
                value: s.to_string(),
            });
        }
        if let Some(name) = s.strip_prefix("oci:") {
            if name.is_empty() {
                anyhow::bail!("oci: image ref is missing a name");
            }
            return Ok(Self {
                value: s.to_string(),
            });
        }
        if s.starts_with(':') || s.starts_with('@') || s.starts_with('/') {
            anyhow::bail!("invalid image ref: {s}");
        }
        if !s.chars().any(|c| c.is_ascii_alphanumeric()) {
            anyhow::bail!("invalid image ref: {s}");
        }
        Ok(Self {
            value: s.to_string(),
        })
    }
}

impl<'de> Deserialize<'de> for ImageRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl FromStr for Digest {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let Some(hex) = s.strip_prefix("sha256:") else {
            anyhow::bail!("digest must use the sha256:<hex> format");
        };
        if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            anyhow::bail!("digest must contain exactly 64 hexadecimal digits");
        }
        Ok(Self {
            value: s.to_string(),
        })
    }
}

fn deserialize_optional_parsed<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: FromStr,
    T::Err: fmt::Display,
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    match value.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => s.parse().map(Some).map_err(serde::de::Error::custom),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelArg {
    value: String,
}

impl KernelArg {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl AsRef<str> for KernelArg {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for KernelArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for KernelArg {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() || s.chars().any(|c| c.is_whitespace() || c.is_control()) {
            anyhow::bail!("invalid kernel arg: {s}");
        }
        Ok(Self {
            value: s.to_string(),
        })
    }
}

impl<'de> Deserialize<'de> for KernelArg {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrandName {
    value: String,
}

impl BrandName {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl AsRef<str> for BrandName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for BrandName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BrandName {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            anyhow::bail!("branding.name must not be empty");
        }
        if s.chars().any(|c| c as u32 > 0xFFFF) {
            anyhow::bail!(
                "branding.name {s:?} must contain only Basic Multilingual Plane characters"
            );
        }
        Ok(Self {
            value: s.to_string(),
        })
    }
}

impl<'de> Deserialize<'de> for BrandName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SingleLine {
    value: String,
}

impl SingleLine {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl AsRef<str> for SingleLine {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for SingleLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SingleLine {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.contains(['\n', '\r']) {
            anyhow::bail!("value must not contain newline characters");
        }
        Ok(Self {
            value: s.to_string(),
        })
    }
}

impl<'de> Deserialize<'de> for SingleLine {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timezone {
    value: String,
}

impl Timezone {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl AsRef<str> for Timezone {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for Timezone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Timezone {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            anyhow::bail!("timezone must not be empty");
        }
        let relative = Path::new(s);
        if !relative
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        {
            anyhow::bail!("invalid timezone: {s}");
        }
        Ok(Self {
            value: s.to_string(),
        })
    }
}

impl<'de> Deserialize<'de> for Timezone {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerUrl {
    value: String,
}

impl ServerUrl {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl AsRef<str> for ServerUrl {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for ServerUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ServerUrl {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() || s.chars().any(char::is_whitespace) {
            anyhow::bail!("server URL must be a single non-empty token");
        }
        Ok(Self {
            value: s.to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeLabel<const MAX: usize> {
    value: String,
}

impl<const MAX: usize> VolumeLabel<MAX> {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl<const MAX: usize> AsRef<str> for VolumeLabel<MAX> {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl<const MAX: usize> fmt::Display for VolumeLabel<MAX> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<const MAX: usize> FromStr for VolumeLabel<MAX> {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            anyhow::bail!("volume label must not be empty");
        }
        if s.len() > MAX {
            anyhow::bail!(
                "volume label {s:?} is {} bytes, exceeding the {MAX}-byte filesystem label limit",
                s.len()
            );
        }
        Ok(Self {
            value: s.to_string(),
        })
    }
}

impl<'de, const MAX: usize> Deserialize<'de> for VolumeLabel<MAX> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionSize {
    bytes: u64,
}

impl PartitionSize {
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

const PART_SIZE_UNITS: &[(&str, u64)] = &[
    ("K", 1024),
    ("M", 1024 * 1024),
    ("G", 1024 * 1024 * 1024),
    ("T", 1024 * 1024 * 1024 * 1024),
    ("P", 1024 * 1024 * 1024 * 1024 * 1024),
];

impl FromStr for PartitionSize {
    type Err = anyhow::Error;

    fn from_str(size: &str) -> Result<Self, Self::Err> {
        let s = size.strip_prefix('+').unwrap_or(size);
        let (num_str, unit) = s
            .split_at_checked(s.len().saturating_sub(1))
            .ok_or_else(|| anyhow::anyhow!("unsupported partition size: {size}"))?;
        let num: u64 = num_str
            .parse()
            .map_err(|_| anyhow::anyhow!("unsupported partition size: {size}"))?;
        let multiplier = PART_SIZE_UNITS
            .iter()
            .find(|(u, _)| *u == unit)
            .map(|(_, m)| *m)
            .ok_or_else(|| anyhow::anyhow!("unsupported partition unit: {unit}"))?;
        let bytes = num
            .checked_mul(multiplier)
            .ok_or_else(|| anyhow::anyhow!("partition size is too large: {size}"))?;
        if bytes == 0 {
            anyhow::bail!("partition size must be greater than zero");
        }
        Ok(Self { bytes })
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

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct K3s {
    pub helper: PathBuf,
    pub role: K3sRole,
    #[serde(default, deserialize_with = "deserialize_optional_parsed")]
    pub server_url: Option<ServerUrl>,
    #[serde(default, deserialize_with = "deserialize_optional_parsed")]
    pub node_name: Option<Hostname>,
}

impl Default for K3s {
    fn default() -> Self {
        Self {
            helper: "/usr/libexec/puu/configure-k3s-cluster".into(),
            role: K3sRole::Server,
            server_url: None,
            node_name: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Homed {
    pub groups: Vec<GroupName>,
}

impl Default for Homed {
    fn default() -> Self {
        Self {
            groups: [
                "adm", "audio", "cdrom", "dip", "input", "lpadmin", "netdev", "plugdev", "render",
                "sudo", "video", "wheel", "realtime", "uucp",
            ]
            .into_iter()
            .map(|name| GroupName { value: name.into() })
            .collect(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Kernel {
    pub extra_arm64_args: Vec<KernelArg>,
}

impl Default for Kernel {
    fn default() -> Self {
        Self {
            extra_arm64_args: vec![KernelArg {
                value: "cma=256M".into(),
            }],
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
        serde_norway::from_str(&text)
            .with_context(|| format!("failed to parse config {}", path.display()))
    }
}
