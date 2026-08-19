// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! Shared terminal palette.
//!
//! Each constant is 32-bit RGB carried by the offscreen renderer
//! ([`crate::tui::style::Color`]). The values match the VGA RGB the map already
//! samples. Emit quantizes them to 16 or 256 colours when the terminal
//! advertised that depth.

use crate::tui::style::Color;

pub(crate) const BACKGROUND: Color = Color::rgb(0, 0, 0); // black
pub(crate) const PANEL: Color = Color::rgb(85, 85, 85); // bright black (dark grey)
pub(crate) const TITLE: Color = Color::rgb(85, 85, 255); // bright blue
pub(crate) const TITLE_DETAIL: Color = Color::rgb(0, 0, 170); // blue
pub(crate) const FOREGROUND: Color = Color::rgb(255, 255, 255); // bright white
pub(crate) const MUTED: Color = Color::rgb(170, 170, 170); // grey
pub(crate) const ACCENT: Color = Color::rgb(85, 255, 255); // bright cyan
pub(crate) const SUCCESS: Color = Color::rgb(85, 255, 85); // bright green
pub(crate) const WARNING: Color = Color::rgb(255, 255, 85); // bright yellow
pub(crate) const DANGER: Color = Color::rgb(255, 85, 85); // bright red
pub(crate) const LAND: Color = Color::rgb(0, 170, 0); // green
pub(crate) const COAST: Color = Color::rgb(85, 255, 85); // bright green
pub(crate) const SEA: Color = Color::rgb(0, 0, 170); // blue
pub(crate) const SEA_SHIMMER: Color = Color::rgb(85, 85, 255); // bright blue
pub(crate) const GUIDE: Color = Color::rgb(0, 170, 170); // cyan
pub(crate) const LAND_NIGHT: Color = Color::rgb(0, 85, 0); // green, dimmed by lighting
pub(crate) const COAST_NIGHT: Color = Color::rgb(0, 85, 0); // green, dimmed by lighting
pub(crate) const SEA_NIGHT: Color = Color::rgb(0, 0, 0); // black
pub(crate) const GUIDE_NIGHT: Color = Color::rgb(0, 0, 85); // blue, dimmed by lighting
pub(crate) const LAND_TWILIGHT: Color = Color::rgb(0, 85, 0);
pub(crate) const SEA_TWILIGHT: Color = Color::rgb(0, 0, 85);
pub(crate) const GUIDE_TWILIGHT: Color = Color::rgb(0, 85, 85);
