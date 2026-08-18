// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! Shared terminal palette.
//!
//! Each constant is a 16-colour ANSI index carried by the offscreen renderer
//! ([`crate::tui::render::Color`]). The indexes match the VGA RGB the map already
//! samples. Emit keeps the short SGR form on `Ansi16` and promotes the same RGB
//! to 256 or 16M when the terminal advertised it.

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
pub(crate) const SUCCESS: Color = idx(10); // bright green
pub(crate) const WARNING: Color = idx(11); // bright yellow
pub(crate) const DANGER: Color = idx(9); // bright red
pub(crate) const LAND: Color = idx(2); // green
pub(crate) const COAST: Color = idx(10); // bright green
pub(crate) const SEA: Color = idx(4); // blue
pub(crate) const SEA_SHIMMER: Color = idx(12); // bright blue
pub(crate) const GUIDE: Color = idx(6); // cyan
pub(crate) const LAND_NIGHT: Color = idx(2); // green, dimmed by lighting
pub(crate) const COAST_NIGHT: Color = idx(2); // green, dimmed by lighting
pub(crate) const SEA_NIGHT: Color = idx(0); // black
pub(crate) const GUIDE_NIGHT: Color = idx(4); // blue, dimmed by lighting
