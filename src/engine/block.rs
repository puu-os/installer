// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, bail};

pub struct Device {
    pub path: String,
    pub model: String,
    pub size: String,
    pub transport: String,
    pub busy_reason: Option<String>,
}

/// Why a disk is unavailable as an install target.
#[derive(Debug)]
pub enum BusyReason {
    Mounted { ident: String, mountpoint: String },
    Mapping { kind: String, ident: String },
}

impl std::fmt::Display for BusyReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mounted { ident, mountpoint } => {
                write!(f, "mounted: {ident} → {mountpoint}")
            }
            Self::Mapping { kind, ident } => write!(f, "{kind}: {ident}"),
        }
    }
}

/// Map of mount source device node -> mount point, parsed from mountinfo.
fn mounted_sources() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(content) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return map;
    };
    for line in content.lines() {
        // mountinfo: "... <mountpoint(field 5)> ... - <fstype> <source> <opts>"
        let Some((pre, post)) = line.split_once(" - ") else {
            continue;
        };
        let mountpoint_raw = pre.split_whitespace().nth(4).unwrap_or("");
        let mountpoint = crate::engine::util::decode_mountinfo_path(mountpoint_raw);
        let source = post.split_whitespace().nth(1).unwrap_or("");
        if source.starts_with("/dev/") {
            map.entry(source.to_string()).or_insert_with(|| mountpoint);
        }
    }
    map
}

/// Partition sysfs names (e.g. `sda1`, `nvme0n1p2`) belonging to a disk.
fn partition_sysnames(disk: &str) -> Vec<String> {
    let mut parts = Vec::new();
    if let Ok(entries) = std::fs::read_dir(format!("/sys/block/{disk}")) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(disk) && entry.path().join("partition").is_file() {
                parts.push(name);
            }
        }
    }
    parts
}

/// Classify a device-mapper / md holder of a block device.
fn mapping_kind(holder: &str) -> String {
    if Path::new(&format!("/sys/class/block/{holder}/md")).is_dir() {
        return "raid".into();
    }
    let uuid =
        std::fs::read_to_string(format!("/sys/class/block/{holder}/dm/uuid")).unwrap_or_default();
    let uuid = uuid.trim();
    if uuid.starts_with("CRYPT-") {
        "crypt".into()
    } else if uuid.starts_with("LVM-") {
        "lvm".into()
    } else if uuid.contains("raid") {
        "raid".into()
    } else {
        "in use".into()
    }
}

/// First stacked holder (crypt/raid/lvm) of a block device, if any.
fn holder_mapping(sysname: &str) -> Option<(String, String)> {
    let entries = std::fs::read_dir(format!("/sys/class/block/{sysname}/holders")).ok()?;
    let holder = entries.flatten().next()?;
    let name = holder.file_name().to_string_lossy().into_owned();
    Some((mapping_kind(&name), format!("/dev/{name}")))
}

/// Report whether a disk (or any of its partitions) is mounted or stacked
/// under crypt/raid/lvm, replacing `lsblk` busy detection.
pub fn disk_busy(disk_path: &str) -> Option<BusyReason> {
    let disk = Path::new(disk_path).file_name()?.to_str()?.to_string();
    let mounts = mounted_sources();

    let mut nodes = vec![disk.clone()];
    nodes.extend(partition_sysnames(&disk));

    for node in &nodes {
        let devnode = format!("/dev/{node}");
        if let Some(mountpoint) = mounts.get(&devnode) {
            if !mountpoint.is_empty() {
                return Some(BusyReason::Mounted {
                    ident: devnode,
                    mountpoint: mountpoint.clone(),
                });
            }
        }
        if let Some((kind, ident)) = holder_mapping(node) {
            return Some(BusyReason::Mapping { kind, ident });
        }
    }
    None
}

/// Disk model, preferring the human-readable sysfs attribute.
fn disk_model(dev: &udev::Device) -> String {
    let from_attr = dev
        .attribute_value("device/model")
        .and_then(|s| s.to_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);
    let from_prop = dev
        .property_value("ID_MODEL")
        .and_then(|s| s.to_str())
        .map(|s| s.replace('_', " "))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    from_attr
        .or(from_prop)
        .unwrap_or_else(|| "(unknown)".into())
}

fn disk_size(dev: &udev::Device) -> String {
    let sectors = dev
        .attribute_value("size")
        .and_then(|s| s.to_str())
        .and_then(|s| s.trim().parse::<u64>().ok());
    match sectors {
        Some(s) if s > 0 => crate::engine::util::human_size(s * 512),
        _ => "—".into(),
    }
}

fn disk_transport(dev: &udev::Device, sysname: &str) -> String {
    if let Some(bus) = dev.property_value("ID_BUS").and_then(|s| s.to_str()) {
        if !bus.is_empty() {
            return bus.to_uppercase();
        }
    }
    if sysname.starts_with("nvme") {
        return "NVME".into();
    }
    "—".into()
}

pub fn list_disks() -> Result<Vec<Device>> {
    let mut enumerator = udev::Enumerator::new()?;
    enumerator.match_subsystem("block")?;

    let mut disks = Vec::new();
    for dev in enumerator.scan_devices()? {
        if dev.devtype().and_then(|s| s.to_str()) != Some("disk") {
            continue;
        }
        let sysname = dev.sysname().to_string_lossy().into_owned();
        // Skip virtual / non-installable block devices.
        if ["loop", "ram", "zram", "sr", "fd", "md", "dm-"]
            .iter()
            .any(|p| sysname.starts_with(p))
        {
            continue;
        }

        let path = dev.devnode().map_or_else(
            || format!("/dev/{sysname}"),
            |p| p.to_string_lossy().into_owned(),
        );

        disks.push(Device {
            model: disk_model(&dev),
            size: disk_size(&dev),
            transport: disk_transport(&dev, &sysname),
            busy_reason: disk_busy(&path)
                .as_ref()
                .map(std::string::ToString::to_string),
            path,
        });
    }

    disks.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(disks)
}

pub(crate) fn probe_value(device: &str, partitions: bool, key: &str) -> Result<String> {
    let probe = blkid::prober::Prober::new_from_filename(device)
        .map_err(|e| anyhow::anyhow!("failed to probe {device}: {e}"))?;
    if partitions {
        probe
            .enable_partitions(true)
            .map_err(|e| anyhow::anyhow!("failed to enable partition probing for {device}: {e}"))?;
    }
    probe
        .do_safe_probe()
        .map_err(|e| anyhow::anyhow!("failed to probe {device}: {e}"))?;
    Ok(probe
        .lookup_value(key)
        .unwrap_or_default()
        .trim()
        .to_string())
}

pub fn capture_uuid(device: &str) -> Result<String> {
    let uuid = probe_value(device, false, "UUID")?;
    if uuid.is_empty() {
        bail!("unable to determine UUID for {device}");
    }
    Ok(uuid)
}

pub fn capture_partuuid(device: &str) -> Result<String> {
    let partuuid = probe_value(device, true, "PART_ENTRY_UUID")?;
    if !partuuid.is_empty() {
        return Ok(partuuid);
    }

    let partuuid = probe_value(device, false, "PARTUUID")?;
    if !partuuid.is_empty() {
        return Ok(partuuid);
    }

    let name = Path::new(device)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid partition device '{device}'"))?;
    let uevent = std::fs::read_to_string(format!("/sys/class/block/{name}/uevent"));
    if let Ok(uevent) = uevent {
        for line in uevent.lines() {
            if let Some(partuuid) = line.strip_prefix("PARTUUID=") {
                return Ok(partuuid.to_string());
            }
        }
    }

    let device_path = std::fs::canonicalize(device)
        .map_err(|e| anyhow::anyhow!("failed to canonicalize {device}: {e}"))?;
    for entry in std::fs::read_dir("/dev/disk/by-partuuid")
        .into_iter()
        .flatten()
        .flatten()
    {
        let entry_path = entry.path();
        let target = std::fs::canonicalize(&entry_path);
        if target.as_ref().ok() != Some(&device_path) {
            continue;
        }

        if let Some(partuuid) = entry_path.file_name().and_then(|name| name.to_str()) {
            return Ok(partuuid.to_string());
        }
    }

    bail!("unable to determine PARTUUID for {device}");
}

pub fn drive_size_bytes(drive: &str) -> Result<u64> {
    // `/sys/class/block/<name>/size` is always expressed in 512-byte sectors.
    let name = Path::new(drive)
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid drive path '{drive}'"))?;
    let sysfs = format!("/sys/class/block/{name}/size");
    let sectors: u64 = std::fs::read_to_string(&sysfs)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| anyhow::anyhow!("unable to determine size of target drive '{drive}'"))?;
    Ok(sectors * 512)
}
