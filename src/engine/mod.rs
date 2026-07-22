// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

pub(crate) mod block;
pub(crate) mod command;
pub(crate) mod config;
pub(crate) mod der;
pub(crate) mod firmware;
pub(crate) mod image;
pub(crate) mod partition;
pub(crate) mod passwd;
pub(crate) mod pipeline;
pub(crate) mod progress;
pub(crate) mod secureboot;
pub(crate) mod steps;
pub(crate) mod util;

pub(crate) const INSTALLATION_LOG_TARGET: &str = "puu_installer::installation";
