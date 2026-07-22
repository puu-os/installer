// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use nix::mount::MsFlags;
use nix::sched::{CloneFlags, unshare};

use crate::engine::command;
use crate::engine::partition::DerivedPartitions;

pub use crate::engine::steps::STEPS;

pub const DEPENDENCIES: &[&str] = &["bootc", "mke2fs", "mkfs.fat", "mkfs.xfs", "unshare"];

pub const TARGET_MOUNT: &str = "/run/puu-installer/target";
pub const TARGET_EFI_MOUNT: &str = "/run/puu-installer/target/efi";
pub const TARGET_HOME_MOUNT: &str = "/run/puu-installer/target/var/home";

/// Progress callback invoked with the current `step_index` and its completion
/// `fraction` in `[0.0, 1.0]`.
pub type ProgressCb = Box<dyn FnMut(usize, f64) + Send>;

/// Skip callback invoked with the `step_index` of each step the selection
/// excludes, so the UI reflects the engine's decisions instead of re-deriving
/// the selection rules from mutable state.
pub type SkipCb = Box<dyn FnMut(usize) + Send>;

pub const BOOTC_STORAGE_MOUNT: &str = "/run/puu-installer/target/var/tmp/puu-bootc-storage";
pub const BOOTC_IMAGE_STORAGE: &str =
    "/run/puu-installer/target/var/tmp/puu-bootc-storage/containers/storage";
pub const BOOTC_VAR_TMP: &str = "/var/tmp";
pub const CONTAINERS_RUNROOT: &str = "/run/containers/storage";
pub const BOOTC_RUNROOT: &str = "/run/puu-installer/bootc-runroot";

/// Enter a private mount namespace before creating installer mounts.
pub fn enter_private_mount_namespace() -> Result<()> {
    unshare(CloneFlags::CLONE_NEWNS)
        .map_err(|e| anyhow::anyhow!("failed to create private mount namespace: {e}"))?;
    nix::mount::mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_PRIVATE | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(|e| anyhow::anyhow!("failed to make mount namespace private: {e}"))
}

/// Mount `source` at `target` via `mount(2)`, creating the mountpoint and
/// skipping if it is already mounted. Replaces shelling out to `mount(8)`.
pub fn mount_target(source: &str, target: &str, fstype: &str, data: Option<&str>) -> Result<()> {
    std::fs::create_dir_all(target)?;
    if path_is_mount(target) {
        tracing::info!(
            target: crate::engine::INSTALLATION_LOG_TARGET,
            "{target} is already mounted; reusing it"
        );
        return Ok(());
    }
    nix::mount::mount(Some(source), target, Some(fstype), MsFlags::empty(), data)
        .map_err(|e| anyhow::anyhow!("failed to mount {source} at {target}: {e}"))?;
    Ok(())
}

/// Whether `path` is an exact mount point according to `/proc/self/mountinfo`.
pub fn path_is_mount(path: impl AsRef<Path>) -> bool {
    let path = path.as_ref();
    let normalized = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let normalized = normalized.to_string_lossy();
    let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    mountinfo.lines().any(|line| {
        crate::engine::util::parse_mountinfo_line(line)
            .is_some_and(|(mountpoint, _, _)| mountpoint == normalized)
    })
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

#[derive(Clone)]
pub struct Context {
    pub config: crate::engine::config::Config,
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
    pub parts: DerivedPartitions,
}

impl Default for Context {
    fn default() -> Self {
        Self {
            config: crate::engine::config::Config::default(),
            source_image: None,
            target_imgref: String::new(),
            image_version: String::new(),
            drive: String::new(),
            drive_label: String::new(),
            username: String::new(),
            user_password: String::new(),
            user_shell: "/bin/bash".to_string(),
            keymap: String::new(),
            locale: String::new(),
            secure_boot: true,
            k3s_token: String::new(),
            parts: DerivedPartitions::default(),
        }
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        crate::engine::util::clear_secret(&mut self.user_password);
        crate::engine::util::clear_secret(&mut self.k3s_token);
    }
}

pub type StepFn = fn(&mut Runner) -> Result<()>;

pub struct Step {
    pub title: &'static str,
    pub fn_: StepFn,
    pub point_of_no_return: bool,
    pub requires_secure_boot: bool,
    /// Relative expected cost, used to weight the overall progress bar so it
    /// tracks wall-clock rather than step count.
    pub weight: u32,
}

/// Stack of cleanup actions that run in reverse order on drop, even if a
/// step fails partway through.
pub(crate) struct CleanupStack(Vec<Box<dyn FnOnce()>>);

impl CleanupStack {
    pub(crate) fn new() -> Self {
        Self(Vec::new())
    }

    pub(crate) fn push(&mut self, f: impl FnOnce() + 'static) {
        self.0.push(Box::new(f));
    }
}

impl Drop for CleanupStack {
    fn drop(&mut self) {
        for f in self.0.drain(..).rev() {
            f();
        }
    }
}

pub struct Runner {
    pub ctx: Context,
    pub progress: ProgressCb,
    pub skipped: SkipCb,
    pub step_index: usize,
}

impl Runner {
    pub fn progress(&mut self, fraction: f64) {
        (self.progress)(self.step_index, fraction);
    }

    /// Build a command that samples `source` while it runs and
    /// reports it as the current step's progress.
    pub fn cmd_tracked<S: AsRef<str>>(
        &mut self,
        argv: &[S],
        source: Box<dyn crate::engine::progress::ProgressSource>,
    ) -> command::Command<'_> {
        let step_index = self.step_index;
        let progress = &mut self.progress;
        let emit = move |fraction: f64| (progress)(step_index, fraction);
        command::Command::new(argv).track(source, Box::new(emit))
    }

    pub fn execute(
        &mut self,
        steps: &[Step],
        mut is_cancelled: impl FnMut() -> bool,
    ) -> Result<bool> {
        let mut committed = false;
        for (i, step) in steps.iter().enumerate() {
            if !committed && is_cancelled() {
                return Ok(false);
            }
            if step.requires_secure_boot && !self.ctx.secure_boot {
                (self.skipped)(i);
                continue;
            }
            self.step_index = i;
            self.progress(0.0);
            if !committed && is_cancelled() {
                return Ok(false);
            }
            if step.point_of_no_return {
                tracing::info!(
                    target: crate::engine::INSTALLATION_LOG_TARGET,
                    "entering point of no return"
                );
                committed = true;
            }
            (step.fn_)(self)?;
            self.progress(1.0);
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

fn mounted_target_points() -> Vec<String> {
    let mut points = Vec::new();
    let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return points;
    };

    for line in mountinfo.lines() {
        let Some((mount_point, _, _)) = crate::engine::util::parse_mountinfo_line(line) else {
            continue;
        };
        if mount_point == TARGET_MOUNT
            || mount_point.starts_with(&format!("{TARGET_MOUNT}/"))
            || mount_point == BOOTC_VAR_TMP
        {
            points.push(mount_point);
        }
    }

    points
}

/// Mount points to tear down, in unmount order: chroot bind mounts (reversed)
/// followed by the target filesystems from deepest to root.
fn teardown_mount_points() -> Vec<String> {
    let mut points = mounted_target_points();
    if path_is_mount(BOOTC_VAR_TMP) {
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

pub fn cleanup_mounts() -> Result<()> {
    for mp in teardown_mount_points() {
        if path_is_mount(&mp) {
            tracing::info!(target: crate::engine::INSTALLATION_LOG_TARGET, "unmounting {mp}");
            unmount(&mp)?;
        }
    }
    Ok(())
}

pub fn mount_chroot_binds() -> Result<Vec<String>> {
    let mut mounted = Vec::new();
    for (src, rel) in CHROOT_BIND_MOUNTS {
        let dst = format!("{TARGET_MOUNT}/{rel}");
        std::fs::create_dir_all(&dst)?;
        if path_is_mount(&dst) {
            continue;
        }
        bind_mount(src, &dst)?;
        mounted.push(dst);
    }
    Ok(mounted)
}

pub fn unmount_chroot_binds(mounted: &[String]) -> Result<()> {
    for dst in mounted.iter().rev() {
        unmount(dst)?;
    }
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

pub fn install_kernel_args(root_uuid: &str, config: &crate::engine::config::Config) -> Vec<String> {
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
            "aarch64" => Some("console=ttyAMA0,115200".into()),
            _ => None,
        });
    if let Some(console) = serial_console {
        args.push(console);
    }
    if machine == "aarch64" {
        for a in &config.kernel.extra_arm64_args {
            args.push(a.clone());
        }
    }
    args
}

const PART_WAIT_TIMEOUT_S: u64 = 30;
const PART_WAIT_INTERVAL_MS: u64 = 500;

pub fn wait_for_block_device(part: &str) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(PART_WAIT_TIMEOUT_S);
    loop {
        if Path::new(part).exists() && std::fs::OpenOptions::new().read(true).open(part).is_ok() {
            std::thread::sleep(Duration::from_millis(PART_WAIT_INTERVAL_MS));
            if Path::new(part).exists() && std::fs::OpenOptions::new().read(true).open(part).is_ok()
            {
                return Ok(());
            }
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "partition {part} did not appear within {PART_WAIT_TIMEOUT_S}s \
                 after writing the partition table"
            );
        }
        tracing::info!(
            target: crate::engine::INSTALLATION_LOG_TARGET,
            "waiting for {part} to appear..."
        );
        std::thread::sleep(Duration::from_millis(PART_WAIT_INTERVAL_MS));
    }
}

pub fn mount_bootc_vartmp() -> Result<()> {
    let bootc_tmp = PathBuf::from(BOOTC_STORAGE_MOUNT).join("tmp");
    std::fs::create_dir_all(BOOTC_VAR_TMP)?;
    bind_mount(&bootc_tmp.to_string_lossy(), BOOTC_VAR_TMP)
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

pub fn rollback() {
    for mp in teardown_mount_points() {
        if path_is_mount(&mp) {
            tracing::info!(
                target: crate::engine::INSTALLATION_LOG_TARGET,
                "rollback: unmounting {mp}"
            );
            let _ = unmount(&mp);
        }
    }
}
