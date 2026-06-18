// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use efivar::boot::{
    BootEntry, BootEntryAttributes, EFIHardDrive, EFIHardDriveType, FilePath, FilePathList,
};
use sha2::Digest;

use crate::pipeline::{
    self, BOOTC_IMAGE_STORAGE, BOOTC_RUNROOT, BOOTC_STORAGE_MOUNT, BOOTC_VAR_TMP,
    CONTAINERS_RUNROOT, DEPENDENCIES, Runner, Step, TARGET_EFI_MOUNT, TARGET_HOME_MOUNT,
    TARGET_MOUNT,
};

fn step_dependencies(r: &mut Runner) -> Result<()> {
    let missing: Vec<&str> = DEPENDENCIES
        .iter()
        .copied()
        .filter(|d| !crate::util::which_exists(d))
        .collect();
    if !missing.is_empty() {
        r.log("missing required tools:");
        for dep in &missing {
            r.log(&format!("  {dep}"));
        }
        bail!("missing dependencies: {}", missing.join(", "));
    }
    r.log(&format!("all {} dependencies present", DEPENDENCIES.len()));
    Ok(())
}

fn step_setup_mode(r: &mut Runner) -> Result<()> {
    if !r.ctx.secure_boot {
        r.log("Secure Boot disabled; skipping firmware setup check");
        return Ok(());
    }
    let status = crate::firmware::detect_setup_mode();
    if status.setup_mode.is_none() {
        r.ctx.secure_boot = false;
        r.log(
            "Secure Boot Setup Mode could not be detected; \
             continuing without Secure Boot",
        );
        if !status.reason.is_empty() {
            r.log(&format!("  reason: {}", status.reason));
        }
        return Ok(());
    }
    if status.setup_mode == Some(false) {
        bail!(
            "Secure Boot Setup Mode is off. Reboot to firmware setup or \
             continue without Secure Boot."
        );
    }
    let state = if status.secure_boot == Some(true) {
        "enabled"
    } else {
        "disabled"
    };
    r.log(&format!(
        "firmware reports Setup Mode on; Secure Boot is {state}"
    ));
    Ok(())
}

fn step_cleanup(r: &mut Runner) -> Result<()> {
    pipeline::cleanup_mounts(r)
}

fn step_validate_drive(r: &mut Runner) -> Result<()> {
    let drive = r.ctx.drive.clone();
    if !Path::new(&drive).exists() {
        bail!("'{drive}' is not a block device");
    }
    if let Some(ref src) = r.ctx.source_image {
        if !src.is_file() && !src.is_dir() {
            bail!("source bootc image not found at {}", src.display());
        }
    }

    match crate::block::disk_busy(&drive) {
        Some(crate::block::BusyReason::Mounted { ident, mountpoint }) => {
            bail!("'{drive}' is busy: {ident} mounted at {mountpoint}");
        }
        Some(crate::block::BusyReason::Mapping { kind, ident }) => {
            bail!("'{drive}' has active {kind} mapping {ident}");
        }
        None => {}
    }

    let size_b = pipeline::drive_size_bytes(&drive)?;
    let min = pipeline::min_target_size_bytes(&r.ctx.config);
    if size_b < min {
        bail!(
            "'{drive}' is too small: {} detected, need at least {} for EFI, \
             root and home partitions",
            crate::util::human_size(size_b),
            crate::util::human_size(min)
        );
    }

    r.ctx.parts = pipeline::derive_partitions(&drive);

    r.log(&format!(
        "target {drive} ({}; {}, {}, {})",
        crate::util::human_size(size_b),
        r.ctx.parts.efi_part,
        r.ctx.parts.root_part,
        r.ctx.parts.home_part
    ));
    Ok(())
}

fn step_partition(r: &mut Runner) -> Result<()> {
    let drive = r.ctx.drive.clone();
    let efi = r.ctx.config.partitions.efi.clone();
    let home = r.ctx.config.partitions.home.clone();
    r.ctx.parts.layout = Some(pipeline::partition_drive(r, &drive, &efi, &home)?);

    pipeline::wait_for_block_device(r, &r.ctx.parts.efi_part.clone())?;
    pipeline::wait_for_block_device(r, &r.ctx.parts.root_part.clone())?;
    pipeline::wait_for_block_device(r, &r.ctx.parts.home_part.clone())?;
    Ok(())
}

fn step_format_efi(r: &mut Runner) -> Result<()> {
    let part = r.ctx.parts.efi_part.clone();
    r.run_cmd(&["mkfs.fat", "-F32", "-n", "EFI", &part], None, None, None)?;
    r.ctx.parts.efi_uuid = pipeline::capture_partuuid(&part)?;
    Ok(())
}

fn step_format_root(r: &mut Runner) -> Result<()> {
    let part = r.ctx.parts.root_part.clone();
    r.run_cmd(
        &["mkfs.xfs", "-f", "-L", "puu-root", &part],
        None,
        None,
        None,
    )?;
    r.ctx.parts.root_uuid = pipeline::capture_uuid(&part)?;
    Ok(())
}

fn step_format_home(r: &mut Runner) -> Result<()> {
    let part = r.ctx.parts.home_part.clone();
    r.run_cmd(
        &[
            "mke2fs", "-t", "ext4", "-O", "encrypt", "-F", "-L", "puu-home", &part,
        ],
        None,
        None,
        None,
    )?;
    r.ctx.parts.home_uuid = pipeline::capture_uuid(&part)?;
    Ok(())
}

fn step_mount(r: &mut Runner) -> Result<()> {
    let root_part = r.ctx.parts.root_part.clone();
    pipeline::mount_target(r, &root_part, TARGET_MOUNT, "xfs", None)?;

    let efi_part = r.ctx.parts.efi_part.clone();
    pipeline::mount_target(
        r,
        &efi_part,
        TARGET_EFI_MOUNT,
        "vfat",
        Some("fmask=0077,dmask=0077"),
    )?;
    Ok(())
}

fn step_mount_home(r: &mut Runner) -> Result<()> {
    let home_part = r.ctx.parts.home_part.clone();
    pipeline::mount_target(r, &home_part, TARGET_HOME_MOUNT, "ext4", None)?;
    Ok(())
}

fn step_bootc_install(r: &mut Runner) -> Result<()> {
    let source_image = r.ctx.source_image.clone();
    let source_image = if source_image.is_none() {
        let default = pipeline::default_source_image(&r.ctx.config);
        r.ctx.source_image.clone_from(&default);
        if let Some(ref img) = default {
            r.ctx.target_imgref = pipeline::load_target_imgref(Some(img), &r.ctx.config);
            r.ctx.image_version = pipeline::load_image_version(Some(img));
            r.log(&format!(
                "using local bootc image source: {}",
                img.display()
            ));
        }
        default
    } else {
        source_image
    };

    if source_image.is_none() && !pipeline::running_in_container() {
        bail!(
            "no local bootc source image found; expected \
             /dev/disk/by-label/{}",
            r.ctx.config.image.source_label
        );
    }

    let mut mounted_bootc_tmp = false;
    let mut remove_bootc_storage = false;

    let result = (|| -> Result<()> {
        if !Path::new(BOOTC_STORAGE_MOUNT).exists() {
            std::fs::create_dir_all(BOOTC_STORAGE_MOUNT)?;
            remove_bootc_storage = true;
        }

        for dir in &[BOOTC_IMAGE_STORAGE, CONTAINERS_RUNROOT, BOOTC_RUNROOT] {
            std::fs::create_dir_all(dir)?;
        }

        mounted_bootc_tmp = pipeline::mount_bootc_vartmp(r)?;

        let storage_conf = PathBuf::from(BOOTC_STORAGE_MOUNT).join("storage.conf");
        std::fs::write(
            &storage_conf,
            format!(
                "[storage]\ndriver = \"overlay\"\ngraphroot = \"{BOOTC_IMAGE_STORAGE}\"\n\
                 runroot = \"{CONTAINERS_RUNROOT}\"\n"
            ),
        )?;

        let mut argv: Vec<String> = vec![
            "bootc".into(),
            "install".into(),
            "to-filesystem".into(),
            "--target-imgref".into(),
            r.ctx.target_imgref.clone(),
            "--skip-fetch-check".into(),
            "--bootloader".into(),
            "none".into(),
            "--skip-finalize".into(),
        ];

        for arg in pipeline::install_kernel_args(&r.ctx.parts.root_uuid, &r.ctx.config) {
            argv.push("--karg".into());
            argv.push(arg);
        }

        argv.push(TARGET_MOUNT.into());

        if let Some(ref src) = source_image {
            let idx = argv
                .iter()
                .position(|a| a == "to-filesystem")
                .ok_or_else(|| anyhow::anyhow!("bootc argv missing to-filesystem"))?
                + 1;
            argv.insert(idx, pipeline::image_source_ref(src));
            argv.insert(idx, "--source-imgref".into());
        }

        r.log("bootc image deployment may take several minutes without native progress output");
        r.progress(0.05, "Install bootc image");

        let env = vec![
            (
                "CONTAINERS_STORAGE_CONF".into(),
                storage_conf.to_string_lossy().to_string(),
            ),
            ("TMPDIR".into(), BOOTC_VAR_TMP.into()),
        ];
        let args_ref: Vec<&str> = argv.iter().map(std::string::String::as_str).collect();
        r.run_cmd(
            &args_ref,
            Some(&env),
            Some(Duration::from_secs(30)),
            Some("bootc install still running; deploying image"),
        )?;

        Ok(())
    })();

    if mounted_bootc_tmp {
        let _ = pipeline::unmount(BOOTC_VAR_TMP);
    }
    if remove_bootc_storage {
        let _ = std::fs::remove_dir_all(BOOTC_STORAGE_MOUNT);
    }

    result?;
    r.progress(0.95, "Install bootc image");
    Ok(())
}

fn step_configure_mounts(r: &mut Runner) -> Result<()> {
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
    let new = pipeline::merge_fstab(&old, &r.ctx.parts.efi_uuid, &r.ctx.parts.home_uuid);
    std::fs::write(&fstab, &new)?;
    std::fs::set_permissions(&fstab, PermissionsExt::from_mode(0o644))?;
    Ok(())
}

fn step_verify_image(r: &mut Runner) -> Result<()> {
    let Some(src) = r.ctx.source_image.clone() else {
        r.log("using running bootc image as install source");
        return Ok(());
    };
    let manifest = pipeline::image_manifest_metadata(&src);
    if manifest.is_null() {
        r.log("no image manifest found, skipping digest verification");
        return Ok(());
    }
    let expected = manifest
        .get("digest")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if expected.is_empty() {
        r.log("manifest has no digest entry, skipping");
        return Ok(());
    }
    r.log(&format!("expected image digest: {expected}"));

    let raw = if src.is_dir() {
        // A skopeo `dir:` layout stores the raw manifest as `manifest.json`;
        // `skopeo inspect --raw` just emits that file, so hash it directly.
        let manifest_path = src.join("manifest.json");
        std::fs::read(&manifest_path).map_err(|e| {
            anyhow::anyhow!(
                "failed to read image manifest {}: {e}",
                manifest_path.display()
            )
        })?
    } else {
        let file = std::fs::File::open(&src)?;
        let mut archive = tar::Archive::new(file);
        let mut raw = None;
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?;
            if path.as_ref() == Path::new("manifest.json") {
                let mut data = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut data)?;
                raw = Some(data);
                break;
            }
        }
        raw.ok_or_else(|| anyhow::anyhow!("image archive {} has no manifest.json", src.display()))?
    };

    let actual = format!("sha256:{}", hex::encode(sha2::Sha256::digest(&raw)));
    if actual != expected {
        bail!("image digest mismatch: expected {expected}, got {actual}");
    }
    r.log(&format!("image digest verified: {actual}"));
    Ok(())
}

fn step_firstboot(r: &mut Runner) -> Result<()> {
    let ident = &r.ctx.config.identity;
    let keymap = if r.ctx.keymap.is_empty() {
        ident.keymap.clone()
    } else {
        r.ctx.keymap.clone()
    };
    let locale = if r.ctx.locale.is_empty() {
        ident.locale.clone()
    } else {
        r.ctx.locale.clone()
    };

    let timezone = ident.timezone.clone();
    let hostname = ident.hostname.clone();

    // Equivalent of `systemd-firstboot --root`: write the config files directly,
    // and like firstboot, never overwrite values already present in the image.
    let etc = Path::new(TARGET_MOUNT).join("etc");
    std::fs::create_dir_all(&etc)?;
    let write_if_absent = |path: PathBuf, contents: String| -> Result<()> {
        if !path.exists() {
            std::fs::write(&path, contents)?;
            std::fs::set_permissions(&path, PermissionsExt::from_mode(0o644))?;
        }
        Ok(())
    };
    if !hostname.is_empty() {
        write_if_absent(etc.join("hostname"), format!("{hostname}\n"))?;
    }
    if !locale.is_empty() {
        write_if_absent(etc.join("locale.conf"), format!("LANG={locale}\n"))?;
    }
    if !timezone.is_empty() {
        let link = etc.join("localtime");
        if !link.exists() {
            std::os::unix::fs::symlink(format!("../usr/share/zoneinfo/{timezone}"), &link)?;
        }
    }

    if !keymap.is_empty() {
        let vconsole = Path::new(TARGET_MOUNT).join("etc/vconsole.conf");
        {
            let parent = vconsole
                .parent()
                .ok_or_else(|| anyhow::anyhow!("vconsole path has no parent"))?;
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&vconsole, format!("KEYMAP={keymap}\n"))?;
        std::fs::set_permissions(&vconsole, PermissionsExt::from_mode(0o644))?;
        r.log(&format!("staged keymap {keymap} in /etc/vconsole.conf"));
    }
    Ok(())
}

fn step_configure_k3s_cluster(r: &mut Runner) -> Result<()> {
    if r.ctx.config.k3s.role == "server" && r.ctx.config.k3s.node_name.is_empty() {
        r.log("k3s cluster: using image default server configuration");
        return Ok(());
    }

    let mut argv: Vec<String> = vec![
        r.ctx.config.k3s.helper.clone(),
        "--role".into(),
        r.ctx.config.k3s.role.clone(),
    ];
    if !r.ctx.config.k3s.server_url.is_empty() {
        argv.push("--server-url".into());
        argv.push(r.ctx.config.k3s.server_url.clone());
    }
    if !r.ctx.k3s_token.is_empty() {
        let token_path = Path::new(TARGET_MOUNT).join("etc/rancher/k3s/puu-token");
        {
            let parent = token_path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("token path has no parent"))?;
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&token_path, format!("{}\n", r.ctx.k3s_token))?;
        std::fs::set_permissions(&token_path, PermissionsExt::from_mode(0o600))?;
        argv.push("--token-file".into());
        argv.push("/etc/rancher/k3s/puu-token".into());
    }
    if !r.ctx.config.k3s.node_name.is_empty() {
        argv.push("--node-name".into());
        argv.push(r.ctx.config.k3s.node_name.clone());
    }
    let args_ref: Vec<&str> = argv.iter().map(std::string::String::as_str).collect();
    r.run_cmd_chroot(&args_ref, TARGET_MOUNT, None, None)?;
    r.log(&format!(
        "k3s cluster: staged {} configuration",
        r.ctx.config.k3s.role
    ));
    Ok(())
}

fn step_stage_homed_identity(r: &mut Runner) -> Result<()> {
    if r.ctx.username.is_empty() {
        bail!("username is required to stage a homed identity");
    }
    let name = &r.ctx.username;
    if r.ctx.user_password.contains('\n') || r.ctx.user_password.contains('\r') {
        bail!("password must not contain newline characters");
    }

    let hashed = pipeline::hash_password_sha512(&r.ctx.user_password)?;

    let identity = serde_json::json!({
        "userName": name,
        "secret": { "password": [&r.ctx.user_password] },
        "realName": name,
        "shell": r.ctx.user_shell,
        "memberOf": r.ctx.config.homed.groups.as_slice(),
        "homeDirectory": format!("/var/home/{name}"),
        "storage": "directory",
        "preferredSessionLauncher": "gnome",
        "privileged": {
            "hashedPassword": [&hashed],
            "password": [&r.ctx.user_password],
        },
    });

    let target = Path::new(TARGET_MOUNT).join("var/lib/puu/identity.json");
    {
        let parent = target
            .parent()
            .ok_or_else(|| anyhow::anyhow!("identity target path has no parent"))?;
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, serde_json::to_string_pretty(&identity)?)?;
    std::fs::set_permissions(&target, PermissionsExt::from_mode(0o600))?;
    r.log(&format!(
        "staged homed identity for {name} at /var/lib/puu/identity.json"
    ));
    Ok(())
}

fn step_flatpak_seed(r: &mut Runner) -> Result<()> {
    let manifest =
        Path::new(TARGET_MOUNT).join(r.ctx.config.flatpak.manifest_path.trim_start_matches('/'));
    let refs = pipeline::flatpak_manifest_refs(&manifest);
    if refs.is_empty() {
        r.log("flatpak-seed: no Flatpak refs in manifest");
        return Ok(());
    }

    let sync =
        Path::new(TARGET_MOUNT).join(r.ctx.config.flatpak.sync_helper.trim_start_matches('/'));
    if !sync.is_file() {
        r.log("flatpak-seed: flatpak-sync helper missing; skipping");
        return Ok(());
    }

    r.log(&format!(
        "flatpak-seed: installing {} system Flatpak refs",
        refs.len()
    ));
    let mounted = pipeline::mount_chroot_binds(r)?;
    let result = r.run_cmd_chroot(
        &[&r.ctx.config.flatpak.sync_helper.clone()],
        TARGET_MOUNT,
        Some(Duration::from_secs(30)),
        Some("flatpak pre-seed still running; downloading apps"),
    );
    pipeline::unmount_chroot_binds(r, &mounted)?;
    result?;
    r.log(
        "flatpak-seed: system Flatpaks installed; system flatpak-sync.timer \
         will keep them reconciled",
    );
    Ok(())
}

fn step_nvram(r: &mut Runner) -> Result<()> {
    let source = systemd_boot_source()?;
    let arch = efi_arch_suffix()?;
    let systemd_dir = Path::new(TARGET_EFI_MOUNT).join("EFI/systemd");
    let fallback_dir = Path::new(TARGET_EFI_MOUNT).join("EFI/BOOT");
    std::fs::create_dir_all(&systemd_dir)?;
    std::fs::create_dir_all(&fallback_dir)?;

    let systemd_target = systemd_dir.join(format!("systemd-boot{arch}.efi"));
    let fallback_target = fallback_dir.join(format!("BOOT{arch}.EFI"));
    std::fs::copy(&source, &systemd_target)?;
    std::fs::copy(&source, &fallback_target)?;

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
        description: "puu".into(),
        file_path_list: Some(file_path_list),
        optional_data: Vec::new(),
    };

    let mut vars = efivar::system();
    let old_order = vars.get_boot_order().unwrap_or_default();
    let boot_id = first_free_boot_id(&old_order, vars.as_ref())?;
    vars.add_boot_entry(boot_id, entry)
        .map_err(|e| anyhow::anyhow!("failed to write Boot{boot_id:04X}: {e}"))?;

    let mut new_order = vec![boot_id];
    new_order.extend(old_order.into_iter().filter(|id| *id != boot_id));
    vars.set_boot_order(new_order)
        .map_err(|e| anyhow::anyhow!("failed to update BootOrder: {e}"))?;
    r.log(&format!(
        "installed systemd-boot and registered Boot{boot_id:04X}"
    ));
    Ok(())
}

fn systemd_boot_source() -> Result<PathBuf> {
    let dir = Path::new(TARGET_MOUNT).join("usr/lib/systemd/boot/efi");
    for entry in std::fs::read_dir(&dir)? {
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
    bail!("systemd-boot EFI binary not found in {}", dir.display())
}

fn efi_arch_suffix() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x64"),
        "aarch64" => Ok("aa64"),
        "arm" => Ok("arm"),
        arch => bail!("unsupported EFI architecture: {arch}"),
    }
}

fn first_free_boot_id(order: &[u16], vars: &dyn efivar::VarManager) -> Result<u16> {
    for id in 0..=u16::MAX {
        if order.contains(&id) {
            continue;
        }
        let name = efivar::efi::Variable::new(&format!("Boot{id:04X}"));
        if vars.read(&name).is_err() {
            return Ok(id);
        }
    }
    bail!("no free Boot#### variable ID found")
}

fn step_stage_esp_boot(r: &mut Runner) -> Result<()> {
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
    unsafe {
        libc::sync();
    }
    r.log("staged boot loader entries and ostree kernel files on ESP");
    Ok(())
}

fn step_secureboot(r: &mut Runner) -> Result<()> {
    pipeline::ensure_sbctl_keys(r)?;
    let keydir = pipeline::sbctl_keydir();
    let owner = pipeline::sbctl_owner_guid();
    match crate::secureboot::enroll_keys(&keydir, &owner) {
        Ok(()) => {
            r.log("enrolled Secure Boot PK/KEK/db keys");
            Ok(())
        }
        Err(e) => {
            // If the firmware already left Setup Mode, keys are likely enrolled.
            if crate::firmware::detect_setup_mode().setup_mode == Some(false) {
                r.log("Secure Boot keys appear to be enrolled already");
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

fn step_sign(r: &mut Runner) -> Result<()> {
    pipeline::ensure_sbctl_keys(r)?;
    let efi_binaries = pipeline::target_efi_binaries();
    if efi_binaries.is_empty() {
        bail!("no EFI binaries found under {TARGET_EFI_MOUNT}");
    }
    let keydir = pipeline::sbctl_keydir();
    let db_key = std::fs::read_to_string(keydir.join("db/db.key"))?;
    let db_cert = std::fs::read_to_string(keydir.join("db/db.pem"))?;

    for binary in &efi_binaries {
        let image = std::fs::read(binary)?;
        if crate::secureboot::verify_pe(&image, &db_cert).unwrap_or(false) {
            continue;
        }
        let signed = crate::secureboot::sign_pe(&image, &db_key, &db_cert)?;
        std::fs::write(binary, &signed)?;
    }
    r.log(&format!(
        "signed {} EFI binaries on target ESP",
        efi_binaries.len()
    ));

    for binary in &efi_binaries {
        let image = std::fs::read(binary)?;
        if !crate::secureboot::verify_pe(&image, &db_cert)? {
            bail!("signature verification failed for {}", binary.display());
        }
    }
    Ok(())
}

fn step_final_cleanup(r: &mut Runner) -> Result<()> {
    pipeline::cleanup_mounts(r)?;
    r.log("target filesystems unmounted; ready to reboot");
    Ok(())
}

pub static STEPS: std::sync::LazyLock<Vec<Step>> = std::sync::LazyLock::new(|| {
    vec![
        Step {
            title: "Verify dependencies",
            fn_: step_dependencies,
            point_of_no_return: false,
            requires_secure_boot: false,
        },
        Step {
            title: "Verify Secure Boot setup",
            fn_: step_setup_mode,
            point_of_no_return: false,
            requires_secure_boot: true,
        },
        Step {
            title: "Clean up previous mounts",
            fn_: step_cleanup,
            point_of_no_return: false,
            requires_secure_boot: false,
        },
        Step {
            title: "Validate target drive",
            fn_: step_validate_drive,
            point_of_no_return: false,
            requires_secure_boot: false,
        },
        Step {
            title: "Verify image integrity",
            fn_: step_verify_image,
            point_of_no_return: false,
            requires_secure_boot: false,
        },
        Step {
            title: "Partition drive",
            fn_: step_partition,
            point_of_no_return: true,
            requires_secure_boot: false,
        },
        Step {
            title: "Format EFI partition",
            fn_: step_format_efi,
            point_of_no_return: false,
            requires_secure_boot: false,
        },
        Step {
            title: "Format root filesystem",
            fn_: step_format_root,
            point_of_no_return: false,
            requires_secure_boot: false,
        },
        Step {
            title: "Format home filesystem",
            fn_: step_format_home,
            point_of_no_return: false,
            requires_secure_boot: false,
        },
        Step {
            title: "Mount target filesystems",
            fn_: step_mount,
            point_of_no_return: false,
            requires_secure_boot: false,
        },
        Step {
            title: "Install bootc image",
            fn_: step_bootc_install,
            point_of_no_return: false,
            requires_secure_boot: false,
        },
        Step {
            title: "Mount home filesystem",
            fn_: step_mount_home,
            point_of_no_return: false,
            requires_secure_boot: false,
        },
        Step {
            title: "Configure persistent mounts",
            fn_: step_configure_mounts,
            point_of_no_return: false,
            requires_secure_boot: false,
        },
        Step {
            title: "Configure firstboot identity",
            fn_: step_firstboot,
            point_of_no_return: false,
            requires_secure_boot: false,
        },
        Step {
            title: "Configure k3s cluster",
            fn_: step_configure_k3s_cluster,
            point_of_no_return: false,
            requires_secure_boot: false,
        },
        Step {
            title: "Stage homed user identity",
            fn_: step_stage_homed_identity,
            point_of_no_return: false,
            requires_secure_boot: false,
        },
        Step {
            title: "Pre-stage Flatpak refs",
            fn_: step_flatpak_seed,
            point_of_no_return: false,
            requires_secure_boot: false,
        },
        Step {
            title: "Manage NVRAM boot entry",
            fn_: step_nvram,
            point_of_no_return: false,
            requires_secure_boot: false,
        },
        Step {
            title: "Stage ESP boot files",
            fn_: step_stage_esp_boot,
            point_of_no_return: false,
            requires_secure_boot: false,
        },
        Step {
            title: "Sign EFI binaries",
            fn_: step_sign,
            point_of_no_return: false,
            requires_secure_boot: true,
        },
        Step {
            title: "Enroll Secure Boot keys",
            fn_: step_secureboot,
            point_of_no_return: false,
            requires_secure_boot: true,
        },
        Step {
            title: "Unmount target filesystems",
            fn_: step_final_cleanup,
            point_of_no_return: false,
            requires_secure_boot: false,
        },
    ]
});
