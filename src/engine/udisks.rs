// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! `UDisks2` access over the system bus.
//!
//! Used to list disks and read filesystem labels/UUIDs without linking
//! libudev, libblkid, or libostree.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use zbus::blocking::Connection;
use zbus::blocking::fdo::ObjectManagerProxy;
use zbus::names::OwnedInterfaceName;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

const UDISKS2_SERVICE: &str = "org.freedesktop.UDisks2";
const UDISKS2_PATH: &str = "/org/freedesktop/UDisks2";
const IFACE_BLOCK: &str = "org.freedesktop.UDisks2.Block";
const IFACE_DRIVE: &str = "org.freedesktop.UDisks2.Drive";
const IFACE_PARTITION: &str = "org.freedesktop.UDisks2.Partition";

pub(crate) struct Objects {
    objects: HashMap<OwnedObjectPath, HashMap<OwnedInterfaceName, HashMap<String, OwnedValue>>>,
}

impl Objects {
    pub(crate) fn fetch() -> Result<Self> {
        let conn = Connection::system().context("failed to connect to the system bus")?;
        let manager = ObjectManagerProxy::builder(&conn)
            .destination(UDISKS2_SERVICE)?
            .path(UDISKS2_PATH)?
            .build()
            .context("failed to create the UDisks2 object-manager proxy")?;
        let objects = manager
            .get_managed_objects()
            .context("failed to query UDisks2 managed objects")?;
        Ok(Self { objects })
    }

    fn iface(&self, path: &OwnedObjectPath, iface: &str) -> Option<&HashMap<String, OwnedValue>> {
        self.objects
            .get(path)?
            .iter()
            .find_map(|(name, props)| (name.as_str() == iface).then_some(props))
    }

    fn prop<'a>(
        &'a self,
        path: &OwnedObjectPath,
        iface: &str,
        key: &str,
    ) -> Option<&'a OwnedValue> {
        self.iface(path, iface)?.get(key)
    }

    pub(crate) fn block_by_device(&self, device: &Path) -> Option<&OwnedObjectPath> {
        let want = canonicalize_or(device);
        self.objects.keys().find(|path| {
            self.prop(path, IFACE_BLOCK, "Device")
                .and_then(owned_devnode)
                .is_some_and(|got| canonicalize_or(&got) == want)
        })
    }

    pub(crate) fn disks(&self) -> Vec<DiskInfo> {
        let mut disks = Vec::new();
        for path in self.objects.keys() {
            if self.iface(path, IFACE_PARTITION).is_some() {
                continue;
            }
            if owned_bool(self.prop(path, IFACE_BLOCK, "HintIgnore")).unwrap_or(false) {
                continue;
            }
            let Some(devnode) = self
                .prop(path, IFACE_BLOCK, "Device")
                .and_then(owned_devnode)
            else {
                continue;
            };
            let Some(sysname) = devnode.file_name() else {
                continue;
            };
            if is_virtual_block(sysname.as_encoded_bytes()) {
                continue;
            }

            let drive = self
                .prop(path, IFACE_BLOCK, "Drive")
                .and_then(owned_object_path);
            let model = drive
                .as_ref()
                .and_then(|drive| owned_string(self.prop(drive, IFACE_DRIVE, "Model")));
            let transport = drive.as_ref().and_then(|drive| {
                owned_string(self.prop(drive, IFACE_DRIVE, "ConnectionBus"))
                    .map(|bus| bus.to_ascii_uppercase())
            });
            disks.push(DiskInfo {
                path: devnode,
                model,
                size: owned_u64(self.prop(path, IFACE_BLOCK, "Size")).filter(|size| *size > 0),
                transport,
            });
        }
        disks
    }

    pub(crate) fn find_by_label(&self, label: &str) -> Option<PathBuf> {
        self.objects.keys().find_map(|path| {
            let got = owned_string(self.prop(path, IFACE_BLOCK, "IdLabel"))?;
            (got == label)
                .then(|| {
                    self.prop(path, IFACE_BLOCK, "Device")
                        .and_then(owned_devnode)
                })
                .flatten()
        })
    }

    pub(crate) fn probe(&self, device: &Path, key: &str) -> Option<String> {
        let path = self.block_by_device(device)?;
        match key {
            "UUID" => owned_string(self.prop(path, IFACE_BLOCK, "IdUUID")),
            "TYPE" => owned_string(self.prop(path, IFACE_BLOCK, "IdType")),
            "LABEL" => owned_string(self.prop(path, IFACE_BLOCK, "IdLabel")),
            "PARTUUID" | "PART_ENTRY_UUID" => {
                owned_string(self.prop(path, IFACE_PARTITION, "UUID"))
            }
            _ => None,
        }
    }
}

pub(crate) struct DiskInfo {
    pub(crate) path: PathBuf,
    pub(crate) model: Option<String>,
    pub(crate) size: Option<u64>,
    pub(crate) transport: Option<String>,
}

pub(crate) fn is_virtual_block(sysname: &[u8]) -> bool {
    ["loop", "ram", "zram", "sr", "fd", "md", "dm-"]
        .iter()
        .any(|prefix| sysname.starts_with(prefix.as_bytes()))
}

fn canonicalize_or(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn owned_string(value: Option<&OwnedValue>) -> Option<String> {
    String::try_from(value?.clone())
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn owned_u64(value: Option<&OwnedValue>) -> Option<u64> {
    u64::try_from(value?.clone()).ok()
}

fn owned_bool(value: Option<&OwnedValue>) -> Option<bool> {
    bool::try_from(value?.clone()).ok()
}

fn owned_object_path(value: &OwnedValue) -> Option<OwnedObjectPath> {
    OwnedObjectPath::try_from(value.clone()).ok()
}

fn owned_devnode(value: &OwnedValue) -> Option<PathBuf> {
    let bytes = Vec::<u8>::try_from(value.clone()).ok()?;
    let end = bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len());
    let text = std::str::from_utf8(bytes.get(..end)?).ok()?;
    (!text.is_empty()).then(|| PathBuf::from(text))
}
