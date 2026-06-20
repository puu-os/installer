// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! Discovery of the bootc install source image and access to its sidecar
//! manifest (`image.manifest`).

use std::path::{Path, PathBuf};

use anyhow::Result;
use nix::mount::MsFlags;

use crate::engine::block::probe_value;
use crate::engine::config::Config;
use crate::engine::pipeline::{path_is_mount, unmount};

const MANIFEST_FILENAME: &str = "image.manifest";

pub fn running_in_container() -> bool {
    std::env::var("container").as_deref() == Ok("podman")
}

fn read_manifest(source_image: &Path) -> Option<serde_json::Value> {
    let manifest = source_image.parent()?.join(MANIFEST_FILENAME);
    let data = std::fs::read_to_string(&manifest).ok()?;
    serde_json::from_str(&data).ok()
}

fn manifest_field(source_image: Option<&Path>, key: &str) -> Option<String> {
    let json = read_manifest(source_image?)?;
    json.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

pub fn load_target_imgref(source_image: Option<&Path>, config: &Config) -> String {
    manifest_field(source_image, "target_imgref")
        .unwrap_or_else(|| config.image.target_img_ref.clone())
}

pub fn load_image_version(source_image: Option<&Path>) -> String {
    manifest_field(source_image, "version").unwrap_or_default()
}

pub fn image_manifest_metadata(source_image: &Path) -> serde_json::Value {
    read_manifest(source_image).unwrap_or(serde_json::Value::Null)
}

pub fn image_source_ref(source_image: &Path) -> String {
    if source_image.is_dir() {
        return format!("dir:{}", source_image.display());
    }
    let ref_name = manifest_field(Some(source_image), "imgref").unwrap_or_default();
    if ref_name.is_empty() {
        return format!("oci-archive:{}", source_image.display());
    }
    let name = ref_name.strip_prefix("oci:").unwrap_or(&ref_name);
    format!("oci-archive:{}:{}", source_image.display(), name)
}

pub fn default_source_image(config: &Config) -> Option<PathBuf> {
    let mount = PathBuf::from(&config.image.source_mount);
    let default_img = mount.join("image.tar");
    if default_img.is_file() {
        return Some(default_img);
    }
    if path_is_mount(&mount) {
        let _ = unmount_default_source(config);
    }
    if running_in_container() {
        return None;
    }

    // Resolve the payload label to a device node via libblkid (replaces `blkid -L`).
    let cache = blkid::cache::Cache::new().ok()?;
    cache.probe_all().ok()?;
    let dev = cache
        .find_dev_with_tag(blkid::tag::Tag::new(
            blkid::tag::SuperblockTag::Label,
            &config.image.source_label,
        ))
        .ok()??;
    let device = dev.name().to_path_buf();
    let device = device.to_str()?;

    // mount(2) needs an explicit filesystem type; probe it (replaces `mount -o ro`).
    let fstype = probe_value(device, false, "TYPE").ok()?;
    if fstype.is_empty() {
        return None;
    }

    std::fs::create_dir_all(&mount).ok()?;
    nix::mount::mount(
        Some(device),
        mount.as_path(),
        Some(fstype.as_str()),
        MsFlags::MS_RDONLY,
        None::<&str>,
    )
    .ok()?;

    if default_img.is_file() {
        Some(default_img)
    } else {
        let _ = unmount_default_source(config);
        None
    }
}

fn unmount_default_source(config: &Config) -> Result<()> {
    unmount(&PathBuf::from(&config.image.source_mount).to_string_lossy())
}
