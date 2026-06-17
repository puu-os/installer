// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use sha2::Digest;

use crate::pipeline::{
    self, BOOTC_IMAGE_STORAGE, BOOTC_RUNROOT, BOOTC_STORAGE_MOUNT, BOOTC_VAR_TMP,
    CONTAINERS_RUNROOT, DEPENDENCIES, Runner, Step, TARGET_EFI_MOUNT, TARGET_HOME_MOUNT,
    TARGET_MOUNT,
};

fn step_dependencies(r: &mut Runner) -> Result<()> {
    let deps: Vec<&str> = if r.ctx.secure_boot {
        DEPENDENCIES.to_vec()
    } else {
        DEPENDENCIES
            .iter()
            .filter(|d| **d != "sbctl")
            .copied()
            .collect()
    };
    let missing: Vec<&str> = deps
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
    r.log(&format!("all {} dependencies present", deps.len()));
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

    let out = Runner::capture_cmd(&["lsblk", "-nrpo", "NAME,TYPE,MOUNTPOINT", &drive], None)?;
    for line in out.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 && !parts[2].is_empty() {
            bail!("'{drive}' is busy: {} mounted at {}", parts[0], parts[2]);
        }
        if parts.len() >= 2 && matches!(parts[1], "crypt" | "raid" | "lvm") {
            bail!("'{drive}' has active {} mapping {}", parts[1], parts[0]);
        }
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
    r.run_cmd(&["sgdisk", "-Z", &drive], None, None, None)?;
    r.run_cmd(
        &[
            "sgdisk",
            &format!("-n1:0:+{}", r.ctx.config.partitions.efi),
            "-t1:ef00",
            "-c1:EFI",
            &drive,
        ],
        None,
        None,
        None,
    )?;
    r.run_cmd(
        &[
            "sgdisk",
            &format!(
                "-n3:-{}:0",
                r.ctx.config.partitions.home.trim_start_matches('+')
            ),
            "-t3:8300",
            "-c3:home",
            &drive,
        ],
        None,
        None,
        None,
    )?;
    r.run_cmd(
        &["sgdisk", "-n2:0:0", "-t2:8300", "-c2:root", &drive],
        None,
        None,
        None,
    )?;
    r.run_cmd(&["partprobe", &drive], None, None, None)?;
    r.run_cmd(&["udevadm", "settle"], None, None, None)?;

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
    pipeline::mount_if_needed(
        r,
        TARGET_MOUNT,
        &["mount", "-t", "xfs", &root_part, TARGET_MOUNT],
    )?;

    let efi_part = r.ctx.parts.efi_part.clone();
    pipeline::mount_if_needed(
        r,
        TARGET_EFI_MOUNT,
        &[
            "mount",
            "-o",
            "fmask=0077,dmask=0077",
            &efi_part,
            TARGET_EFI_MOUNT,
        ],
    )?;
    Ok(())
}

fn step_mount_home(r: &mut Runner) -> Result<()> {
    let home_part = r.ctx.parts.home_part.clone();
    pipeline::mount_if_needed(
        r,
        TARGET_HOME_MOUNT,
        &["mount", "-t", "ext4", &home_part, TARGET_HOME_MOUNT],
    )?;
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
        let _ = r.run_cmd(&["umount", BOOTC_VAR_TMP], None, None, None);
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
    if r.ctx.source_image.is_none() {
        r.log("using running bootc image as install source");
        return Ok(());
    }
    let src = r
        .ctx
        .source_image
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("source image is None"))?
        .clone();
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

    if src.is_dir() {
        let raw = Runner::capture_cmd(
            &[
                "skopeo",
                "inspect",
                "--raw",
                &pipeline::image_source_ref(&src),
            ],
            None,
        )?;
        let actual = format!(
            "sha256:{}",
            hex::encode(sha2::Sha256::digest(raw.as_bytes()))
        );
        if actual != expected {
            bail!("image digest mismatch: expected {expected}, got {actual}");
        }
        r.log(&format!("image digest verified: {actual}"));
    } else {
        r.log("archive digest verification: skipping (not yet implemented)");
    }
    Ok(())
}

fn step_firstboot(r: &mut Runner) -> Result<()> {
    let ident = pipeline::parse_identity(&r.ctx.config.identity.config_path);
    let keymap = if r.ctx.keymap.is_empty() {
        ident.get("KEYMAP").cloned().unwrap_or_else(|| "us".into())
    } else {
        r.ctx.keymap.clone()
    };
    let locale = if r.ctx.locale.is_empty() {
        ident
            .get("LOCALE")
            .cloned()
            .unwrap_or_else(|| "C.UTF-8".into())
    } else {
        r.ctx.locale.clone()
    };

    let timezone = ident
        .get("TIMEZONE")
        .cloned()
        .unwrap_or_else(|| "UTC".into());
    let hostname = ident
        .get("HOSTNAME")
        .cloned()
        .unwrap_or_else(|| "puu".into());

    r.run_cmd(
        &[
            "systemd-firstboot",
            "--root",
            TARGET_MOUNT,
            &format!("--timezone={timezone}"),
            &format!("--hostname={hostname}"),
            &format!("--locale={locale}"),
        ],
        None,
        None,
        None,
    )?;

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
        "chroot".into(),
        TARGET_MOUNT.into(),
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
    r.run_cmd(&args_ref, None, None, None)?;
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

    let hashed = crate::subproc::capture(
        &[
            "openssl".into(),
            "passwd".into(),
            "-6".into(),
            "-stdin".into(),
        ],
        Some(r.ctx.user_password.as_bytes()),
        None,
    )?
    .trim()
    .to_string();

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
    let result = r.run_cmd(
        &[
            "chroot",
            TARGET_MOUNT,
            &r.ctx.config.flatpak.sync_helper.clone(),
        ],
        None,
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
    let esp_arg = format!("--esp-path={TARGET_EFI_MOUNT}");
    let result = r.run_cmd(
        &["bootctl", &esp_arg, "install", "--variables=yes"],
        None,
        None,
        None,
    );
    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            r.log(&format!("bootctl install failed; retrying as update: {e}"));
            r.run_cmd(&["bootctl", &esp_arg, "update"], None, None, None)
        }
    }
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
    let argv = pipeline::target_sbctl_argv(&["enroll-keys", "--yes-this-might-brick-my-machine"])?;
    let args_ref: Vec<&str> = argv.iter().map(std::string::String::as_str).collect();
    match r.run_cmd(&args_ref, None, None, None) {
        Ok(()) => Ok(()),
        Err(e) => {
            r.log(&format!(
                "sbctl enroll-keys failed; checking current status: {e}"
            ));
            let status_argv = pipeline::target_sbctl_argv(&["status"])?;
            let status_cmd_args: Vec<&str> = status_argv
                .iter()
                .map(std::string::String::as_str)
                .collect();
            let status = Runner::capture_cmd(&status_cmd_args, None)?;
            if status.contains("Setup Mode") && status.contains("Disabled") {
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
    for binary in &efi_binaries {
        let argv = pipeline::target_sbctl_argv(&["sign", &binary.to_string_lossy()])?;
        let args_ref: Vec<&str> = argv.iter().map(std::string::String::as_str).collect();
        r.run_cmd(&args_ref, None, None, None)?;
    }
    r.log(&format!(
        "signed {} EFI binaries on target ESP",
        efi_binaries.len()
    ));
    let verify_argv = pipeline::target_sbctl_argv(&["verify"])?;
    let verify_cmd_args: Vec<&str> = verify_argv
        .iter()
        .map(std::string::String::as_str)
        .collect();
    let env = pipeline::target_sbctl_env();
    r.run_cmd(&verify_cmd_args, Some(&env), None, None)?;
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
