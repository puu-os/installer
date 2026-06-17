// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};

use crate::subproc::{self, LogFn};

pub use crate::steps::STEPS;

pub const DEPENDENCIES: &[&str] = &[
    "awk",
    "blkid",
    "bootc",
    "bootctl",
    "chroot",
    "jq",
    "lsblk",
    "mke2fs",
    "mkfs.fat",
    "mkfs.xfs",
    "mount",
    "openssl",
    "ostree",
    "partprobe",
    "podman",
    "sbctl",
    "sgdisk",
    "skopeo",
    "systemd-firstboot",
    "udevadm",
    "umount",
];

pub const TARGET_MOUNT: &str = "/run/puu-installer/target";
pub const TARGET_EFI_MOUNT: &str = "/run/puu-installer/target/efi";
pub const TARGET_HOME_MOUNT: &str = "/run/puu-installer/target/var/home";

#[allow(clippy::doc_markdown)]
/// Progress callback: (step_index, status_text, fraction)
type ProgressCb = Box<dyn FnMut(usize, &str, f64) + Send>;

const _MANIFEST_FILENAME: &str = "image.manifest";

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
    let manifest = source_image.parent()?.join(_MANIFEST_FILENAME);
    let data = std::fs::read_to_string(&manifest).ok()?;
    serde_json::from_str(&data).ok()
}
#[allow(clippy::manual_strip)]
pub fn load_target_imgref(source_image: Option<&Path>, config: &crate::config::Config) -> String {
    let Some(img) = source_image else {
        return config.image.target_img_ref.clone();
    };
    let Some(json) = read_manifest(img) else {
        return config.image.target_img_ref.clone();
    };
    json.get("target_imgref")
        .or_else(|| json.get("imgref"))
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
    if !ref_name.is_empty() {
        let oci_prefix = "oci:";
        if ref_name.starts_with(oci_prefix) {
            return format!(
                "oci-archive:{}:{}",
                source_image.display(),
                ref_name
                    .strip_prefix(oci_prefix)
                    .unwrap_or(ref_name.as_str())
            );
        }
        return format!("oci-archive:{}:{}", source_image.display(), ref_name);
    }
    format!("oci-archive:{}", source_image.display())
}

pub fn default_source_image(config: &crate::config::Config) -> Option<PathBuf> {
    let default_img = default_source_image_path(config);
    if default_img.is_file() {
        return Some(default_img);
    }
    let mount = default_source_mount(config);
    if mount.is_mount() {
        let _ = unmount_default_source(config, "stale");
    }
    if running_in_container() {
        return None;
    }

    let output = std::process::Command::new("blkid")
        .args(["-L", &config.image.source_label])
        .output()
        .ok()?;
    let device = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if device.is_empty() {
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if !stderr.is_empty() {
                eprintln!(
                    "unable to locate live payload label {}: {stderr}",
                    config.image.source_label
                );
            }
        }
        return None;
    }

    std::fs::create_dir_all(&mount).ok()?;
    let mount_output = std::process::Command::new("mount")
        .args(["-o", "ro", &device, &mount.to_string_lossy()])
        .output()
        .ok()?;
    if !mount_output.status.success() {
        return None;
    }

    if default_img.is_file() {
        Some(default_img)
    } else {
        let _ = unmount_default_source(config, "invalid");
        None
    }
}

fn unmount_default_source(config: &crate::config::Config, _reason: &str) -> Result<()> {
    let output = std::process::Command::new("umount")
        .arg(default_source_mount(config))
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run umount: {e}"))?;
    if !output.status.success() {
        bail!(
            "unable to unmount payload: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
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

#[derive(Debug, Clone, Default)]
pub struct DerivedPartitions {
    pub efi_part: String,
    pub efi_uuid: String,
    pub root_part: String,
    pub root_uuid: String,
    pub home_part: String,
    pub home_uuid: String,
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
    pub fn capture_cmd(argv: &[&str], stdin_data: Option<&[u8]>) -> Result<String> {
        let cmd_args: Vec<String> = argv.iter().map(std::string::ToString::to_string).collect();
        subproc::capture(&cmd_args, stdin_data, None)
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

pub fn cleanup_mounts(runner: &mut Runner) -> Result<()> {
    for (_, rel) in CHROOT_BIND_MOUNTS.iter().rev() {
        let dst = format!("{TARGET_MOUNT}/{rel}");
        if Path::new(&dst).is_mount() {
            runner.run_cmd(&["umount", &dst], None, None, None)?;
        }
    }
    for mp in TARGET_MOUNTS.iter().rev() {
        if Path::new(mp).is_mount() {
            runner.run_cmd(&["umount", mp], None, None, None)?;
        }
    }
    Ok(())
}

pub fn mount_chroot_binds(runner: &mut Runner) -> Result<Vec<String>> {
    let mut mounted = Vec::new();
    for (src, rel) in CHROOT_BIND_MOUNTS {
        let dst = format!("{TARGET_MOUNT}/{rel}");
        std::fs::create_dir_all(&dst)?;
        if Path::new(&dst).is_mount() {
            continue;
        }
        runner.run_cmd(&["mount", "--bind", src, &dst], None, None, None)?;
        mounted.push(dst);
    }
    Ok(mounted)
}

pub fn unmount_chroot_binds(runner: &mut Runner, mounted: &[String]) -> Result<()> {
    for dst in mounted.iter().rev() {
        runner.run_cmd(&["umount", dst], None, None, None)?;
    }
    Ok(())
}

pub fn capture_uuid(device: &str) -> Result<String> {
    let uuid = Runner::capture_cmd(&["blkid", "-s", "UUID", "-o", "value", device], None)?
        .trim()
        .to_string();
    if uuid.is_empty() {
        bail!("unable to determine UUID for {device}");
    }
    Ok(uuid)
}

pub fn capture_partuuid(device: &str) -> Result<String> {
    let partuuid = Runner::capture_cmd(&["blkid", "-s", "PARTUUID", "-o", "value", device], None)?
        .trim()
        .to_string();
    if partuuid.is_empty() {
        bail!("unable to determine PARTUUID for {device}");
    }
    Ok(partuuid)
}

pub fn drive_size_bytes(drive: &str) -> Result<u64> {
    let out = Runner::capture_cmd(&["lsblk", "-dnbo", "SIZE", drive], None)?;
    out.trim()
        .parse::<u64>()
        .map_err(|_| anyhow::anyhow!("unable to determine size of target drive '{drive}'"))
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

const SBCTL_CONFIG: &str = "/run/puu-installer/sbctl-target.conf";

fn target_sbctl_dir() -> PathBuf {
    let path = Path::new(TARGET_MOUNT).join("var/lib/sbctl");
    let _ = std::fs::create_dir_all(&path);
    path
}

fn write_target_sbctl_config() -> Result<PathBuf> {
    let sbctl_dir = target_sbctl_dir();
    let config = Path::new(SBCTL_CONFIG);
    let parent = config
        .parent()
        .ok_or_else(|| anyhow::anyhow!("sbctl config path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let content = format!(
        "landlock: false\nkeydir: {}\nguid: {}\nfiles_db: {}\nbundles_db: {}\n",
        sbctl_dir.join("keys").display(),
        sbctl_dir.join("GUID").display(),
        sbctl_dir.join("files.json").display(),
        sbctl_dir.join("bundles.json").display(),
    );
    std::fs::write(config, &content)?;
    std::fs::set_permissions(config, PermissionsExt::from_mode(0o600))?;
    Ok(config.to_path_buf())
}

pub fn target_sbctl_argv(args: &[&str]) -> Result<Vec<String>> {
    let config = write_target_sbctl_config()?;
    let mut cmd = vec![
        "sbctl".to_string(),
        "--disable-landlock".to_string(),
        "--config".to_string(),
        config.to_string_lossy().to_string(),
    ];
    for a in args {
        cmd.push((*a).to_string());
    }
    Ok(cmd)
}

pub fn target_sbctl_env() -> Vec<(String, String)> {
    vec![
        ("SYSTEMD_ESP_PATH".into(), TARGET_EFI_MOUNT.into()),
        ("ESP_PATH".into(), TARGET_EFI_MOUNT.into()),
    ]
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

pub fn ensure_sbctl_keys(runner: &mut Runner) -> Result<()> {
    if target_sbctl_keys_exist() {
        runner.log("Secure Boot signing keys already exist; reusing them");
        return Ok(());
    }
    let sbctl_argv = target_sbctl_argv(&["create-keys", "--keytype", "file"])?;
    let cmd_args: Vec<&str> = sbctl_argv.iter().map(std::string::String::as_str).collect();
    match runner.run_cmd(&cmd_args, None, None, None) {
        Ok(()) => Ok(()),
        Err(e) => {
            if target_sbctl_keys_exist() {
                runner.log("Secure Boot signing keys already exist after create-keys");
                Ok(())
            } else {
                Err(e)
            }
        }
    }
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
    for (mach, console) in &config.serial_console {
        if machine == *mach {
            args.push(console.clone());
            break;
        }
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
                 after partprobe"
            );
        }
        runner.log(&format!("waiting for {part} to appear..."));
        std::thread::sleep(Duration::from_millis(PART_WAIT_INTERVAL_MS));
    }
}

pub fn mount_if_needed(runner: &mut Runner, mountpoint: &str, argv: &[&str]) -> Result<()> {
    std::fs::create_dir_all(mountpoint)?;
    if Path::new(mountpoint).is_mount() {
        runner.log(&format!("{mountpoint} is already mounted; reusing it"));
        return Ok(());
    }
    runner.run_cmd(argv, None, None, None)
}

pub fn mount_bootc_vartmp(runner: &mut Runner) -> Result<bool> {
    let bootc_tmp = PathBuf::from(BOOTC_STORAGE_MOUNT).join("tmp");
    std::fs::create_dir_all(&bootc_tmp)?;
    std::fs::set_permissions(&bootc_tmp, PermissionsExt::from_mode(0o1777))?;

    std::fs::create_dir_all(BOOTC_VAR_TMP)?;
    std::fs::set_permissions(BOOTC_VAR_TMP, PermissionsExt::from_mode(0o1777))?;

    if Path::new(BOOTC_VAR_TMP).is_mount() {
        return Ok(false);
    }
    runner.run_cmd(
        &[
            "mount",
            "--bind",
            &bootc_tmp.to_string_lossy(),
            BOOTC_VAR_TMP,
        ],
        None,
        None,
        None,
    )?;
    Ok(true)
}

pub fn deployment_root() -> Result<PathBuf> {
    let out = Runner::capture_cmd(
        &[
            "ostree",
            "admin",
            &format!("--sysroot={TARGET_MOUNT}"),
            "--print-current-dir",
        ],
        None,
    )?;
    let path = out.trim();
    if path.is_empty() {
        bail!("ostree did not report the installed deployment root");
    }
    Ok(PathBuf::from(path))
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
    fn quiet(argv: &[&str], log: &mut LogFn) {
        let cmd_args: Vec<String> = argv.iter().map(std::string::ToString::to_string).collect();
        let _ = subproc::run(&cmd_args, log, None, None, None, None, None);
    }

    for (_, rel) in CHROOT_BIND_MOUNTS.iter().rev() {
        let dst = format!("{TARGET_MOUNT}/{rel}");
        if Path::new(&dst).is_mount() {
            quiet(&["umount", &dst], log);
        }
    }
    for mp in TARGET_MOUNTS.iter().rev() {
        if Path::new(mp).is_mount() {
            quiet(&["umount", mp], log);
        }
    }
}
