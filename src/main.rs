// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#![deny(clippy::all)]
#![deny(clippy::pedantic)]
// ratatui Rect::new takes u16; TUI coordinates are always small
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_precision_loss)]

mod app;
mod block;
mod config;
mod firmware;
mod pipeline;
mod screens;
mod steps;
mod subproc;
mod util;

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result, bail};
use argh::FromArgs;

use crate::app::Installer;
use crate::config::Config;
use crate::pipeline::Context;

/// Puu system installer.
#[derive(FromArgs)]
struct Args {
    /// path to the YAML configuration file (overrides `PUU_INSTALLER_CONFIG`;
    /// defaults to /usr/share/puu/machine.yaml)
    #[argh(option)]
    config: Option<PathBuf>,

    /// pre-select the target drive (e.g. /dev/nvme0n1)
    #[argh(option)]
    drive: Option<String>,

    /// override install source with a bootc image dir layout or oci-archive path
    #[argh(option)]
    source_image: Option<PathBuf>,

    /// resume the pipeline at step N (1-indexed)
    #[argh(option, default = "1")]
    start_from: usize,

    /// skip step numbers (comma-separated, 1-indexed)
    #[argh(option)]
    skip: Option<String>,

    /// read the k3s cluster token from this file
    #[argh(option)]
    k3s_token_file: Option<PathBuf>,
}

fn parse_step_list(value: &str, step_count: usize) -> Result<Vec<usize>> {
    let mut steps = Vec::new();
    for item in value.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let n: usize = item.parse().with_context(|| {
            format!("--skip expects comma-separated step numbers, got '{value}'")
        })?;
        if n < 1 || n > step_count {
            bail!("--skip values must be between 1 and {step_count}: {n}");
        }
        steps.push(n);
    }
    Ok(steps)
}

fn read_first_line(path: &std::path::Path) -> Result<String> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    Ok(content.lines().next().unwrap_or("").to_string())
}

fn main() -> Result<()> {
    let args: Args = argh::from_env();

    if unsafe { libc::geteuid() } != 0 {
        eprintln!("puu-installer must be run as root.");
        std::process::exit(1);
    }

    let config_path = args
        .config
        .or_else(|| std::env::var_os("PUU_INSTALLER_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/usr/share/puu/machine.yaml"));

    let cfg = Config::load(&config_path)?;

    let source_image = match args.source_image {
        Some(p) => Some(p),
        None => pipeline::default_source_image(&cfg),
    };

    if source_image.is_none() && !pipeline::running_in_container() {
        eprintln!(
            "no local bootc source image found; expected \
             /dev/disk/by-label/{} or --source-image.",
            cfg.image.source_label
        );
        std::process::exit(1);
    }

    let step_count = pipeline::STEPS.len();
    if args.start_from < 1 || args.start_from > step_count {
        eprintln!("--start-from must be between 1 and {step_count}");
        std::process::exit(2);
    }

    let skip_steps: Vec<usize> = match &args.skip {
        Some(s) => parse_step_list(s, step_count)?,
        None => Vec::new(),
    };

    let k3s_token = match &args.k3s_token_file {
        Some(path) => read_first_line(path)?,
        None => String::new(),
    };

    if cfg.k3s.role != "server" && cfg.k3s.role != "agent" {
        eprintln!(
            "k3s.role must be \"server\" or \"agent\", got \"{}\".",
            cfg.k3s.role
        );
        std::process::exit(1);
    }

    if cfg.k3s.role == "agent" && (cfg.k3s.server_url.is_empty() || k3s_token.is_empty()) {
        eprintln!("k3s.role agent requires k3s.serverUrl and --k3s-token-file.");
        std::process::exit(1);
    }

    let target_imgref = pipeline::load_target_imgref(source_image.as_deref(), &cfg);
    let image_version = pipeline::load_image_version(source_image.as_deref());

    let ctx = Context {
        config: cfg,
        source_image,
        target_imgref,
        image_version,
        drive: args.drive.unwrap_or_default(),
        drive_label: String::new(),
        username: String::new(),
        user_password: String::new(),
        user_shell: "/bin/bash".to_string(),
        keymap: String::new(),
        locale: String::new(),
        secure_boot: true,
        k3s_token,
        last_error: String::new(),
        start_from: args.start_from,
        skip_steps,
        parts: pipeline::DerivedPartitions::default(),
    };

    let mut app = Installer::new(ctx);
    loop {
        match app.run() {
            app::Trap::Quit => return Ok(()),
            app::Trap::Reboot => {
                unsafe { libc::sync() };
                let _ = nix::sys::reboot::reboot(nix::sys::reboot::RebootMode::RB_AUTOBOOT);
                bail!("reboot syscall failed");
            }
            app::Trap::Shell => {
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
                let _ = Command::new(&shell).status();
                app = Installer::new(app.into_context());
            }
        }
    }
}
