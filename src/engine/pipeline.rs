// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use nix::mount::MsFlags;

use crate::engine::partition::DerivedPartitions;
use crate::engine::subproc::{self, LogFn};

pub use crate::engine::steps::STEPS;

pub const DEPENDENCIES: &[&str] = &["bootc", "mke2fs"];

pub const TARGET_MOUNT: &str = "/run/puu-installer/target";
pub const TARGET_EFI_MOUNT: &str = "/run/puu-installer/target/efi";
pub const TARGET_HOME_MOUNT: &str = "/run/puu-installer/target/var/home";

#[allow(clippy::doc_markdown)]
/// Progress callback: (step_index, status_text, fraction)
pub type ProgressCb = Box<dyn FnMut(usize, &str, f64) + Send>;

pub const BOOTC_STORAGE_MOUNT: &str = "/run/puu-installer/target/var/tmp/puu-bootc-storage";
pub const BOOTC_IMAGE_STORAGE: &str =
    "/run/puu-installer/target/var/tmp/puu-bootc-storage/containers/storage";
pub const BOOTC_VAR_TMP: &str = "/var/tmp";
pub const CONTAINERS_RUNROOT: &str = "/run/containers/storage";
pub const BOOTC_RUNROOT: &str = "/run/puu-installer/bootc-runroot";

/// Mount `source` at `target` via `mount(2)`, creating the mountpoint and
/// skipping if it is already mounted. Replaces shelling out to `mount(8)`.
pub fn mount_target(
    log: &mut dyn FnMut(String),
    source: &str,
    target: &str,
    fstype: &str,
    data: Option<&str>,
) -> Result<()> {
    std::fs::create_dir_all(target)?;
    if path_is_mount(target) {
        log(format!("{target} is already mounted; reusing it"));
        return Ok(());
    }
    nix::mount::mount(Some(source), target, Some(fstype), MsFlags::empty(), data)
        .map_err(|e| anyhow::anyhow!("failed to mount {source} at {target}: {e}"))?;
    Ok(())
}

/// Whether `path` is a mount point, by comparing its device id against its
/// parent directory's.
pub fn path_is_mount(path: impl AsRef<Path>) -> bool {
    use std::os::unix::fs::MetadataExt;
    let path = path.as_ref();
    match (
        std::fs::metadata(path),
        std::fs::metadata(path.parent().unwrap_or(path)),
    ) {
        (Ok(self_meta), Ok(parent_meta)) => self_meta.dev() != parent_meta.dev(),
        _ => false,
    }
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
    pub last_error: String,
    pub start_from: usize,
    pub skip_steps: Vec<usize>,
    pub parts: DerivedPartitions,
}

pub struct ContextBuilder {
    config: crate::engine::config::Config,
    source_image: Option<PathBuf>,
    target_imgref: String,
    image_version: String,
    drive: String,
    k3s_token: String,
    start_from: usize,
    skip_steps: Vec<usize>,
}

impl ContextBuilder {
    pub fn new(
        config: crate::engine::config::Config,
        target_imgref: String,
        image_version: String,
    ) -> Self {
        Self {
            config,
            source_image: None,
            target_imgref,
            image_version,
            drive: String::new(),
            k3s_token: String::new(),
            start_from: 1,
            skip_steps: Vec::new(),
        }
    }

    pub fn source_image(mut self, source_image: Option<PathBuf>) -> Self {
        self.source_image = source_image;
        self
    }

    pub fn drive(mut self, drive: String) -> Self {
        self.drive = drive;
        self
    }

    pub fn k3s_token(mut self, k3s_token: String) -> Self {
        self.k3s_token = k3s_token;
        self
    }

    pub fn start_from(mut self, start_from: usize) -> Self {
        self.start_from = start_from;
        self
    }

    pub fn skip_steps(mut self, skip_steps: Vec<usize>) -> Self {
        self.skip_steps = skip_steps;
        self
    }

    pub fn build(self) -> Context {
        Context {
            config: self.config,
            source_image: self.source_image,
            target_imgref: self.target_imgref,
            image_version: self.image_version,
            drive: self.drive,
            drive_label: String::new(),
            username: String::new(),
            user_password: String::new(),
            user_shell: "/bin/bash".to_string(),
            keymap: String::new(),
            locale: String::new(),
            secure_boot: true,
            k3s_token: self.k3s_token,
            last_error: String::new(),
            start_from: self.start_from,
            skip_steps: self.skip_steps,
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

    /// Begin building a subprocess invocation. Use [`subproc::Cmd::env`],
    /// [`subproc::Cmd::heartbeat`] and [`subproc::Cmd::chroot`] to opt into
    /// extras, then [`subproc::Cmd::run`] to execute it.
    pub fn cmd<S: AsRef<str>>(&mut self, argv: &[S]) -> subproc::Cmd<'_> {
        let argv = argv.iter().map(|a| a.as_ref().to_string()).collect();
        subproc::Cmd::new(&mut self.log, argv)
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

fn mounted_target_points() -> Vec<String> {
    let mut points = Vec::new();
    let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return points;
    };

    for line in mountinfo.lines() {
        let Some(mount_point) = line.split_whitespace().nth(4) else {
            continue;
        };
        let mount_point = crate::engine::util::decode_mountinfo_path(mount_point);
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

pub fn cleanup_mounts(runner: &mut Runner) -> Result<()> {
    for mp in teardown_mount_points() {
        if path_is_mount(&mp) {
            runner.log(&format!("unmounting {mp}"));
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
        let openable =
            Path::new(part).exists() && std::fs::OpenOptions::new().read(true).open(part).is_ok();
        if openable {
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
        runner.log(&format!("waiting for {part} to appear..."));
        std::thread::sleep(Duration::from_millis(PART_WAIT_INTERVAL_MS));
    }
}

pub fn mount_bootc_vartmp() -> Result<bool> {
    let bootc_tmp = PathBuf::from(BOOTC_STORAGE_MOUNT).join("tmp");
    std::fs::create_dir_all(&bootc_tmp)?;
    std::fs::set_permissions(&bootc_tmp, PermissionsExt::from_mode(0o1777))?;

    std::fs::create_dir_all(BOOTC_VAR_TMP)?;
    std::fs::set_permissions(BOOTC_VAR_TMP, PermissionsExt::from_mode(0o1777))?;

    if path_is_mount(BOOTC_VAR_TMP) {
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
        if path_is_mount(&mp) {
            log(format!("rollback: unmounting {mp}"));
            let _ = unmount(&mp);
        }
    }
}
