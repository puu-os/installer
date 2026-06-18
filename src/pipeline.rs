// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use nix::mount::MsFlags;

use crate::subproc::{self, LogFn};

pub use crate::steps::STEPS;

pub const DEPENDENCIES: &[&str] = &["bootc", "mke2fs", "mkfs.fat", "mkfs.xfs"];

pub const TARGET_MOUNT: &str = "/run/puu-installer/target";
pub const TARGET_EFI_MOUNT: &str = "/run/puu-installer/target/efi";
pub const TARGET_HOME_MOUNT: &str = "/run/puu-installer/target/var/home";

#[allow(clippy::doc_markdown)]
/// Progress callback: (step_index, status_text, fraction)
pub type ProgressCb = Box<dyn FnMut(usize, &str, f64) + Send>;

const MANIFEST_FILENAME: &str = "image.manifest";

pub fn default_source_image_path(config: &crate::config::Config) -> PathBuf {
    PathBuf::from(&config.image.source_mount).join("image.tar")
}

pub fn default_source_mount(config: &crate::config::Config) -> PathBuf {
    PathBuf::from(&config.image.source_mount)
}

pub const BOOTC_STORAGE_MOUNT: &str = "/run/puu-installer/target/var/tmp/puu-bootc-storage";
pub const BOOTC_IMAGE_STORAGE: &str =
    "/run/puu-installer/target/var/tmp/puu-bootc-storage/containers/storage";
pub const BOOTC_VAR_TMP: &str = "/var/tmp";
pub const CONTAINERS_RUNROOT: &str = "/run/containers/storage";
pub const BOOTC_RUNROOT: &str = "/run/puu-installer/bootc-runroot";

const PART_SIZE_UNITS: &[(&str, u64)] = &[
    ("K", 1024),
    ("M", 1024 * 1024),
    ("G", 1024 * 1024 * 1024),
    ("T", 1024 * 1024 * 1024 * 1024),
    ("P", 1024 * 1024 * 1024 * 1024 * 1024),
];

fn part_size_bytes(size: &str) -> Result<u64> {
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
    Ok(num * multiplier)
}

// GPT partition type GUIDs, in on-disk (mixed-endian) byte order.
// EFI System Partition: C12A7328-F81F-11D2-BA4B-00A0C93EC93B
const PART_TYPE_ESP: [u8; 16] = [
    0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
];
// Linux filesystem: 0FC63DAF-8483-4772-8E79-3D69D8477DE4
const PART_TYPE_LINUX: [u8; 16] = [
    0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
];

fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    use std::io::Read;
    let mut buf = [0u8; N];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf)
}

fn random_guid() -> Result<[u8; 16]> {
    random_bytes::<16>()
}

/// Mount `source` at `target` via `mount(2)`, creating the mountpoint and
/// skipping if it is already mounted. Replaces shelling out to `mount(8)`.
pub fn mount_target(
    runner: &mut Runner,
    source: &str,
    target: &str,
    fstype: &str,
    data: Option<&str>,
) -> Result<()> {
    std::fs::create_dir_all(target)?;
    if Path::new(target).is_mount() {
        runner.log(&format!("{target} is already mounted; reusing it"));
        return Ok(());
    }
    nix::mount::mount(Some(source), target, Some(fstype), MsFlags::empty(), data)
        .map_err(|e| anyhow::anyhow!("failed to mount {source} at {target}: {e}"))?;
    Ok(())
}

pub fn path_is_mount(path: impl AsRef<Path>) -> bool {
    path.as_ref().is_mount()
}

pub fn bind_mount(source: &str, target: &str) -> Result<()> {
    nix::mount::mount(
        Some(source),
        target,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .map_err(|e| anyhow::anyhow!("failed to bind-mount {source} at {target}: {e}"))
}

pub fn unmount(target: &str) -> Result<()> {
    nix::mount::umount(target).map_err(|e| anyhow::anyhow!("failed to unmount {target}: {e}"))
}

/// Produce a `$6$` SHA-512 crypt hash, replacing `openssl passwd -6 -stdin`.
pub fn hash_password_sha512(password: &str) -> Result<String> {
    use sha_crypt::{PasswordHasher, ShaCrypt};
    // 12 raw bytes encode to a 16-character crypt salt, matching `openssl passwd`.
    let salt = random_bytes::<12>()?;
    let hash = ShaCrypt::SHA512
        .hash_password_with_salt(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("failed to hash password: {e}"))?;
    Ok(hash.to_string())
}

/// Create a fresh GPT on `drive` with EFI, root and home partitions, using the
/// `gptman` crate instead of shelling out to `sgdisk`/`partprobe`.
///
/// Layout mirrors the previous `sgdisk` calls: EFI (`efi_size`) at the front,
/// home (`home_size`) pinned to the end of the disk, and root filling the gap.
pub fn partition_drive(
    runner: &mut Runner,
    drive: &str,
    efi_size: &str,
    home_size: &str,
) -> Result<PartitionLayout> {
    let efi_bytes = part_size_bytes(efi_size)?;
    let home_bytes = part_size_bytes(home_size)?;

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(drive)?;
    let sector_size = gptman::linux::get_sector_size(&mut file).unwrap_or(512);

    // A fresh GPT overwrites any existing partition table (equivalent to `sgdisk -Z`).
    let mut gpt = gptman::GPT::new_from(&mut file, sector_size, random_guid()?)?;

    let efi_sectors = efi_bytes.div_ceil(sector_size);
    let home_sectors = home_bytes.div_ceil(sector_size);

    let efi_start = gpt
        .find_first_place(efi_sectors)
        .ok_or_else(|| anyhow::anyhow!("no room for EFI partition on {drive}"))?;
    gpt[1] = gptman::GPTPartitionEntry {
        partition_type_guid: PART_TYPE_ESP,
        unique_partition_guid: random_guid()?,
        starting_lba: efi_start,
        ending_lba: efi_start + efi_sectors - 1,
        attribute_bits: 0,
        partition_name: "EFI".into(),
    };

    // Pin home to the tail of the disk; it consumes everything up to the last usable sector.
    let home_start = gpt
        .find_last_place(home_sectors)
        .ok_or_else(|| anyhow::anyhow!("no room for home partition on {drive}"))?;
    let last_usable = gpt.header.last_usable_lba;
    gpt[3] = gptman::GPTPartitionEntry {
        partition_type_guid: PART_TYPE_LINUX,
        unique_partition_guid: random_guid()?,
        starting_lba: home_start,
        ending_lba: last_usable,
        attribute_bits: 0,
        partition_name: "home".into(),
    };

    // Root fills the remaining free region between EFI and home.
    let root_size = gpt.get_maximum_partition_size()?;
    let root_start = gpt
        .find_optimal_place(root_size)
        .ok_or_else(|| anyhow::anyhow!("no room for root partition on {drive}"))?;
    gpt[2] = gptman::GPTPartitionEntry {
        partition_type_guid: PART_TYPE_LINUX,
        unique_partition_guid: random_guid()?,
        starting_lba: root_start,
        ending_lba: root_start + root_size - 1,
        attribute_bits: 0,
        partition_name: "root".into(),
    };

    let part_bytes = |n: u32| (gpt[n].ending_lba - gpt[n].starting_lba + 1) * sector_size;
    runner.log(&format!(
        "wrote GPT to {drive}: EFI {}, root {}, home {}",
        crate::util::human_size(part_bytes(1)),
        crate::util::human_size(part_bytes(2)),
        crate::util::human_size(part_bytes(3)),
    ));

    gptman::GPT::write_protective_mbr_into(&mut file, sector_size)?;
    gpt.write_into(&mut file)?;
    file.sync_all()?;

    // Make the kernel pick up the new table (equivalent to `partprobe`).
    gptman::linux::reread_partition_table(&mut file)
        .map_err(|e| anyhow::anyhow!("failed to reread partition table on {drive}: {e}"))?;

    Ok(PartitionLayout {
        efi: PartitionInfo {
            number: 1,
            unique_guid: gpt[1].unique_partition_guid,
            starting_lba: gpt[1].starting_lba,
            ending_lba: gpt[1].ending_lba,
        },
    })
}

pub fn min_target_size_bytes(config: &crate::config::Config) -> u64 {
    let efi = format!("+{}", config.partitions.efi);
    let root = format!("+{}", config.partitions.root_min);
    let home = format!("+{}", config.partitions.home);
    let sizes = [efi.as_str(), root.as_str(), home.as_str(), "+1G"];
    sizes.iter().map(|s| part_size_bytes(s).unwrap_or(0)).sum()
}

pub fn running_in_container() -> bool {
    std::env::var("container").as_deref() == Ok("podman")
}

pub fn derive_partitions(drive: &str) -> DerivedPartitions {
    if drive.ends_with(|c: char| c.is_ascii_digit()) {
        DerivedPartitions {
            efi_part: format!("{drive}p1"),
            root_part: format!("{drive}p2"),
            home_part: format!("{drive}p3"),
            ..Default::default()
        }
    } else {
        DerivedPartitions {
            efi_part: format!("{drive}1"),
            root_part: format!("{drive}2"),
            home_part: format!("{drive}3"),
            ..Default::default()
        }
    }
}

fn read_manifest(source_image: &Path) -> Option<serde_json::Value> {
    let manifest = source_image.parent()?.join(MANIFEST_FILENAME);
    let data = std::fs::read_to_string(&manifest).ok()?;
    serde_json::from_str(&data).ok()
}
pub fn load_target_imgref(source_image: Option<&Path>, config: &crate::config::Config) -> String {
    let Some(img) = source_image else {
        return config.image.target_img_ref.clone();
    };
    let Some(json) = read_manifest(img) else {
        return config.image.target_img_ref.clone();
    };
    json.get("target_imgref")
        .and_then(|v| v.as_str())
        .map_or_else(
            || config.image.target_img_ref.clone(),
            std::string::ToString::to_string,
        )
}

pub fn load_image_version(source_image: Option<&Path>) -> String {
    let Some(img) = source_image else {
        return String::new();
    };
    let Some(json) = read_manifest(img) else {
        return String::new();
    };
    json.get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

pub fn image_manifest_metadata(source_image: &Path) -> serde_json::Value {
    read_manifest(source_image).unwrap_or(serde_json::Value::Null)
}

fn source_image_ref_name(metadata: &serde_json::Value) -> String {
    metadata
        .get("imgref")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

pub fn image_source_ref(source_image: &Path) -> String {
    if source_image.is_dir() {
        return format!("dir:{}", source_image.display());
    }
    let metadata = image_manifest_metadata(source_image);
    let ref_name = source_image_ref_name(&metadata);
    if ref_name.is_empty() {
        return format!("oci-archive:{}", source_image.display());
    }
    let name = ref_name.strip_prefix("oci:").unwrap_or(&ref_name);
    format!("oci-archive:{}:{}", source_image.display(), name)
}

pub fn default_source_image(config: &crate::config::Config) -> Option<PathBuf> {
    let default_img = default_source_image_path(config);
    if default_img.is_file() {
        return Some(default_img);
    }
    let mount = default_source_mount(config);
    if mount.is_mount() {
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

fn unmount_default_source(config: &crate::config::Config) -> Result<()> {
    unmount(&default_source_mount(config).to_string_lossy())
}

trait PathMountExt {
    fn is_mount(&self) -> bool;
}

impl<P: AsRef<Path>> PathMountExt for P {
    fn is_mount(&self) -> bool {
        let path = self.as_ref();
        if let (Ok(self_meta), Ok(parent_meta)) = (
            std::fs::metadata(path),
            std::fs::metadata(path.parent().unwrap_or(path)),
        ) {
            use std::os::unix::fs::MetadataExt;
            self_meta.dev() != parent_meta.dev()
        } else {
            false
        }
    }
}

#[derive(Debug, Clone)]
pub struct PartitionInfo {
    pub number: u32,
    pub unique_guid: [u8; 16],
    pub starting_lba: u64,
    pub ending_lba: u64,
}

#[derive(Debug, Clone)]
pub struct PartitionLayout {
    pub efi: PartitionInfo,
}

#[derive(Debug, Clone, Default)]
pub struct DerivedPartitions {
    pub efi_part: String,
    pub efi_uuid: String,
    pub root_part: String,
    pub root_uuid: String,
    pub home_part: String,
    pub home_uuid: String,
    pub layout: Option<PartitionLayout>,
}

#[derive(Clone)]
pub struct Context {
    pub config: crate::config::Config,
    pub source_image: Option<PathBuf>,
    pub target_imgref: String,
    pub image_version: String,
    pub drive: String,
    pub drive_label: String,
    pub username: String,
    pub user_password: String,
    pub user_shell: String,
    pub keymap: String,
    pub locale: String,
    pub secure_boot: bool,
    pub k3s_token: String,
    pub last_error: String,
    pub start_from: usize,
    pub skip_steps: Vec<usize>,
    pub parts: DerivedPartitions,
}

pub type StepFn = fn(&mut Runner) -> Result<()>;

pub struct Step {
    pub title: &'static str,
    pub fn_: StepFn,
    pub point_of_no_return: bool,
    pub requires_secure_boot: bool,
}

pub struct StepSelection {
    pub start_index: usize,
    pub skip_indices: Vec<usize>,
}

impl StepSelection {
    fn includes(&self, index: usize, step: &Step, secure_boot: bool) -> bool {
        index >= self.start_index
            && !self.skip_indices.contains(&index)
            && (!step.requires_secure_boot || secure_boot)
    }
}

pub struct Runner {
    pub ctx: Context,
    pub log: LogFn,
    pub progress: ProgressCb,
    pub step_index: usize,
}

impl Runner {
    pub fn log(&mut self, msg: &str) {
        (self.log)(msg.to_string());
    }

    pub fn progress(&mut self, fraction: f64, status: &str) {
        (self.progress)(self.step_index, status, fraction);
    }

    pub fn run_cmd(
        &mut self,
        argv: &[&str],
        env: Option<&[(String, String)]>,
        heartbeat_interval: Option<Duration>,
        heartbeat_msg: Option<&str>,
    ) -> Result<()> {
        let cmd_args: Vec<String> = argv.iter().map(std::string::ToString::to_string).collect();
        subproc::check(
            &cmd_args,
            &mut self.log,
            env,
            None,
            heartbeat_interval,
            heartbeat_msg,
        )
    }

    /// Like [`Runner::run_cmd`], but the command runs chrooted into `chroot_dir`,
    /// replacing the external `chroot` helper.
    pub fn run_cmd_chroot(
        &mut self,
        argv: &[&str],
        chroot_dir: &str,
        heartbeat_interval: Option<Duration>,
        heartbeat_msg: Option<&str>,
    ) -> Result<()> {
        let cmd_args: Vec<String> = argv.iter().map(std::string::ToString::to_string).collect();
        subproc::check_chroot(
            &cmd_args,
            &mut self.log,
            None,
            None,
            heartbeat_interval,
            heartbeat_msg,
            Some(chroot_dir),
        )
    }

    pub fn execute(
        &mut self,
        steps: &[Step],
        selection: &StepSelection,
        mut is_cancelled: impl FnMut() -> bool,
    ) -> Result<bool> {
        for (i, step) in steps.iter().enumerate() {
            if is_cancelled() {
                return Ok(false);
            }
            if !selection.includes(i, step, self.ctx.secure_boot) {
                continue;
            }
            self.step_index = i;
            self.progress(0.0, step.title);
            if step.point_of_no_return {
                self.log("entering point of no return");
            }
            (step.fn_)(self)?;
            self.progress(1.0, step.title);
        }
        Ok(true)
    }
}

const CHROOT_BIND_MOUNTS: &[(&str, &str)] = &[
    ("/proc", "proc"),
    ("/sys", "sys"),
    ("/dev", "dev"),
    ("/run", "run"),
];

const TARGET_MOUNTS: &[&str] = &[TARGET_HOME_MOUNT, TARGET_EFI_MOUNT, TARGET_MOUNT];

/// Mount points to tear down, in unmount order: chroot bind mounts (reversed)
/// followed by the target filesystems from deepest to root.
fn decode_mountinfo_path(path: &str) -> String {
    let mut out = String::new();
    let mut chars = path.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            let octal: String = chars.by_ref().take(3).collect();
            if octal.len() == 3 {
                if let Ok(value) = u8::from_str_radix(&octal, 8) {
                    out.push(value as char);
                    continue;
                }
            }
            out.push('\\');
            out.push_str(&octal);
        } else {
            out.push(c);
        }
    }

    out
}

fn mounted_target_points() -> Vec<String> {
    let mut points = Vec::new();
    let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return points;
    };

    for line in mountinfo.lines() {
        let Some(mount_point) = line.split_whitespace().nth(4) else {
            continue;
        };
        let mount_point = decode_mountinfo_path(mount_point);
        if mount_point == TARGET_MOUNT
            || mount_point.starts_with(&format!("{TARGET_MOUNT}/"))
            || mount_point == BOOTC_VAR_TMP
        {
            points.push(mount_point);
        }
    }

    points
}

fn teardown_mount_points() -> Vec<String> {
    let mut points = mounted_target_points();
    if Path::new(BOOTC_VAR_TMP).is_mount() {
        points.push(BOOTC_VAR_TMP.to_string());
    }
    for (_, rel) in CHROOT_BIND_MOUNTS.iter().rev() {
        points.push(format!("{TARGET_MOUNT}/{rel}"));
    }
    for mp in TARGET_MOUNTS {
        points.push((*mp).to_string());
    }

    points.sort_by_key(|point| std::cmp::Reverse(point.len()));
    points.dedup();
    points
}

pub fn cleanup_mounts(runner: &mut Runner) -> Result<()> {
    for mp in teardown_mount_points() {
        if Path::new(&mp).is_mount() {
            runner.log(&format!("unmounting {mp}"));
            unmount(&mp)?;
        }
    }
    Ok(())
}

pub fn mount_chroot_binds(_runner: &mut Runner) -> Result<Vec<String>> {
    let mut mounted = Vec::new();
    for (src, rel) in CHROOT_BIND_MOUNTS {
        let dst = format!("{TARGET_MOUNT}/{rel}");
        std::fs::create_dir_all(&dst)?;
        if Path::new(&dst).is_mount() {
            continue;
        }
        bind_mount(src, &dst)?;
        mounted.push(dst);
    }
    Ok(mounted)
}

pub fn unmount_chroot_binds(_runner: &mut Runner, mounted: &[String]) -> Result<()> {
    for dst in mounted.iter().rev() {
        unmount(dst)?;
    }
    Ok(())
}

fn probe_value(device: &str, partitions: bool, key: &str) -> Result<String> {
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

const SBCTL_KEY_PATHS: &[&str] = &[
    "var/lib/sbctl/keys/PK/PK.key",
    "var/lib/sbctl/keys/KEK/KEK.key",
    "var/lib/sbctl/keys/db/db.key",
];

fn target_sbctl_keys_exist() -> bool {
    let target = Path::new(TARGET_MOUNT);
    SBCTL_KEY_PATHS.iter().all(|p| target.join(p).is_file())
}

fn target_sbctl_dir() -> PathBuf {
    let path = Path::new(TARGET_MOUNT).join("var/lib/sbctl");
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Directory holding the generated PK/KEK/db key pairs.
pub fn sbctl_keydir() -> PathBuf {
    target_sbctl_dir().join("keys")
}

/// Signature owner GUID (from the generated GUID file) in EFI byte order.
pub fn sbctl_owner_guid() -> [u8; 16] {
    let contents = std::fs::read_to_string(target_sbctl_dir().join("GUID")).unwrap_or_default();
    uuid::Uuid::parse_str(contents.trim())
        .unwrap_or(uuid::Uuid::nil())
        .to_bytes_le()
}

pub fn target_efi_binaries() -> Vec<PathBuf> {
    let esp = Path::new(TARGET_EFI_MOUNT);
    let mut paths: Vec<PathBuf> = Vec::new();
    for pattern in &["*.efi", "*.EFI"] {
        for entry in walkdir_like(esp, pattern) {
            if entry.is_file() {
                paths.push(entry);
            }
        }
    }
    paths.sort();
    paths
}

fn walkdir_like(root: &Path, glob: &str) -> Vec<PathBuf> {
    let mut results = Vec::new();
    let ext_lower = glob.trim_start_matches('*').to_lowercase();
    #[allow(clippy::items_after_statements)]
    fn recurse(dir: &Path, ext: &str, results: &mut Vec<PathBuf>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    recurse(&path, ext, results);
                } else if path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.to_lowercase() == ext)
                {
                    results.push(path);
                }
            }
        }
    }
    recurse(root, &ext_lower, &mut results);
    results
}

fn random_uuid() -> Result<String> {
    let b = random_bytes::<16>()?;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    ))
}

/// Generate an RSA-2048 key and a self-signed X.509 certificate for a Secure
/// Boot key (PK/KEK/db), returning their PEM encodings. Replaces the keygen
/// side of `sbctl create-keys`.
pub(crate) fn generate_sb_key(common_name: &str) -> Result<(String, String)> {
    use openssl::asn1::Asn1Time;
    use openssl::bn::{BigNum, MsbOption};
    use openssl::hash::MessageDigest;
    use openssl::pkey::PKey;
    use openssl::rsa::Rsa;
    use openssl::x509::{X509, X509NameBuilder};

    let rsa = Rsa::generate(2048)?;
    let pkey = PKey::from_rsa(rsa)?;

    let mut name = X509NameBuilder::new()?;
    name.append_entry_by_text("CN", common_name)?;
    let name = name.build();

    let mut builder = X509::builder()?;
    builder.set_version(2)?; // X.509 v3
    let mut serial = BigNum::new()?;
    serial.rand(159, MsbOption::MAYBE_ZERO, false)?;
    let serial = serial.to_asn1_integer()?;
    builder.set_serial_number(&serial)?;
    builder.set_subject_name(&name)?;
    builder.set_issuer_name(&name)?;
    builder.set_pubkey(&pkey)?;
    let not_before = Asn1Time::days_from_now(0)?;
    let not_after = Asn1Time::days_from_now(3650)?;
    builder.set_not_before(&not_before)?;
    builder.set_not_after(&not_after)?;
    builder.sign(&pkey, MessageDigest::sha256())?;
    let cert = builder.build();

    let key_pem = String::from_utf8(pkey.private_key_to_pem_pkcs8()?)?;
    let cert_pem = String::from_utf8(cert.to_pem()?)?;
    Ok((key_pem, cert_pem))
}

pub fn ensure_sbctl_keys(runner: &mut Runner) -> Result<()> {
    if target_sbctl_keys_exist() {
        runner.log("Secure Boot signing keys already exist; reusing them");
        return Ok(());
    }
    let keydir = target_sbctl_dir().join("keys");
    for kind in ["PK", "KEK", "db"] {
        let dir = keydir.join(kind);
        std::fs::create_dir_all(&dir)?;
        let (key_pem, cert_pem) = generate_sb_key(kind)?;
        let key_path = dir.join(format!("{kind}.key"));
        std::fs::write(&key_path, key_pem)?;
        std::fs::set_permissions(&key_path, PermissionsExt::from_mode(0o600))?;
        std::fs::write(dir.join(format!("{kind}.pem")), cert_pem)?;
    }
    let guid_path = target_sbctl_dir().join("GUID");
    if !guid_path.exists() {
        std::fs::write(&guid_path, format!("{}\n", random_uuid()?))?;
    }
    runner.log("generated Secure Boot PK/KEK/db keys");
    Ok(())
}

pub fn merge_fstab(existing: &str, efi_uuid: &str, home_uuid: &str) -> String {
    let managed = ["/efi", "/var/home"];
    let mut lines: Vec<String> = Vec::new();
    for line in existing.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 2 && managed.contains(&fields[1]) {
            continue;
        }
        lines.push(line.to_string());
    }
    lines.push(format!(
        "PARTUUID={efi_uuid} /efi vfat fmask=0077,dmask=0077,nofail 0 2"
    ));
    lines.push(format!("UUID={home_uuid} /var/home ext4 defaults 0 2"));
    let mut result = lines.join("\n");
    result.push('\n');
    result
}

pub fn install_kernel_args(root_uuid: &str, config: &crate::config::Config) -> Vec<String> {
    let machine = std::env::consts::ARCH;
    let mut args = vec![
        format!("root=UUID={root_uuid}"),
        "rw".into(),
        "vt.global_cursor_default=0".into(),
        "console=tty0".into(),
        "threadirqs".into(),
        "preempt=full".into(),
    ];
    let serial_console = config
        .serial_console
        .get(machine)
        .cloned()
        .or_else(|| match machine {
            "x86_64" => Some("console=ttyS0,115200".into()),
            "aarch64" | "arm64" => Some("console=ttyAMA0,115200".into()),
            _ => None,
        });
    if let Some(console) = serial_console {
        args.push(console);
    }
    if machine == "aarch64" || machine == "arm64" {
        for a in &config.kernel.extra_arm64_args {
            args.push(a.clone());
        }
    }
    args
}

const PART_WAIT_TIMEOUT_S: u64 = 30;
const PART_WAIT_INTERVAL_MS: u64 = 500;

pub fn wait_for_block_device(runner: &mut Runner, part: &str) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(PART_WAIT_TIMEOUT_S);
    loop {
        if Path::new(part).exists() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "partition {part} did not appear within {PART_WAIT_TIMEOUT_S}s \
                 after writing the partition table"
            );
        }
        runner.log(&format!("waiting for {part} to appear..."));
        std::thread::sleep(Duration::from_millis(PART_WAIT_INTERVAL_MS));
    }
}

pub fn mount_bootc_vartmp(_runner: &mut Runner) -> Result<bool> {
    let bootc_tmp = PathBuf::from(BOOTC_STORAGE_MOUNT).join("tmp");
    std::fs::create_dir_all(&bootc_tmp)?;
    std::fs::set_permissions(&bootc_tmp, PermissionsExt::from_mode(0o1777))?;

    std::fs::create_dir_all(BOOTC_VAR_TMP)?;
    std::fs::set_permissions(BOOTC_VAR_TMP, PermissionsExt::from_mode(0o1777))?;

    if Path::new(BOOTC_VAR_TMP).is_mount() {
        return Ok(false);
    }
    bind_mount(&bootc_tmp.to_string_lossy(), BOOTC_VAR_TMP)?;
    Ok(true)
}

pub fn deployment_root() -> Result<PathBuf> {
    use ostree::gio;
    use ostree::prelude::*;

    let sysroot = ostree::Sysroot::new(Some(&gio::File::for_path(TARGET_MOUNT)));
    sysroot
        .load(gio::Cancellable::NONE)
        .map_err(|e| anyhow::anyhow!("failed to load ostree sysroot at {TARGET_MOUNT}: {e}"))?;
    // The first deployment in boot order is the current one (as printed by
    // `ostree admin --print-current-dir`); a fresh install has exactly one.
    let deployment =
        sysroot.deployments().into_iter().next().ok_or_else(|| {
            anyhow::anyhow!("ostree did not report the installed deployment root")
        })?;
    sysroot
        .deployment_directory(&deployment)
        .path()
        .ok_or_else(|| anyhow::anyhow!("ostree deployment directory has no path"))
}

pub fn flatpak_manifest_refs(path: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

pub fn copy_tree_files(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        std::fs::remove_dir_all(dst)?;
    }
    #[allow(clippy::items_after_statements)]
    fn recurse(src: &Path, dst: &Path) -> Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());
            if src_path.is_dir() {
                recurse(&src_path, &dst_path)?;
            } else {
                std::fs::copy(&src_path, &dst_path)?;
            }
        }
        Ok(())
    }
    recurse(src, dst)
}

pub fn rollback(log: &mut LogFn) {
    for mp in teardown_mount_points() {
        if Path::new(&mp).is_mount() {
            log(format!("rollback: unmounting {mp}"));
            let _ = unmount(&mp);
        }
    }
}
