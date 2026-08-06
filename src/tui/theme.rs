// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! Shared 16-colour terminal palette.
//!
//! Each constant is an ANSI palette index carried by the offscreen renderer
//! ([`crate::tui::render::Color`]). The indexes match the crossterm named colours
//! the installer previously used, so the on-screen look is unchanged.

use crate::tui::render::Color;

const fn idx(index: u8) -> Color {
    Color::indexed(index)
}

pub(crate) const BACKGROUND: Color = idx(0); // black
pub(crate) const PANEL: Color = idx(8); // bright black (dark grey)
pub(crate) const TITLE: Color = idx(12); // bright blue
pub(crate) const TITLE_DETAIL: Color = idx(4); // blue
pub(crate) const FOREGROUND: Color = idx(15); // bright white
pub(crate) const MUTED: Color = idx(7); // grey
pub(crate) const ACCENT: Color = idx(14); // bright cyan
pub(crate) const SELECTED: Color = idx(6); // cyan
pub(crate) const ON_SELECTED: Color = idx(0); // black
pub(crate) const SUCCESS: Color = idx(10); // bright green
pub(crate) const WARNING: Color = idx(11); // bright yellow
pub(crate) const DANGER: Color = idx(9); // bright red
pub(crate) const LAND: Color = idx(2); // green
pub(crate) const COAST: Color = idx(10); // bright green
pub(crate) const SEA: Color = idx(4); // blue
pub(crate) const SEA_SHIMMER: Color = idx(12); // bright blue
pub(crate) const GUIDE: Color = idx(6); // cyan
pub(crate) const LAND_NIGHT: Color = idx(8); // dark grey
pub(crate) const COAST_NIGHT: Color = idx(7); // grey
pub(crate) const SEA_NIGHT: Color = idx(0); // black
pub(crate) const SEA_NIGHT_SHIMMER: Color = idx(8); // dark grey
pub(crate) const GUIDE_NIGHT: Color = idx(8); // dark grey
/// Thin twilight band along the day/night terminator.
pub(crate) const TERMINATOR: Color = idx(13); // bright magenta
