// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! Installer chrome and globe materials.
//!
//! Chrome painters take a [`Theme`] and named styles. Colour fields are
//! private; a fill or border goes through a role accessor. Globe land, sea,
//! lighting samples, and overlays live on [`MapPalette`]. Look primitives
//! live in [`crate::tui::style`].

use crate::tui::style::{Color, Style};

/// Named RGB tokens for installer chrome and the globe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Theme {
    background: Color,
    panel: Color,
    title_color: Color,
    title_detail: Color,
    foreground: Color,
    muted_color: Color,
    accent_color: Color,
    success_color: Color,
    warning_color: Color,
    danger_color: Color,
    map: MapPalette,
}

/// Named map materials and lighting samples.
///
/// Day, night, and twilight are 32-bit RGB. Sampling lerps these colours;
/// glyph classification compares against the same tokens after quantize.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MapPalette {
    pub(crate) land: Color,
    pub(crate) coast: Color,
    pub(crate) sea: Color,
    pub(crate) guide: Color,
    pub(crate) land_night: Color,
    pub(crate) coast_night: Color,
    pub(crate) sea_night: Color,
    pub(crate) guide_night: Color,
    pub(crate) land_twilight: Color,
    pub(crate) sea_twilight: Color,
    pub(crate) guide_twilight: Color,
    pub(crate) grid_cross: Color,
    pub(crate) legend: Color,
    pub(crate) city: Color,
    pub(crate) pin: Color,
}

impl MapPalette {
    pub(crate) const INSTALLER: Self = Self {
        land: Color::rgb(0, 170, 0),
        coast: Color::rgb(85, 255, 85),
        sea: Color::rgb(0, 0, 170),
        guide: Color::rgb(0, 170, 170),
        land_night: Color::rgb(0, 85, 0),
        coast_night: Color::rgb(0, 85, 0),
        sea_night: Color::rgb(0, 0, 0),
        guide_night: Color::rgb(0, 0, 85),
        land_twilight: Color::rgb(0, 85, 0),
        sea_twilight: Color::rgb(0, 0, 85),
        guide_twilight: Color::rgb(0, 85, 85),
        grid_cross: Color::rgb(85, 255, 255),
        legend: Color::rgb(170, 170, 170),
        city: Color::rgb(85, 255, 255),
        pin: Color::rgb(255, 255, 85),
    };
}

impl Theme {
    pub(crate) const INSTALLER: Self = Self {
        background: Color::rgb(0, 0, 0),
        panel: Color::rgb(85, 85, 85),
        title_color: Color::rgb(85, 85, 255),
        title_detail: Color::rgb(0, 0, 170),
        foreground: Color::rgb(255, 255, 255),
        muted_color: Color::rgb(170, 170, 170),
        accent_color: Color::rgb(85, 255, 255),
        success_color: Color::rgb(85, 255, 85),
        warning_color: Color::rgb(255, 255, 85),
        danger_color: Color::rgb(255, 85, 85),
        map: MapPalette::INSTALLER,
    };

    fn pair(fg: Color, bg: Color) -> Style {
        Style::default().fg(fg).bg(bg)
    }

    pub(crate) fn canvas(self) -> Style {
        Style::default().bg(self.background)
    }

    pub(crate) fn title(self) -> Style {
        Self::pair(self.foreground, self.title_color)
    }

    pub(crate) fn subtitle(self) -> Style {
        Self::pair(self.accent_color, self.title_detail)
    }

    pub(crate) fn text(self) -> Style {
        Self::pair(self.foreground, self.background)
    }

    pub(crate) fn muted(self) -> Style {
        Self::pair(self.muted_color, self.background)
    }

    pub(crate) fn accent(self) -> Style {
        Self::pair(self.accent_color, self.background)
    }

    pub(crate) fn success(self) -> Style {
        Self::pair(self.success_color, self.background)
    }

    pub(crate) fn warning(self) -> Style {
        Self::pair(self.warning_color, self.background)
    }

    pub(crate) fn danger(self) -> Style {
        Self::pair(self.danger_color, self.background)
    }

    pub(crate) fn well(self) -> Style {
        Style::default().bg(self.panel)
    }

    pub(crate) fn border(self) -> Style {
        Self::pair(self.panel, self.background)
    }

    pub(crate) fn inverse(self) -> Style {
        Self::pair(self.background, self.accent_color)
    }

    fn on_canvas(self, color: Color) -> Style {
        Self::pair(color, self.background)
    }

    fn on_well(self, color: Color) -> Style {
        Self::pair(color, self.panel)
    }

    pub(crate) fn well_text(self) -> Style {
        self.on_well(self.foreground)
    }

    pub(crate) fn well_muted(self) -> Style {
        self.on_well(self.muted_color)
    }

    pub(crate) fn well_accent(self) -> Style {
        self.on_well(self.accent_color)
    }

    pub(crate) fn well_warning(self) -> Style {
        self.on_well(self.warning_color)
    }

    pub(crate) fn well_danger(self) -> Style {
        self.on_well(self.danger_color)
    }

    pub(crate) fn map(self) -> MapPalette {
        self.map
    }

    pub(crate) fn track(self) -> Style {
        self.on_canvas(self.panel)
    }
}
