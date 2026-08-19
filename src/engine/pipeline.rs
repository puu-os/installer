// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use nix::mount::MsFlags;
use nix::sched::{CloneFlags, unshare};
use uuid::Uuid;
use xattr::FileExt;
use zeroize::Zeroizing;

use crate::engine::command;
use crate::engine::config::{Arch, ImageRef, KernelArg, UserShell, Username};
use crate::engine::firmware::SecureBootMode;
use crate::engine::partition::DerivedPartitions;
use crate::engine::util::Mounts;

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
pub fn mount_target(
    source: impl AsRef<Path>,
    target: impl AsRef<Path>,
    fstype: &str,
    data: Option<&str>,
) -> Result<()> {
    let source = source.as_ref();
    let target = target.as_ref();
    std::fs::create_dir_all(target)?;
    if path_is_mount(target) {
        tracing::info!(
            target: crate::engine::INSTALLATION_LOG_TARGET,
            "{} is already mounted; reusing it",
            target.display()
        );
        return Ok(());
    }
    nix::mount::mount(Some(source), target, Some(fstype), MsFlags::empty(), data).map_err(|e| {
        anyhow::anyhow!(
            "failed to mount {} at {}: {e}",
            source.display(),
            target.display()
        )
    })?;
    Ok(())
}

/// Whether `path` is an exact mount point according to `/proc/self/mountinfo`.
pub fn path_is_mount(path: impl AsRef<Path>) -> bool {
    Mounts::current().contains_mountpoint(path)
}

pub fn bind_mount(source: impl AsRef<Path>, target: impl AsRef<Path>) -> Result<()> {
    let source = source.as_ref();
    let target = target.as_ref();
    nix::mount::mount(
        Some(source),
        target,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .map_err(|e| {
        anyhow::anyhow!(
            "failed to bind-mount {} at {}: {e}",
            source.display(),
            target.display()
        )
    })
}

pub fn unmount(target: impl AsRef<Path>) -> Result<()> {
    let target = target.as_ref();
    nix::mount::umount(target)
        .map_err(|e| anyhow::anyhow!("failed to unmount {}: {e}", target.display()))
}

#[derive(Clone)]
pub struct Context {
    pub config: crate::engine::config::Config,
    pub source_image: Option<PathBuf>,
    pub target_imgref: ImageRef,
    pub image_version: Option<String>,
    pub drive: PathBuf,
    pub drive_label: Option<String>,
    pub username: Option<Username>,
    pub user_password: Zeroizing<String>,
    pub user_shell: UserShell,
    pub secure_boot: SecureBootMode,
    pub k3s_token: Zeroizing<String>,
    pub parts: DerivedPartitions,
}

impl Default for Context {
    fn default() -> Self {
        Self {
            config: crate::engine::config::Config::default(),
            source_image: None,
            target_imgref: ImageRef::default(),
            image_version: None,
            drive: PathBuf::new(),
            drive_label: None,
            username: None,
            user_password: Zeroizing::new(String::new()),
            user_shell: UserShell::default(),
            secure_boot: SecureBootMode::default(),
            k3s_token: Zeroizing::new(String::new()),
            parts: DerivedPartitions::default(),
        }
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

impl Step {
    pub const fn new(title: &'static str, fn_: StepFn) -> Self {
        Self {
            title,
            fn_,
            point_of_no_return: false,
            requires_secure_boot: false,
            weight: 1,
        }
    }

    #[must_use]
    pub const fn point_of_no_return(mut self) -> Self {
        self.point_of_no_return = true;
        self
    }

    #[must_use]
    pub const fn requires_secure_boot(mut self) -> Self {
        self.requires_secure_boot = true;
        self
    }

    #[must_use]
    pub const fn weight(mut self, weight: u32) -> Self {
        self.weight = weight;
        self
    }
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

impl Default for CleanupStack {
    fn default() -> Self {
        Self::new()
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
    pub fn cmd_tracked(
        &mut self,
        program: impl AsRef<OsStr>,
        source: Box<dyn crate::engine::progress::ProgressSource>,
    ) -> command::Command<'_> {
        let step_index = self.step_index;
        let progress = &mut self.progress;
        let emit = move |fraction: f64| (progress)(step_index, fraction);
        command::Command::new(program).track(source, Box::new(emit))
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
            if step.requires_secure_boot && self.ctx.secure_boot == SecureBootMode::Skip {
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

/// Mount points to tear down, in unmount order: live target mounts plus
/// chroot bind mounts and target filesystems, deepest first.
fn teardown_mount_points() -> Vec<PathBuf> {
    let mounts = Mounts::current();
    let mut points: Vec<PathBuf> = mounts
        .iter()
        .filter(|info| {
            info.mountpoint.starts_with(TARGET_MOUNT) || info.mountpoint == Path::new(BOOTC_VAR_TMP)
        })
        .map(|info| info.mountpoint.clone())
        .collect();

    if mounts.contains_mountpoint(BOOTC_VAR_TMP) {
        points.push(PathBuf::from(BOOTC_VAR_TMP));
    }
    for (_, rel) in CHROOT_BIND_MOUNTS.iter().rev() {
        let dst = Path::new(TARGET_MOUNT).join(rel);
        if mounts.contains_mountpoint(&dst) {
            points.push(dst);
        }
    }
    for mp in TARGET_MOUNTS {
        if mounts.contains_mountpoint(*mp) {
            points.push(PathBuf::from(*mp));
        }
    }

    points.sort_by_key(|point| std::cmp::Reverse(point.as_os_str().len()));
    points.dedup();
    points
}

pub fn cleanup_mounts() -> Result<()> {
    for mp in teardown_mount_points() {
        tracing::info!(
            target: crate::engine::INSTALLATION_LOG_TARGET,
            "unmounting {}",
            mp.display()
        );
        unmount(&mp)?;
    }
    Ok(())
}

pub fn mount_chroot_binds() -> Result<Vec<PathBuf>> {
    let mut mounted = Vec::new();
    for (src, rel) in CHROOT_BIND_MOUNTS {
        let dst = Path::new(TARGET_MOUNT).join(rel);
        std::fs::create_dir_all(&dst)?;
        if path_is_mount(&dst) {
            continue;
        }
        bind_mount(src, &dst)?;
        mounted.push(dst);
    }
    Ok(mounted)
}

pub fn unmount_chroot_binds(mounted: &[impl AsRef<Path>]) -> Result<()> {
    for dst in mounted.iter().rev() {
        unmount(dst)?;
    }
    Ok(())
}

pub fn merge_fstab(existing: &str, efi_uuid: &Uuid, home_uuid: &Uuid) -> String {
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

pub fn install_kernel_args(
    root_uuid: &Uuid,
    config: &crate::engine::config::Config,
) -> Result<Vec<KernelArg>> {
    let machine = Arch::host()?;
    let mut args = vec![
        format!("root=UUID={root_uuid}").parse()?,
        "rw".parse()?,
        "vt.global_cursor_default=0".parse()?,
        "console=tty0".parse()?,
        "threadirqs".parse()?,
        "preempt=full".parse()?,
    ];
    let serial_console = match config.serial_console.get(&machine) {
        Some(console) => Some(console.clone()),
        None => match machine {
            Arch::X86_64 => Some("console=ttyS0,115200".parse()?),
            Arch::Aarch64 => Some("console=ttyAMA0,115200".parse()?),
        },
    };
    if let Some(console) = serial_console {
        args.push(console);
    }
    Ok(args)
}

const PART_WAIT_TIMEOUT_S: u64 = 30;
const PART_WAIT_INTERVAL_MS: u64 = 500;

pub fn wait_for_block_device(part: impl AsRef<Path>) -> Result<()> {
    let part = part.as_ref();
    let mut was_ready = false;
    crate::engine::util::wait_until(
        Duration::from_secs(PART_WAIT_TIMEOUT_S),
        Duration::from_millis(PART_WAIT_INTERVAL_MS),
        || {
            let ready = part.exists() && std::fs::OpenOptions::new().read(true).open(part).is_ok();
            let stable = ready && was_ready;
            was_ready = ready;
            if !ready {
                tracing::info!(
                    target: crate::engine::INSTALLATION_LOG_TARGET,
                    "waiting for {} to appear...",
                    part.display()
                );
            }
            stable
        },
    )
    .with_context(|| format!("partition {} did not appear", part.display()))
}

pub fn mount_bootc_vartmp() -> Result<()> {
    let bootc_tmp = PathBuf::from(BOOTC_STORAGE_MOUNT).join("tmp");
    std::fs::create_dir_all(BOOTC_VAR_TMP)?;
    bind_mount(&bootc_tmp, BOOTC_VAR_TMP)
}

pub fn deployment_root() -> Result<PathBuf> {
    let deploy = Path::new(TARGET_MOUNT).join("ostree/deploy");
    let mut found = Vec::new();
    let os_entries = std::fs::read_dir(&deploy).map_err(|e| {
        anyhow::anyhow!(
            "failed to read ostree deployments at {}: {e}",
            deploy.display()
        )
    })?;
    for os in os_entries.flatten() {
        let deployments = os.path().join("deploy");
        let Ok(entries) = std::fs::read_dir(&deployments) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if path
                .extension()
                .is_some_and(|ext| ext.as_encoded_bytes() == b"origin")
            {
                continue;
            }
            found.push(path);
        }
    }
    found.sort();
    found
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("ostree did not report the installed deployment root"))
}

pub fn flatpak_manifest_refs(path: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|line| {
            let value = line.split_once('#').map_or(line, |(value, _)| value).trim();
            (!value.is_empty()).then(|| value.to_string())
        })
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

pub fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        std::fs::remove_dir_all(dst)?;
    }
    let mut hardlinks = HashMap::new();
    copy_tree_entry(src, dst, &mut hardlinks)
}

fn copy_tree_entry(
    src: &Path,
    dst: &Path,
    hardlinks: &mut HashMap<(u64, u64), PathBuf>,
) -> Result<()> {
    let metadata = std::fs::symlink_metadata(src)?;
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_tree_entry(&entry.path(), &dst.join(entry.file_name()), hardlinks)?;
        }
        std::fs::set_permissions(dst, metadata.permissions())?;
        copy_xattrs(src, dst)?;
    } else if file_type.is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)?;
    } else if file_type.is_file() {
        let key = (metadata.dev(), metadata.ino());
        if metadata.nlink() > 1 {
            if let Some(existing) = hardlinks.get(&key) {
                std::fs::hard_link(existing, dst)?;
                return Ok(());
            }
        }
        std::fs::copy(src, dst)?;
        std::fs::set_permissions(dst, metadata.permissions())?;
        copy_xattrs(src, dst)?;
        if metadata.nlink() > 1 {
            hardlinks.insert(key, dst.to_path_buf());
        }
    } else {
        bail!("unsupported file type at {}", src.display());
    }
    Ok(())
}

fn copy_xattrs(src: &Path, dst: &Path) -> Result<()> {
    let src_file = std::fs::File::open(src)?;
    let dst_file = std::fs::File::open(dst)?;
    for name in src_file.list_xattr()? {
        if !name.as_encoded_bytes().starts_with(b"user.") {
            continue;
        }
        if let Some(value) = src_file.get_xattr(&name)? {
            dst_file.set_xattr(&name, &value)?;
        }
    }
    Ok(())
}

pub fn rollback() {
    for mp in teardown_mount_points() {
        tracing::info!(
            target: crate::engine::INSTALLATION_LOG_TARGET,
            "rollback: unmounting {}",
            mp.display()
        );
        let _ = unmount(&mp);
    }
}
