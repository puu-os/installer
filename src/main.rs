// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#![deny(clippy::all)]
#![deny(clippy::pedantic)]
// ratatui Rect::new takes u16; TUI coordinates are always small
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

use crate::engine::config::Config;
use crate::engine::pipeline::Context;
use crate::tui::app::Installer;

/// Puu system installer.
#[derive(FromArgs)]
struct Args {
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
    let args: Args = argh::from_env();

    if !engine::util::running_as_root() {
        eprintln!("puu-installer must be run as root.");
        std::process::exit(1);
    }

    let config_path = std::env::var_os("PUU_INSTALLER_CONFIG").map_or_else(
        || PathBuf::from("/usr/share/puu/machine.yaml"),
        PathBuf::from,
    );
    let mut cfg = Config::load(&config_path)?;

    if let Some(role) = args.k3s_role {
        if role != "server" && role != "agent" {
            eprintln!("--k3s-role must be \"server\" or \"agent\", got \"{role}\".");
            std::process::exit(1);
        }
        cfg.k3s.role = role;
    }

    let source_image = match args.source_image {
        Some(p) => Some(p),
        None => engine::image::default_source_image(&cfg),
    };

    if source_image.is_none() && !engine::image::running_in_container() {
        eprintln!(
            "no local bootc source image found; expected \
             /dev/disk/by-label/{} or --source-image.",
            cfg.image.source_label
        );
        std::process::exit(1);
    }

    let k3s_token = match &args.k3s_token_file {
        Some(path) => read_first_line(path)?,
        None => String::new(),
    };

    if cfg.k3s.role == "agent" && (cfg.k3s.server_url.is_empty() || k3s_token.is_empty()) {
        eprintln!("--k3s-role agent requires k3s.serverUrl and --k3s-token-file.");
        std::process::exit(1);
    }

    let target_imgref = engine::image::load_target_imgref(source_image.as_deref(), &cfg);
    let image_version = engine::image::load_image_version(source_image.as_deref());

    let ctx = Context {
        config: cfg,
        source_image,
        target_imgref,
        image_version,
        k3s_token,
        ..Default::default()
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
