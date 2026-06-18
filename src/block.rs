// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;

pub struct Device {
    pub path: String,
    pub model: String,
    pub size: String,
    pub transport: String,
    pub busy: bool,
    pub busy_reason: String,
}

/// Why a disk is unavailable as an install target.
pub enum BusyReason {
    Mounted { ident: String, mountpoint: String },
    Mapping { kind: String, ident: String },
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
        let mountpoint = pre.split_whitespace().nth(4).unwrap_or("");
        let source = post.split_whitespace().nth(1).unwrap_or("");
        if source.starts_with("/dev/") {
            map.entry(source.to_string())
                .or_insert_with(|| mountpoint.to_string());
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

fn busy_reason_text(reason: &BusyReason) -> String {
    match reason {
        BusyReason::Mounted { ident, mountpoint } => format!("mounted: {ident} → {mountpoint}"),
        BusyReason::Mapping { kind, ident } => format!("{kind}: {ident}"),
    }
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
        Some(s) if s > 0 => crate::util::human_size(s * 512),
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

        let (busy, busy_reason) = match disk_busy(&path) {
            Some(reason) => (true, busy_reason_text(&reason)),
            None => (false, String::new()),
        };

        disks.push(Device {
            model: disk_model(&dev),
            size: disk_size(&dev),
            transport: disk_transport(&dev, &sysname),
            busy,
            busy_reason,
            path,
        });
    }

    disks.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(disks)
}
