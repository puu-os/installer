// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#![deny(clippy::all)]
#![deny(clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_precision_loss)]

mod engine;
mod tui;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result, bail};
use argh::FromArgs;

use crate::engine::config::{Config, K3sRole};
use crate::engine::pipeline::Context;
use crate::tui::app::Installer;

/// Puu system installer.
#[derive(FromArgs)]
struct Args {
    /// read installer configuration from this YAML file
    #[argh(option)]
    config_path: Option<PathBuf>,
    /// override install source with a bootc image dir layout or oci-archive path
    #[argh(option)]
    source_image: Option<PathBuf>,

    /// override k3s cluster role from config ("server" or "agent")
    #[argh(option)]
    k3s_role: Option<String>,

    /// read the k3s cluster token from this file
    #[argh(option)]
    k3s_token_file: Option<PathBuf>,
}

fn read_first_line(path: &std::path::Path) -> Result<String> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    Ok(content.lines().next().unwrap_or("").to_string())
}

fn main() -> Result<()> {
    tui::logging::init()?;
    tracing::info!(target: engine::INSTALLATION_LOG_TARGET, "installer started");

    let args: Args = argh::from_env();

    if !engine::util::running_as_root() {
        bail!("puu-installer must be run as root.");
    }

    engine::pipeline::enter_private_mount_namespace()?;

    let Some(config_path) = args
        .config_path
        .or_else(|| std::env::var_os("PUU_INSTALLER_CONFIG").map(PathBuf::from))
    else {
        bail!(
            "configuration file path must be provided via --config-path or PUU_INSTALLER_CONFIG environment variable."
        );
    };
    let mut cfg = Config::load(&config_path)?;

    if let Some(role_str) = args.k3s_role {
        let Ok(role) = role_str.parse::<K3sRole>() else {
            bail!("--k3s-role must be \"server\" or \"agent\", got \"{role_str}\".");
        };
        cfg.k3s.role = role;
    }

    let source_image = match args.source_image {
        Some(p) => Some(p),
        None => engine::image::default_source_image(&cfg),
    };

    if source_image.is_none() && !engine::image::running_in_container() {
        bail!(
            "no local bootc source image found; expected \
             /dev/disk/by-label/{} or --source-image.",
            cfg.image.source_label
        );
    }

    let k3s_token = match &args.k3s_token_file {
        Some(path) => read_first_line(path)?,
        None => String::new(),
    };

    if cfg.k3s.role == K3sRole::Agent && (cfg.k3s.server_url.is_empty() || k3s_token.is_empty()) {
        bail!("--k3s-role agent requires k3s.serverUrl and --k3s-token-file.");
    }

    let target_imgref = engine::image::load_target_imgref(source_image.as_deref(), &cfg);
    let image_version = engine::image::load_image_version(source_image.as_deref());

    let ctx = Context {
        config: cfg,
        source_image,
        target_imgref,
        image_version,
        k3s_token: k3s_token.into(),
        ..Context::default()
    };

    let mut app = Installer::new(ctx);
    loop {
        match app.run() {
            tui::app::Trap::Quit => return Ok(()),
            tui::app::Trap::Reboot => {
                engine::util::sync_all();
                let _ = nix::sys::reboot::reboot(nix::sys::reboot::RebootMode::RB_AUTOBOOT);
                bail!("reboot syscall failed");
            }
            tui::app::Trap::Shell => {
                let shell = engine::util::default_shell();
                let _ = Command::new(&shell).status();
                app = Installer::new(app.into_context());
            }
        }
    }
}
