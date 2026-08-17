// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub path: PathBuf,
    pub model: Option<String>,
    pub size: Option<u64>,
    pub transport: Option<String>,
    pub busy_reason: Option<BusyReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceField {
    Uuid,
    PartUuid,
}

/// Stacked holder that keeps a disk from being an install target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingKind {
    Crypt,
    Lvm,
    Raid,
    InUse,
}

impl MappingKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Crypt => "crypt",
            Self::Lvm => "lvm",
            Self::Raid => "raid",
            Self::InUse => "in use",
        }
    }
}

impl std::fmt::Display for MappingKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a disk is unavailable as an install target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusyReason {
    Mounted { ident: PathBuf, mountpoint: PathBuf },
    Mapping { kind: MappingKind, ident: PathBuf },
    Swap { ident: PathBuf },
}

impl std::fmt::Display for BusyReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mounted { ident, mountpoint } => {
                write!(
                    f,
                    "mounted: {} -> {}",
                    ident.display(),
                    mountpoint.display()
                )
            }
            Self::Mapping { kind, ident } => write!(f, "{kind}: {}", ident.display()),
            Self::Swap { ident } => write!(f, "active swap: {}", ident.display()),
        }
    }
}

/// Map of mount source device node -> mount point, parsed from mountinfo.
fn mounted_sources() -> HashMap<PathBuf, PathBuf> {
    let mut map = HashMap::new();
    for info in crate::engine::util::Mounts::current() {
        if info.source.starts_with("/dev/") {
            map.entry(info.source).or_insert(info.mountpoint);
        }
    }
    map
}

/// Active swap device nodes, normalized through symlinks when possible.
fn swap_devices() -> HashSet<PathBuf> {
    let Ok(content) = std::fs::read_to_string("/proc/swaps") else {
        return HashSet::new();
    };
    content
        .lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().next())
        .map(Path::new)
        .filter(|source| source.starts_with("/dev/"))
        .map(|source| std::fs::canonicalize(source).unwrap_or_else(|_| source.to_path_buf()))
        .collect()
}

/// Partition sysfs names (e.g. `sda1`, `nvme0n1p2`) belonging to a disk.
fn partition_sysnames(disk: &OsStr) -> Vec<OsString> {
    let mut parts = Vec::new();
    if let Ok(entries) = std::fs::read_dir(Path::new("/sys/block").join(disk)) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.as_bytes().starts_with(disk.as_bytes())
                && entry.path().join("partition").is_file()
            {
                parts.push(name);
            }
        }
    }
    parts
}

/// Classify a device-mapper / md holder of a block device.
fn mapping_kind(holder: &OsStr) -> MappingKind {
    let sysfs = Path::new("/sys/class/block").join(holder);
    if sysfs.join("md").is_dir() {
        return MappingKind::Raid;
    }
    let uuid = std::fs::read_to_string(sysfs.join("dm/uuid")).unwrap_or_default();
    let uuid = uuid.trim();
    if uuid.starts_with("CRYPT-") {
        MappingKind::Crypt
    } else if uuid.starts_with("LVM-") {
        MappingKind::Lvm
    } else if uuid.contains("raid") {
        MappingKind::Raid
    } else {
        MappingKind::InUse
    }
}

/// First stacked holder (crypt/raid/lvm) of a block device, if any.
fn holder_mapping(sysname: &OsStr) -> Option<(MappingKind, PathBuf)> {
    let entries =
        std::fs::read_dir(Path::new("/sys/class/block").join(sysname).join("holders")).ok()?;
    let holder = entries.flatten().next()?;
    let name = holder.file_name();
    Some((mapping_kind(&name), Path::new("/dev").join(name)))
}

/// Report whether a disk (or any of its partitions) is mounted or stacked
/// under crypt/raid/lvm, replacing `lsblk` busy detection.
pub fn disk_busy(disk_path: impl AsRef<Path>) -> Option<BusyReason> {
    let disk = disk_path.as_ref().file_name()?;
    let mounts = mounted_sources();
    let swaps = swap_devices();

    for node in std::iter::once(disk.to_os_string()).chain(partition_sysnames(disk)) {
        let devnode = Path::new("/dev").join(&node);
        let canonical_devnode = std::fs::canonicalize(&devnode).unwrap_or_else(|_| devnode.clone());
        if swaps.contains(&canonical_devnode) {
            return Some(BusyReason::Swap { ident: devnode });
        }
        if let Some(mountpoint) = mounts.get(&devnode) {
            if !mountpoint.as_os_str().is_empty() {
                return Some(BusyReason::Mounted {
                    ident: devnode,
                    mountpoint: mountpoint.clone(),
                });
            }
        }
        if let Some((kind, ident)) = holder_mapping(&node) {
            return Some(BusyReason::Mapping { kind, ident });
        }
    }
    None
}

fn sysfs_disk_model(name: &OsStr) -> Option<String> {
    let from_attr = std::fs::read_to_string(
        Path::new("/sys/class/block")
            .join(name)
            .join("device/model"),
    )
    .ok()
    .map(|value| value.trim().to_string())
    .filter(|value| !value.is_empty());
    let from_prop = udev_property(name, "ID_MODEL").map(|value| value.replace('_', " "));
    from_attr.or(from_prop)
}

fn sysfs_disk_size(name: &OsStr) -> Option<u64> {
    let sectors = std::fs::read_to_string(Path::new("/sys/class/block").join(name).join("size"))
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())?;
    (sectors > 0).then(|| sectors.saturating_mul(512))
}

fn sysfs_disk_transport(name: &OsStr) -> Option<String> {
    if let Some(bus) = udev_property(name, "ID_BUS") {
        return Some(bus.to_ascii_uppercase());
    }
    name.as_bytes().starts_with(b"nvme").then(|| "NVME".into())
}

fn udev_property(name: &OsStr, key: &str) -> Option<String> {
    let sysfs = Path::new("/sys/class/block").join(name);
    let dev = std::fs::read_to_string(sysfs.join("dev")).ok()?;
    let dev = dev.trim();
    if dev.is_empty() {
        return None;
    }
    let data = std::fs::read_to_string(Path::new("/run/udev/data").join(format!("b{dev}"))).ok()?;
    for line in data.lines() {
        let Some(entry) = line.strip_prefix("E:") else {
            continue;
        };
        let Some(value) = entry
            .strip_prefix(key)
            .and_then(|rest| rest.strip_prefix('='))
        else {
            continue;
        };
        let value = value.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

fn sort_disks(mut disks: Vec<Device>) -> Vec<Device> {
    disks.sort_by(|a, b| a.path.cmp(&b.path));
    disks
}

fn by_id_uuid(dir: &str, device: &Path) -> Option<Uuid> {
    let device_path = std::fs::canonicalize(device).ok()?;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let entry_path = entry.path();
        if std::fs::canonicalize(&entry_path).ok().as_ref() != Some(&device_path) {
            continue;
        }
        if let Some(value) = entry_path.file_name().and_then(|name| name.to_str()) {
            return Uuid::parse_str(value.trim()).ok();
        }
    }
    None
}

fn sysfs_partuuid(device: &Path) -> Option<Uuid> {
    if let Some(partuuid) = by_id_uuid("/dev/disk/by-partuuid", device) {
        return Some(partuuid);
    }
    let name = device.file_name()?;
    let uevent =
        std::fs::read_to_string(Path::new("/sys/class/block").join(name).join("uevent")).ok()?;
    uevent.lines().find_map(|line| {
        line.strip_prefix("PARTUUID=")
            .and_then(|value| Uuid::parse_str(value.trim()).ok())
    })
}

trait DiskBackend {
    fn disks(&self) -> Result<Vec<Device>>;
    fn find_by_label(&self, label: &str) -> Option<PathBuf>;
    fn probe_uuid(&self, device: &Path, field: DeviceField) -> Option<Uuid>;
    fn probe_fstype(&self, device: &Path) -> Option<String>;
}

struct Sysfs;

impl DiskBackend for Sysfs {
    fn disks(&self) -> Result<Vec<Device>> {
        let mut disks = Vec::new();
        for entry in std::fs::read_dir("/sys/block")
            .map_err(|e| anyhow::anyhow!("failed to list /sys/block: {e}"))?
        {
            let entry = entry.map_err(|e| anyhow::anyhow!("failed to read /sys/block: {e}"))?;
            let name = entry.file_name();
            if crate::engine::udisks::is_virtual_block(name.as_bytes()) {
                continue;
            }
            if !Path::new("/sys/class/block")
                .join(&name)
                .join("dev")
                .is_file()
            {
                continue;
            }
            let path = Path::new("/dev").join(&name);
            disks.push(Device {
                model: sysfs_disk_model(&name),
                size: sysfs_disk_size(&name),
                transport: sysfs_disk_transport(&name),
                busy_reason: disk_busy(&path),
                path,
            });
        }
        Ok(sort_disks(disks))
    }

    fn find_by_label(&self, label: &str) -> Option<PathBuf> {
        let by_label = Path::new("/dev/disk/by-label").join(label);
        by_label.exists().then_some(by_label)
    }

    fn probe_uuid(&self, device: &Path, field: DeviceField) -> Option<Uuid> {
        match field {
            DeviceField::Uuid => by_id_uuid("/dev/disk/by-uuid", device),
            DeviceField::PartUuid => sysfs_partuuid(device),
        }
    }

    fn probe_fstype(&self, device: &Path) -> Option<String> {
        device
            .file_name()
            .and_then(|name| udev_property(name, "ID_FS_TYPE"))
    }
}

impl DiskBackend for crate::engine::udisks::Objects {
    fn disks(&self) -> Result<Vec<Device>> {
        let disks = crate::engine::udisks::Objects::disks(self)
            .into_iter()
            .map(|disk| {
                let transport = disk
                    .transport
                    .or_else(|| disk.path.file_name().and_then(sysfs_disk_transport));
                Device {
                    busy_reason: disk_busy(&disk.path),
                    path: disk.path,
                    model: disk.model,
                    size: disk.size,
                    transport,
                }
            })
            .collect();
        Ok(sort_disks(disks))
    }

    fn find_by_label(&self, label: &str) -> Option<PathBuf> {
        crate::engine::udisks::Objects::find_by_label(self, label)
    }

    fn probe_uuid(&self, device: &Path, field: DeviceField) -> Option<Uuid> {
        crate::engine::udisks::Objects::probe_uuid(self, device, field)
    }

    fn probe_fstype(&self, device: &Path) -> Option<String> {
        crate::engine::udisks::Objects::probe_fstype(self, device)
    }
}

pub(crate) enum DiskSource {
    Udisks(crate::engine::udisks::Objects),
    Sysfs,
}

impl DiskSource {
    pub(crate) fn open() -> Self {
        match crate::engine::udisks::Objects::fetch() {
            Ok(objects) => Self::Udisks(objects),
            Err(_) => Self::Sysfs,
        }
    }

    fn backend(&self) -> &dyn DiskBackend {
        match self {
            Self::Udisks(objects) => objects,
            Self::Sysfs => &Sysfs,
        }
    }

    pub(crate) fn find_by_label(&self, label: &str) -> Option<PathBuf> {
        DiskBackend::find_by_label(self, label)
    }

    pub(crate) fn probe_uuid(&self, device: &Path, field: DeviceField) -> Option<Uuid> {
        DiskBackend::probe_uuid(self, device, field)
    }

    pub(crate) fn probe_fstype(&self, device: &Path) -> Option<String> {
        DiskBackend::probe_fstype(self, device)
    }
}

impl DiskBackend for DiskSource {
    fn disks(&self) -> Result<Vec<Device>> {
        self.backend().disks()
    }

    fn find_by_label(&self, label: &str) -> Option<PathBuf> {
        self.backend().find_by_label(label).or_else(|| match self {
            Self::Udisks(_) => Sysfs.find_by_label(label),
            Self::Sysfs => None,
        })
    }

    fn probe_uuid(&self, device: &Path, field: DeviceField) -> Option<Uuid> {
        self.backend()
            .probe_uuid(device, field)
            .or_else(|| match self {
                Self::Udisks(_) => Sysfs.probe_uuid(device, field),
                Self::Sysfs => None,
            })
    }

    fn probe_fstype(&self, device: &Path) -> Option<String> {
        self.backend().probe_fstype(device).or_else(|| match self {
            Self::Udisks(_) => Sysfs.probe_fstype(device),
            Self::Sysfs => None,
        })
    }
}

pub fn list_disks() -> Result<Vec<Device>> {
    DiskBackend::disks(&DiskSource::open())
}

pub fn capture_uuid(device: impl AsRef<Path>) -> Result<Uuid> {
    let device = device.as_ref();
    DiskSource::open()
        .probe_uuid(device, DeviceField::Uuid)
        .ok_or_else(|| anyhow::anyhow!("unable to determine UUID for {}", device.display()))
}

pub fn capture_partuuid(device: impl AsRef<Path>) -> Result<Uuid> {
    let device = device.as_ref();
    DiskSource::open()
        .probe_uuid(device, DeviceField::PartUuid)
        .ok_or_else(|| anyhow::anyhow!("unable to determine PARTUUID for {}", device.display()))
}

pub fn drive_size_bytes(drive: impl AsRef<Path>) -> Result<u64> {
    let drive = drive.as_ref();
    // `/sys/class/block/<name>/size` is always expressed in 512-byte sectors.
    let name = drive
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid drive path '{}'", drive.display()))?;
    let sysfs = Path::new("/sys/class/block").join(name).join("size");
    let sectors: u64 = std::fs::read_to_string(&sysfs)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unable to determine size of target drive '{}'",
                drive.display()
            )
        })?;
    Ok(sectors.saturating_mul(512))
}

/// Wait until `/run/udev/queue` is gone, replacing `udevadm settle`.
pub fn wait_for_udev_settle(timeout: std::time::Duration) -> Result<()> {
    let queue = Path::new("/run/udev/queue");
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !queue.exists() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!("udev settle timed out after {}s", timeout.as_secs());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
