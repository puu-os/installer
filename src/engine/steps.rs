// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::ffi::OsStr;
use std::io::Write as _;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use efivar::boot::{
    BootEntry, BootEntryAttributes, EFIHardDrive, EFIHardDriveType, FilePath, FilePathList,
};
use zeroize::Zeroize;

use crate::engine::command::Command;
use crate::engine::config::Timezone;
use crate::engine::firmware::{SecureBootMode, SetupMode};
use crate::engine::image;
use crate::engine::partition;
use crate::engine::pipeline::{
    self, BOOTC_IMAGE_STORAGE, BOOTC_RUNROOT, BOOTC_STORAGE_MOUNT, BOOTC_VAR_TMP,
    CONTAINERS_RUNROOT, DEPENDENCIES, Runner, Step, TARGET_EFI_MOUNT, TARGET_HOME_MOUNT,
    TARGET_MOUNT,
};
use crate::engine::progress;

fn step_dependencies(_r: &mut Runner) -> Result<()> {
    let missing: Vec<&str> = DEPENDENCIES
        .iter()
        .copied()
        .filter(|d| !crate::engine::util::which_exists(d))
        .collect();
    if !missing.is_empty() {
        tracing::info!(target: crate::engine::INSTALLATION_LOG_TARGET, "missing required tools:");
        for dep in &missing {
            tracing::info!(target: crate::engine::INSTALLATION_LOG_TARGET, "  {dep}");
        }
        bail!("missing dependencies: {}", missing.join(", "));
    }
    tracing::info!(
        target: crate::engine::INSTALLATION_LOG_TARGET,
        "all {} dependencies present",
        DEPENDENCIES.len()
    );
    Ok(())
}

fn step_setup_mode(r: &mut Runner) -> Result<()> {
    if r.ctx.secure_boot == SecureBootMode::Skip {
        tracing::info!(
            target: crate::engine::INSTALLATION_LOG_TARGET,
            "Secure Boot disabled; skipping firmware setup check"
        );
        return Ok(());
    }
    match crate::engine::firmware::detect_setup_mode() {
        SetupMode::Unknown { reason } => {
            r.ctx.secure_boot = SecureBootMode::Skip;
            tracing::info!(
                target: crate::engine::INSTALLATION_LOG_TARGET,
                "Secure Boot Setup Mode could not be detected; \
                 continuing without Secure Boot"
            );
            if !reason.is_empty() {
                tracing::info!(
                    target: crate::engine::INSTALLATION_LOG_TARGET,
                    "  reason: {reason}"
                );
            }
            Ok(())
        }
        SetupMode::Off => {
            bail!(
                "Secure Boot Setup Mode is off. Reboot to firmware setup or \
                 continue without Secure Boot."
            );
        }
        SetupMode::On { secure_boot } => {
            let state = if secure_boot == Some(true) {
                "enabled"
            } else {
                "disabled"
            };
            tracing::info!(
                target: crate::engine::INSTALLATION_LOG_TARGET,
                "firmware reports Setup Mode on; Secure Boot is {state}"
            );
            Ok(())
        }
    }
}

fn step_cleanup(_r: &mut Runner) -> Result<()> {
    pipeline::cleanup_mounts()
}

fn validate_target_drive(
    drive: impl AsRef<Path>,
    config: &crate::engine::config::Config,
) -> Result<u64> {
    let drive = drive.as_ref();
    let metadata = std::fs::metadata(drive)
        .with_context(|| format!("failed to inspect target drive {}", drive.display()))?;
    if !metadata.file_type().is_block_device() {
        bail!("'{}' is not a block device", drive.display());
    }
    if let Some(reason) = crate::engine::block::disk_busy(drive) {
        bail!("'{}' is busy: {reason}", drive.display());
    }

    let size_b = crate::engine::block::drive_size_bytes(drive)?;
    let min = partition::min_target_size_bytes(config)?;
    if size_b < min {
        bail!(
            "'{}' is too small: {} detected, need at least {} for EFI, \
             root and home partitions",
            drive.display(),
            crate::engine::util::human_size(size_b),
            crate::engine::util::human_size(min)
        );
    }
    Ok(size_b)
}

fn step_validate_drive(r: &mut Runner) -> Result<()> {
    let drive = &r.ctx.drive;
    let size_b = validate_target_drive(drive, &r.ctx.config)?;
    if let Some(ref src) = r.ctx.source_image {
        if !src.is_file() && !src.is_dir() {
            bail!("source bootc image not found at {}", src.display());
        }
    }

    r.ctx.parts = partition::derive_partitions(drive);

    tracing::info!(
        target: crate::engine::INSTALLATION_LOG_TARGET,
        "target {} ({}; {}, {}, {})",
        drive.display(),
        crate::engine::util::human_size(size_b),
        r.ctx.parts.efi_part.display(),
        r.ctx.parts.root_part.display(),
        r.ctx.parts.home_part.display()
    );
    Ok(())
}

fn step_partition(r: &mut Runner) -> Result<()> {
    let drive = r.ctx.drive.clone();
    // Revalidate immediately before taking exclusive access and writing the GPT.
    validate_target_drive(&drive, &r.ctx.config)?;
    r.ctx.parts.layout = Some(partition::partition_drive(
        &drive,
        &r.ctx.config.partitions.efi,
        &r.ctx.config.partitions.home,
    )?);
    // Wait for the udev event queue to drain (replaces `udevadm settle`).
    crate::engine::block::wait_for_udev_settle(Duration::from_secs(30))?;

    pipeline::wait_for_block_device(&r.ctx.parts.efi_part)?;
    pipeline::wait_for_block_device(&r.ctx.parts.root_part)?;
    pipeline::wait_for_block_device(&r.ctx.parts.home_part)?;
    Ok(())
}

fn step_format_efi(r: &mut Runner) -> Result<()> {
    let part = r.ctx.parts.efi_part.clone();
    pipeline::wait_for_block_device(&part)?;
    tracing::info!(
        target: crate::engine::INSTALLATION_LOG_TARGET,
        "formatting {} as FAT32 (label EFI)",
        part.display()
    );
    Command::new(&[
        OsStr::new("mkfs.fat"),
        OsStr::new("-F32"),
        OsStr::new("-n"),
        OsStr::new("EFI"),
        part.as_os_str(),
    ])
    .run()?;
    r.ctx.parts.efi_uuid = Some(crate::engine::block::capture_partuuid(&part)?);
    Ok(())
}

fn step_format_root(r: &mut Runner) -> Result<()> {
    let part = r.ctx.parts.root_part.clone();
    let label = r.ctx.config.labels.root.clone();
    pipeline::wait_for_block_device(&part)?;
    tracing::info!(
        target: crate::engine::INSTALLATION_LOG_TARGET,
        "formatting {} as XFS (label {label})",
        part.display()
    );
    Command::new(&[
        OsStr::new("mkfs.xfs"),
        OsStr::new("-f"),
        OsStr::new("-L"),
        OsStr::new(label.as_str()),
        part.as_os_str(),
    ])
    .run()?;
    r.ctx.parts.root_uuid = Some(crate::engine::block::capture_uuid(&part)?);
    Ok(())
}

fn step_format_home(r: &mut Runner) -> Result<()> {
    let part = r.ctx.parts.home_part.clone();
    let label = r.ctx.config.labels.home.clone();
    pipeline::wait_for_block_device(&part)?;
    Command::new(&[
        OsStr::new("mke2fs"),
        OsStr::new("-t"),
        OsStr::new("ext4"),
        OsStr::new("-O"),
        OsStr::new("encrypt"),
        OsStr::new("-F"),
        OsStr::new("-L"),
        OsStr::new(label.as_str()),
        part.as_os_str(),
    ])
    .run()?;
    r.ctx.parts.home_uuid = Some(crate::engine::block::capture_uuid(&part)?);
    Ok(())
}

fn mount_efi(r: &mut Runner) -> Result<()> {
    pipeline::mount_target(
        &r.ctx.parts.efi_part,
        TARGET_EFI_MOUNT,
        "vfat",
        Some("fmask=0077,dmask=0077"),
    )
}

fn step_mount(r: &mut Runner) -> Result<()> {
    pipeline::mount_target(&r.ctx.parts.root_part, TARGET_MOUNT, "xfs", None)
}

fn step_mount_home(r: &mut Runner) -> Result<()> {
    pipeline::mount_target(&r.ctx.parts.home_part, TARGET_HOME_MOUNT, "ext4", None)?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn step_bootc_install(r: &mut Runner) -> Result<()> {
    let Some(root_uuid) = r.ctx.parts.root_uuid else {
        bail!("root partition UUID is unset; re-run without skipping the format steps");
    };

    let source_image = r.ctx.source_image.clone();
    let source_image = if source_image.is_none() {
        let default = image::default_source_image(&r.ctx.config);
        r.ctx.source_image.clone_from(&default);
        if let Some(ref img) = default {
            r.ctx.target_imgref = image::load_target_imgref(Some(img), &r.ctx.config)?;
            r.ctx.image_version = image::load_image_version(Some(img));
            tracing::info!(
                target: crate::engine::INSTALLATION_LOG_TARGET,
                "using local bootc image source: {}",
                img.display()
            );
        }
        default
    } else {
        source_image
    };

    if source_image.is_none() && !image::running_in_container() {
        bail!(
            "no local bootc source image found; expected \
             /dev/disk/by-label/{}",
            r.ctx.config.image.source_label
        );
    }

    let mut cleanups = pipeline::CleanupStack::default();

    if !Path::new(BOOTC_STORAGE_MOUNT).exists() {
        std::fs::create_dir_all(BOOTC_STORAGE_MOUNT)?;
        cleanups.push(|| {
            let _ = std::fs::remove_dir_all(BOOTC_STORAGE_MOUNT);
        });
    }

    for dir in &[BOOTC_IMAGE_STORAGE, CONTAINERS_RUNROOT, BOOTC_RUNROOT] {
        std::fs::create_dir_all(dir)?;
    }

    let bootc_tmp_dir = PathBuf::from(BOOTC_STORAGE_MOUNT).join("tmp");
    std::fs::create_dir_all(&bootc_tmp_dir)?;
    std::fs::set_permissions(&bootc_tmp_dir, PermissionsExt::from_mode(0o1777))?;

    if !pipeline::path_is_mount(BOOTC_IMAGE_STORAGE) {
        pipeline::bind_mount(BOOTC_IMAGE_STORAGE, BOOTC_IMAGE_STORAGE)?;
        cleanups.push(|| {
            let _ = pipeline::unmount(BOOTC_IMAGE_STORAGE);
        });
    }
    if !pipeline::path_is_mount(&bootc_tmp_dir) {
        pipeline::bind_mount(&bootc_tmp_dir, &bootc_tmp_dir)?;
        let bootc_tmp_dir = bootc_tmp_dir.clone();
        cleanups.push(move || {
            let _ = pipeline::unmount(&bootc_tmp_dir);
        });
    }

    pipeline::mount_bootc_vartmp()?;
    cleanups.push(|| {
        let _ = pipeline::unmount(BOOTC_VAR_TMP);
    });

    let storage_conf = PathBuf::from("/run/puu-installer/bootc-storage.conf");
    std::fs::write(
        &storage_conf,
        format!(
            "[storage]\ndriver = \"overlay\"\ngraphroot = \"{BOOTC_IMAGE_STORAGE}\"\n\
             runroot = \"{CONTAINERS_RUNROOT}\"\n"
        ),
    )?;

    let mut argv: Vec<String> = vec![
        "unshare".into(),
        "--pid".into(),
        "--fork".into(),
        "--mount-proc".into(),
        "--".into(),
        "bootc".into(),
        "install".into(),
        "to-filesystem".into(),
    ];

    if let Some(ref src) = source_image {
        argv.push("--source-imgref".into());
        argv.push(image::image_source_ref(src).to_string());
    }

    argv.extend([
        "--target-imgref".into(),
        r.ctx.target_imgref.to_string(),
        "--skip-fetch-check".into(),
        "--bootloader".into(),
        "none".into(),
        "--skip-finalize".into(),
    ]);

    for arg in pipeline::install_kernel_args(&root_uuid, &r.ctx.config)? {
        argv.push("--karg".into());
        argv.push(arg.to_string());
    }

    argv.push(TARGET_MOUNT.into());

    tracing::info!(
        target: crate::engine::INSTALLATION_LOG_TARGET,
        "bootc image deployment may take several minutes without native progress output"
    );

    let env = [
        (
            OsStr::new("CONTAINERS_STORAGE_CONF"),
            storage_conf.as_os_str(),
        ),
        (OsStr::new("TMPDIR"), OsStr::new(BOOTC_VAR_TMP)),
    ];

    let tracker = image::install_size_estimate(source_image.as_deref())
        .and_then(|expected| progress::DiskWriteProgress::new(TARGET_MOUNT, expected).ok())
        .map(|source| Box::new(source) as Box<dyn progress::ProgressSource>);
    let cmd = match tracker {
        Some(source) => r.cmd_tracked(&argv, source),
        None => Command::new(&argv),
    };
    cmd.env(&env)
        .heartbeat(
            Duration::from_secs(30),
            "bootc install still running; deploying image",
        )
        .run()?;

    Ok(())
}

fn step_configure_mounts(r: &mut Runner) -> Result<()> {
    let (Some(efi_uuid), Some(home_uuid)) = (r.ctx.parts.efi_uuid, r.ctx.parts.home_uuid) else {
        bail!("EFI or home partition UUID is unset; re-run without skipping the format steps");
    };

    let deploy = pipeline::deployment_root()?;
    for dir in &["efi", "var", "var/home"] {
        std::fs::create_dir_all(deploy.join(dir))?;
    }
    let etc = deploy.join("etc");
    std::fs::create_dir_all(&etc)?;
    let fstab = etc.join("fstab");
    let old = if fstab.exists() {
        std::fs::read_to_string(&fstab)?
    } else {
        String::new()
    };
    let new = pipeline::merge_fstab(&old, &efi_uuid, &home_uuid);
    std::fs::write(&fstab, &new)?;
    std::fs::set_permissions(&fstab, PermissionsExt::from_mode(0o644))?;
    Ok(())
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn step_verify_image(r: &mut Runner) -> Result<()> {
    let Some(src) = r.ctx.source_image.clone() else {
        tracing::info!(
            target: crate::engine::INSTALLATION_LOG_TARGET,
            "using running bootc image as install source"
        );
        return Ok(());
    };
    let expected = r.ctx.config.image.expected_digest.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "image.expectedDigest is required when installing from a local image source"
        )
    })?;

    tracing::info!(
        target: crate::engine::INSTALLATION_LOG_TARGET,
        "expected image digest: {expected}"
    );
    let raw = image::raw_manifest(&src)?;
    let actual = format!("sha256:{}", hex_lower(&openssl::sha::sha256(&raw)));
    if !actual.eq_ignore_ascii_case(expected.as_str()) {
        bail!("image digest mismatch: expected {expected}, got {actual}");
    }
    tracing::info!(
        target: crate::engine::INSTALLATION_LOG_TARGET,
        "image digest verified: {actual}"
    );
    Ok(())
}

fn timezone_in_image(deploy: &Path, timezone: &Timezone) -> Result<()> {
    let path = deploy.join("usr/share/zoneinfo").join(timezone.as_str());
    if !path.is_file() {
        bail!("timezone not found in target system: {timezone}");
    }
    Ok(())
}

fn step_firstboot(r: &mut Runner) -> Result<()> {
    let ident = &r.ctx.config.identity;
    let keymap = ident.keymap.as_str();
    let locale = ident.locale.as_str();

    let timezone = &ident.timezone;
    let hostname = ident.hostname.clone();

    let deploy = pipeline::deployment_root()?;
    timezone_in_image(&deploy, timezone)?;

    // Equivalent of `systemd-firstboot --root`: write the config files directly,
    // and like firstboot, never overwrite values already present in the image.
    let etc = deploy.join("etc");
    std::fs::create_dir_all(&etc)?;
    let write_if_absent = |path: PathBuf, contents: String| -> Result<()> {
        if !path.exists() {
            std::fs::write(&path, contents)?;
            std::fs::set_permissions(&path, PermissionsExt::from_mode(0o644))?;
        }
        Ok(())
    };
    write_if_absent(etc.join("hostname"), format!("{hostname}\n"))?;
    if !locale.is_empty() {
        write_if_absent(etc.join("locale.conf"), format!("LANG={locale}\n"))?;
    }
    let link = etc.join("localtime");
    if !link.exists() {
        std::os::unix::fs::symlink(
            Path::new("../usr/share/zoneinfo").join(timezone.as_str()),
            &link,
        )?;
    }

    if !keymap.is_empty() {
        let vconsole = etc.join("vconsole.conf");
        std::fs::write(&vconsole, format!("KEYMAP={keymap}\n"))?;
        std::fs::set_permissions(&vconsole, PermissionsExt::from_mode(0o644))?;
        tracing::info!(
            target: crate::engine::INSTALLATION_LOG_TARGET,
            "staged keymap {keymap} in /etc/vconsole.conf"
        );
    }
    Ok(())
}

fn step_configure_k3s_cluster(r: &mut Runner) -> Result<()> {
    if r.ctx.config.k3s.role == crate::engine::config::K3sRole::Server
        && r.ctx.config.k3s.node_name.is_none()
    {
        tracing::info!(
            target: crate::engine::INSTALLATION_LOG_TARGET,
            "k3s cluster: using image default server configuration"
        );
        return Ok(());
    }

    let mut argv: Vec<String> = vec![
        r.ctx.config.k3s.helper.display().to_string(),
        "--role".into(),
        r.ctx.config.k3s.role.to_string(),
    ];
    if let Some(server_url) = &r.ctx.config.k3s.server_url {
        argv.push("--server-url".into());
        argv.push(server_url.to_string());
    }

    let mut cleanups = pipeline::CleanupStack::default();
    if !r.ctx.k3s_token.is_empty() {
        let token_path = Path::new(TARGET_MOUNT).join("etc/rancher/k3s/puu-token");
        let parent = token_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("token path has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let mut token_file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&token_path)
            .with_context(|| {
                format!("failed to create temporary token {}", token_path.display())
            })?;
        token_file.set_permissions(PermissionsExt::from_mode(0o600))?;
        let cleanup_path = token_path.clone();
        cleanups.push(move || {
            let _ = std::fs::remove_file(cleanup_path);
        });
        token_file.write_all(r.ctx.k3s_token.as_bytes())?;
        token_file.write_all(b"\n")?;
        token_file.sync_all()?;
        argv.push("--token-file".into());
        argv.push("/etc/rancher/k3s/puu-token".into());
    }
    if let Some(node_name) = &r.ctx.config.k3s.node_name {
        argv.push("--node-name".into());
        argv.push(node_name.to_string());
    }
    Command::new(&argv).chroot(TARGET_MOUNT).run()?;
    drop(cleanups);
    r.ctx.k3s_token.zeroize();

    tracing::info!(
        target: crate::engine::INSTALLATION_LOG_TARGET,
        "k3s cluster: staged {} configuration",
        r.ctx.config.k3s.role
    );
    Ok(())
}

fn step_stage_homed_identity(r: &mut Runner) -> Result<()> {
    let name = r
        .ctx
        .username
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("username is required to stage a homed identity"))?;
    if r.ctx.user_password.contains('\n') || r.ctx.user_password.contains('\r') {
        bail!("password must not contain newline characters");
    }

    let hashed = crate::engine::passwd::hash_password_sha512(&r.ctx.user_password)?;

    let identity = serde_json::json!({
        "userName": name.as_str(),
        "secret": { "password": [r.ctx.user_password.as_str()] },
        "realName": name.as_str(),
        "shell": r.ctx.user_shell.path(),
        "memberOf": r.ctx.config.homed.groups.as_slice(),
        "homeDirectory": Path::new("/var/home").join(name.as_str()),
        "imagePath": Path::new("/var/home").join(format!("{name}.homedir")),
        "storage": "fscrypt",
        "preferredSessionLauncher": "gnome",
        "enforcePasswordPolicy": false,
        "privileged": {
            "hashedPassword": [&hashed],
        },
    });

    let deployment = pipeline::deployment_root()?;
    let ostree_deploy_dir = deployment.parent().and_then(Path::parent).ok_or_else(|| {
        anyhow::anyhow!(
            "deployment root has unexpected path: {}",
            deployment.display()
        )
    })?;
    let target = ostree_deploy_dir.join("var/lib/puu/identity.json");
    {
        let parent = target
            .parent()
            .ok_or_else(|| anyhow::anyhow!("identity target path has no parent"))?;
        std::fs::create_dir_all(parent)?;
    }
    let mut identity_file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&target)
        .with_context(|| format!("failed to create identity file {}", target.display()))?;
    identity_file.set_permissions(PermissionsExt::from_mode(0o600))?;
    serde_json::to_writer_pretty(&mut identity_file, &identity)?;
    identity_file.write_all(b"\n")?;
    identity_file.sync_all()?;
    drop(identity);
    r.ctx.user_password.zeroize();
    tracing::info!(
        target: crate::engine::INSTALLATION_LOG_TARGET,
        "staged homed identity for {name} at /var/lib/puu/identity.json"
    );
    Ok(())
}

fn step_flatpak_seed(r: &mut Runner) -> Result<()> {
    let manifest_path = r.ctx.config.flatpak.manifest_path.as_path();
    let manifest =
        Path::new(TARGET_MOUNT).join(manifest_path.strip_prefix("/").unwrap_or(manifest_path));
    let refs = pipeline::flatpak_manifest_ref_count(&manifest);
    if refs == 0 {
        tracing::info!(
            target: crate::engine::INSTALLATION_LOG_TARGET,
            "flatpak-seed: no Flatpak refs in manifest"
        );
        return Ok(());
    }

    let helper = r.ctx.config.flatpak.sync_helper.clone();
    let sync = Path::new(TARGET_MOUNT).join(helper.strip_prefix("/").unwrap_or(&helper));
    if !sync.is_file() {
        bail!("flatpak-sync helper not found at {}", sync.display());
    }

    tracing::info!(
        target: crate::engine::INSTALLATION_LOG_TARGET,
        "flatpak-seed: installing {} system Flatpak refs",
        refs
    );
    let mounted = pipeline::mount_chroot_binds()?;
    let source = Box::new(progress::MarkerProgress::default()) as Box<dyn progress::ProgressSource>;
    let result = r
        .cmd_tracked(&[helper.as_os_str()], source)
        .chroot(TARGET_MOUNT)
        .heartbeat(
            Duration::from_secs(30),
            "flatpak pre-seed still running; downloading apps",
        )
        .run();
    pipeline::unmount_chroot_binds(&mounted)?;
    result?;
    tracing::info!(
        target: crate::engine::INSTALLATION_LOG_TARGET,
        "flatpak-seed: system Flatpaks installed; system flatpak-sync.timer \
         will keep them reconciled"
    );
    Ok(())
}

fn step_install_bootloader(r: &mut Runner) -> Result<()> {
    mount_efi(r)?;
    let source = systemd_boot_source()?;
    let arch = efi_arch_suffix()?;
    let systemd_dir = Path::new(TARGET_EFI_MOUNT).join("EFI/systemd");
    let fallback_dir = Path::new(TARGET_EFI_MOUNT).join("EFI/BOOT");
    std::fs::create_dir_all(&systemd_dir)?;
    std::fs::create_dir_all(&fallback_dir)?;
    std::fs::copy(&source, systemd_dir.join(format!("systemd-boot{arch}.efi")))?;
    std::fs::copy(&source, fallback_dir.join(format!("BOOT{arch}.EFI")))?;
    tracing::info!(
        target: crate::engine::INSTALLATION_LOG_TARGET,
        "installed systemd-boot on the ESP"
    );
    Ok(())
}

fn step_nvram(r: &mut Runner) -> Result<()> {
    mount_efi(r)?;
    let arch = efi_arch_suffix()?;
    if r.ctx.parts.layout.is_none() {
        r.ctx.parts.layout = Some(partition::load_partition_layout(&r.ctx.drive)?);
    }
    let layout = r
        .ctx
        .parts
        .layout
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("EFI partition layout metadata is missing"))?;
    let partition_sig = uuid::Builder::from_bytes_le(layout.efi.unique_guid).into_uuid();
    let file_path_list = FilePathList {
        hard_drive: EFIHardDrive {
            partition_number: layout.efi.number,
            partition_start: layout.efi.starting_lba,
            partition_size: layout.efi.ending_lba - layout.efi.starting_lba + 1,
            partition_sig,
            format: 2,
            sig_type: EFIHardDriveType::Gpt,
        },
        file_path: FilePath {
            path: format!(r"\EFI\systemd\systemd-boot{arch}.efi"),
        },
    };
    let entry = BootEntry {
        attributes: BootEntryAttributes::LOAD_OPTION_ACTIVE,
        description: r.ctx.config.branding.name.to_string(),
        file_path_list: Some(file_path_list),
        optional_data: Vec::new(),
    };

    let mut vars = efivar::system();
    let boot_order_var = efivar::efi::Variable::new("BootOrder");
    let had_boot_order = vars
        .exists(&boot_order_var)
        .map_err(|e| anyhow::anyhow!("failed to inspect BootOrder: {e}"))?;
    let old_order = if had_boot_order {
        vars.get_boot_order()
            .map_err(|e| anyhow::anyhow!("failed to read BootOrder: {e}"))?
    } else {
        Vec::new()
    };
    let boot_id = first_free_boot_id(&old_order, vars.as_ref())?;
    vars.add_boot_entry(boot_id, entry)
        .map_err(|e| anyhow::anyhow!("failed to write Boot{boot_id:04X}: {e}"))?;

    let mut new_order = vec![boot_id];
    new_order.extend(old_order.iter().copied().filter(|id| *id != boot_id));
    if let Err(error) = vars.set_boot_order(new_order) {
        let boot_var = efivar::efi::Variable::new(&format!("Boot{boot_id:04X}"));
        let delete_entry = vars.delete(&boot_var);
        let restore_order = if had_boot_order {
            vars.set_boot_order(old_order)
        } else {
            match vars.exists(&boot_order_var) {
                Ok(true) => vars.delete(&boot_order_var),
                Ok(false) => Ok(()),
                Err(error) => Err(error),
            }
        };
        bail!(
            "failed to update BootOrder: {error}; rollback Boot{boot_id:04X}: {}; \
             rollback BootOrder: {}",
            result_status(&delete_entry),
            result_status(&restore_order)
        );
    }
    tracing::info!(
        target: crate::engine::INSTALLATION_LOG_TARGET,
        "registered Boot{boot_id:04X} and placed it first in BootOrder"
    );
    Ok(())
}

fn systemd_boot_source() -> Result<PathBuf> {
    let dirs = [
        Path::new(TARGET_MOUNT).join("usr/lib/systemd/boot/efi"),
        PathBuf::from("/usr/lib/systemd/boot/efi"),
    ];

    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        for entry in
            std::fs::read_dir(&dir).with_context(|| format!("failed to read {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let has_efi_extension = Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("efi"));
            if name.starts_with("systemd-boot") && has_efi_extension {
                return Ok(path);
            }
        }
    }

    bail!("systemd-boot EFI binary not found")
}

fn efi_arch_suffix() -> Result<&'static str> {
    Ok(crate::engine::config::Arch::host()?.efi_suffix())
}

fn first_free_boot_id(order: &[u16], vars: &dyn efivar::VarManager) -> Result<u16> {
    for id in 0..=u16::MAX {
        if order.contains(&id) {
            continue;
        }
        let name = efivar::efi::Variable::new(&format!("Boot{id:04X}"));
        if !vars
            .exists(&name)
            .map_err(|e| anyhow::anyhow!("failed to inspect Boot{id:04X}: {e}"))?
        {
            return Ok(id);
        }
    }
    bail!("no free Boot#### variable ID found")
}

fn result_status<T, E: std::fmt::Display>(result: &std::result::Result<T, E>) -> String {
    result
        .as_ref()
        .map_or_else(|error| format!("failed ({error})"), |_| "ok".to_string())
}

fn step_stage_esp_boot(r: &mut Runner) -> Result<()> {
    mount_efi(r)?;
    let source_entries = Path::new(TARGET_MOUNT).join("boot/loader/entries");
    let source_ostree = Path::new(TARGET_MOUNT).join("boot/ostree");
    if !source_entries.is_dir() {
        bail!(
            "boot loader entries not found: {}",
            source_entries.display()
        );
    }
    if !source_ostree.is_dir() {
        bail!("boot ostree files not found: {}", source_ostree.display());
    }

    let esp_entries = Path::new(TARGET_EFI_MOUNT).join("loader/entries");
    let esp_ostree = Path::new(TARGET_EFI_MOUNT).join("boot/ostree");
    if esp_entries.exists() {
        std::fs::remove_dir_all(&esp_entries)?;
    }
    std::fs::create_dir_all(&esp_entries)?;
    {
        let parent = esp_ostree
            .parent()
            .ok_or_else(|| anyhow::anyhow!("esp ostree path has no parent"))?;
        std::fs::create_dir_all(parent)?;
    }

    for entry in std::fs::read_dir(&source_entries)? {
        let entry = entry?;
        if entry.path().is_file() {
            std::fs::copy(entry.path(), esp_entries.join(entry.file_name()))?;
        }
    }
    pipeline::copy_tree_files(&source_ostree, &esp_ostree)?;
    crate::engine::util::sync_all();
    tracing::info!(
        target: crate::engine::INSTALLATION_LOG_TARGET,
        "staged boot loader entries and ostree kernel files on ESP"
    );
    Ok(())
}

fn step_secureboot(_r: &mut Runner) -> Result<()> {
    crate::engine::secureboot::ensure_sbctl_keys()?;
    let keydir = crate::engine::secureboot::sbctl_keydir();
    let owner = crate::engine::secureboot::sbctl_owner_guid();
    match crate::engine::secureboot::enroll_keys(&keydir, &owner) {
        Ok(()) => {
            tracing::info!(
                target: crate::engine::INSTALLATION_LOG_TARGET,
                "enrolled Secure Boot PK/KEK/db keys"
            );
            Ok(())
        }
        Err(e) => {
            // If the firmware already left Setup Mode, keys are likely enrolled.
            if matches!(crate::engine::firmware::detect_setup_mode(), SetupMode::Off) {
                tracing::info!(
                    target: crate::engine::INSTALLATION_LOG_TARGET,
                    "Secure Boot keys appear to be enrolled already"
                );
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

fn step_sign(_r: &mut Runner) -> Result<()> {
    crate::engine::secureboot::ensure_sbctl_keys()?;
    let efi_binaries = crate::engine::secureboot::target_efi_binaries();
    if efi_binaries.is_empty() {
        bail!("no EFI binaries found under {TARGET_EFI_MOUNT}");
    }
    let keydir = crate::engine::secureboot::sbctl_keydir();
    let db_key = std::fs::read_to_string(keydir.join("db/db.key"))?;
    let db_cert = std::fs::read_to_string(keydir.join("db/db.pem"))?;

    let mut signed_count = 0usize;
    let mut skipped_count = 0usize;
    for binary in &efi_binaries {
        let image = std::fs::read(binary)?;
        if crate::engine::secureboot::verify_pe(&image, &db_cert)
            .with_context(|| format!("failed to verify signature for {}", binary.display()))?
        {
            skipped_count += 1;
            continue;
        }
        let signed = crate::engine::secureboot::sign_pe(&image, &db_key, &db_cert)?;
        std::fs::write(binary, &signed)?;
        signed_count += 1;
    }
    tracing::info!(
        target: crate::engine::INSTALLATION_LOG_TARGET,
        "signed {signed_count} EFI binaries on target ESP ({skipped_count} already signed)"
    );

    for binary in &efi_binaries {
        let image = std::fs::read(binary)?;
        if !crate::engine::secureboot::verify_pe(&image, &db_cert)? {
            bail!("signature verification failed for {}", binary.display());
        }
    }
    Ok(())
}

fn step_final_cleanup(_r: &mut Runner) -> Result<()> {
    pipeline::cleanup_mounts()?;
    tracing::info!(
        target: crate::engine::INSTALLATION_LOG_TARGET,
        "target filesystems unmounted; ready to reboot"
    );
    Ok(())
}

pub static STEPS: &[Step] = &[
    Step {
        title: "Verify dependencies",
        fn_: step_dependencies,
        point_of_no_return: false,
        requires_secure_boot: false,
        weight: 1,
    },
    Step {
        title: "Verify Secure Boot setup",
        fn_: step_setup_mode,
        point_of_no_return: false,
        requires_secure_boot: true,
        weight: 1,
    },
    Step {
        title: "Clean up previous mounts",
        fn_: step_cleanup,
        point_of_no_return: false,
        requires_secure_boot: false,
        weight: 1,
    },
    Step {
        title: "Validate target drive",
        fn_: step_validate_drive,
        point_of_no_return: false,
        requires_secure_boot: false,
        weight: 1,
    },
    Step {
        title: "Verify image integrity",
        fn_: step_verify_image,
        point_of_no_return: false,
        requires_secure_boot: false,
        weight: 1,
    },
    Step {
        title: "Partition drive",
        fn_: step_partition,
        point_of_no_return: true,
        requires_secure_boot: false,
        weight: 1,
    },
    Step {
        title: "Format EFI partition",
        fn_: step_format_efi,
        point_of_no_return: false,
        requires_secure_boot: false,
        weight: 1,
    },
    Step {
        title: "Format root filesystem",
        fn_: step_format_root,
        point_of_no_return: false,
        requires_secure_boot: false,
        weight: 1,
    },
    Step {
        title: "Format home filesystem",
        fn_: step_format_home,
        point_of_no_return: false,
        requires_secure_boot: false,
        weight: 1,
    },
    Step {
        title: "Mount target filesystems",
        fn_: step_mount,
        point_of_no_return: false,
        requires_secure_boot: false,
        weight: 1,
    },
    Step {
        title: "Install bootc image",
        fn_: step_bootc_install,
        point_of_no_return: false,
        requires_secure_boot: false,
        weight: 30,
    },
    Step {
        title: "Mount home filesystem",
        fn_: step_mount_home,
        point_of_no_return: false,
        requires_secure_boot: false,
        weight: 1,
    },
    Step {
        title: "Configure persistent mounts",
        fn_: step_configure_mounts,
        point_of_no_return: false,
        requires_secure_boot: false,
        weight: 1,
    },
    Step {
        title: "Configure firstboot identity",
        fn_: step_firstboot,
        point_of_no_return: false,
        requires_secure_boot: false,
        weight: 1,
    },
    Step {
        title: "Configure k3s cluster",
        fn_: step_configure_k3s_cluster,
        point_of_no_return: false,
        requires_secure_boot: false,
        weight: 1,
    },
    Step {
        title: "Stage homed user identity",
        fn_: step_stage_homed_identity,
        point_of_no_return: false,
        requires_secure_boot: false,
        weight: 1,
    },
    Step {
        title: "Pre-stage Flatpak refs",
        fn_: step_flatpak_seed,
        point_of_no_return: false,
        requires_secure_boot: false,
        weight: 15,
    },
    Step {
        title: "Install systemd-boot",
        fn_: step_install_bootloader,
        point_of_no_return: false,
        requires_secure_boot: false,
        weight: 1,
    },
    Step {
        title: "Stage ESP boot files",
        fn_: step_stage_esp_boot,
        point_of_no_return: false,
        requires_secure_boot: false,
        weight: 1,
    },
    Step {
        title: "Sign EFI binaries",
        fn_: step_sign,
        point_of_no_return: false,
        requires_secure_boot: true,
        weight: 1,
    },
    Step {
        title: "Enroll Secure Boot keys",
        fn_: step_secureboot,
        point_of_no_return: false,
        requires_secure_boot: true,
        weight: 1,
    },
    Step {
        title: "Manage NVRAM boot entry",
        fn_: step_nvram,
        point_of_no_return: false,
        requires_secure_boot: false,
        weight: 1,
    },
    Step {
        title: "Unmount target filesystems",
        fn_: step_final_cleanup,
        point_of_no_return: false,
        requires_secure_boot: false,
        weight: 1,
    },
];
