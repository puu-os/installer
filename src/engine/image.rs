// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! Discovery of the bootc install source image and access to its sidecar
//! manifest (`image.manifest`).

use std::path::{Path, PathBuf};

use anyhow::Result;
use nix::mount::MsFlags;

use crate::engine::block::probe_value;
use crate::engine::config::{Config, Digest, ImageRef};
use crate::engine::pipeline::{path_is_mount, unmount};

const MANIFEST_FILENAME: &str = "image.manifest";

/// Compressed-to-on-disk expansion estimate (numerator/denominator) applied to
/// the OCI layer sizes when the sidecar manifest carries no `installSize`.
const LAYER_EXPANSION_NUM: u64 = 22;
const LAYER_EXPANSION_DEN: u64 = 10;

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

pub fn load_target_imgref(source_image: Option<&Path>, config: &Config) -> Result<ImageRef> {
    match manifest_field(source_image, "target_imgref") {
        Some(value) => value.parse(),
        None => Ok(config.image.target_img_ref.clone()),
    }
}

pub fn load_image_version(source_image: Option<&Path>) -> Option<String> {
    manifest_field(source_image, "version").filter(|version| !version.is_empty())
}

/// Estimated bytes the bootc deployment writes to the target filesystem, used
/// to follow its otherwise-opaque progress. Prefers an explicit `installSize`
/// from the sidecar manifest, then falls back to scaling the OCI layer sizes.
/// `None` when no local source image is available.
pub fn install_size_estimate(source_image: Option<&Path>) -> Option<u64> {
    let source_image = source_image?;
    if let Some(size) = manifest_install_size(source_image) {
        return Some(size);
    }
    let raw = raw_manifest(source_image).ok()?;
    let total = compressed_layer_total(&raw)?;
    Some(total.saturating_mul(LAYER_EXPANSION_NUM) / LAYER_EXPANSION_DEN)
}

fn manifest_install_size(source_image: &Path) -> Option<u64> {
    let value = read_manifest(source_image)?;
    let field = value.get("installSize")?;
    field
        .as_u64()
        .or_else(|| field.as_str().and_then(|s| s.parse().ok()))
}

/// Sum of the compressed layer sizes in a raw OCI image manifest.
fn compressed_layer_total(raw_manifest: &[u8]) -> Option<u64> {
    let json: serde_json::Value = serde_json::from_slice(raw_manifest).ok()?;
    let layers = json.get("layers")?.as_array()?;
    let total: u64 = layers
        .iter()
        .filter_map(|layer| layer.get("size").and_then(serde_json::Value::as_u64))
        .sum();
    (total > 0).then_some(total)
}

/// Read a single named entry from a tar archive, if present.
fn read_archive_entry(src: &Path, name: &Path) -> Result<Option<Vec<u8>>> {
    let file = std::fs::File::open(src)?;
    let mut archive = tar::Archive::new(file);
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()?.as_ref() == name {
            let mut data = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut data)?;
            return Ok(Some(data));
        }
    }
    Ok(None)
}

/// Digest of the image manifest referenced by an OCI `index.json`.
fn oci_index_manifest_digest(index: &[u8]) -> Result<Digest> {
    let json: serde_json::Value = serde_json::from_slice(index)?;
    json.get("manifests")
        .and_then(|m| m.as_array())
        .and_then(|m| m.first())
        .and_then(|m| m.get("digest"))
        .and_then(|d| d.as_str())
        .ok_or_else(|| anyhow::anyhow!("OCI index.json has no manifest digest"))?
        .parse()
}

/// Raw image manifest bytes for digest verification: the `manifest.json` from a
/// `dir:` layout or tar archive, else the manifest blob referenced by the
/// archive's `index.json`.
pub fn raw_manifest(source_image: &Path) -> Result<Vec<u8>> {
    if source_image.is_dir() {
        // A skopeo `dir:` layout stores the raw manifest as `manifest.json`;
        // `skopeo inspect --raw` just emits that file, so hash it directly.
        let manifest_path = source_image.join("manifest.json");
        return std::fs::read(&manifest_path).map_err(|e| {
            anyhow::anyhow!(
                "failed to read image manifest {}: {e}",
                manifest_path.display()
            )
        });
    }
    if let Some(data) = read_archive_entry(source_image, Path::new("manifest.json"))? {
        return Ok(data);
    }
    let index = read_archive_entry(source_image, Path::new("index.json"))?.ok_or_else(|| {
        anyhow::anyhow!(
            "image archive {} has neither manifest.json nor index.json",
            source_image.display()
        )
    })?;
    let digest = oci_index_manifest_digest(&index)?;
    let blob = digest.oci_blob_path();
    read_archive_entry(source_image, &blob)?.ok_or_else(|| {
        anyhow::anyhow!(
            "OCI archive {} is missing manifest blob {}",
            source_image.display(),
            blob.display()
        )
    })
}

pub fn image_source_ref(source_image: &Path) -> ImageRef {
    if source_image.is_dir() {
        return ImageRef::dir(source_image);
    }
    let ref_name = manifest_field(Some(source_image), "imgref").unwrap_or_default();
    if ref_name.is_empty() {
        return ImageRef::oci_archive(source_image, None);
    }
    let name = ref_name.strip_prefix("oci:").unwrap_or(&ref_name);
    ImageRef::oci_archive(source_image, Some(name))
}

pub fn default_source_image(config: &Config) -> Option<PathBuf> {
    let mount = &config.image.source_mount;
    let default_img = mount.join("image.tar");
    if default_img.is_file() {
        return Some(default_img);
    }
    if path_is_mount(mount) {
        let _ = unmount(mount);
    }
    if running_in_container() {
        return None;
    }

    // Resolve the payload label to a device node via UDisks2, falling back to
    // /dev/disk/by-label (replaces `blkid -L`).
    let device = crate::engine::block::find_device_by_label(config.image.source_label.as_str())?;

    // mount(2) needs an explicit filesystem type; probe it (replaces `mount -o ro`).
    let fstype = probe_value(&device, "TYPE");
    if fstype.is_empty() {
        return None;
    }

    std::fs::create_dir_all(mount).ok()?;
    nix::mount::mount(
        Some(device.as_path()),
        mount.as_path(),
        Some(fstype.as_str()),
        MsFlags::MS_RDONLY,
        None::<&str>,
    )
    .ok()?;

    if default_img.is_file() {
        Some(default_img)
    } else {
        let _ = unmount(mount);
        None
    }
}
