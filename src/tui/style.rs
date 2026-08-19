// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! Portable look primitives: RGB colour, cell style, and downsample tables.
//!
//! Colours are 32-bit RGB (`0x00RRGGBB`). Quantize maps the same RGB onto 16
//! or 256 colours. Emit, in the renderer, keeps the short SGR form on
//! `Ansi16` and promotes when the terminal advertised it. Style construction
//! is `fg`/`bg`/`bold`/`dim`; emit reads colours and modifiers through
//! getters.

/// A 32-bit RGB colour (`0x00RRGGBB`).
///
/// The cell buffer and theme constants always store RGB. Emit quantizes through
/// [`ANSI16_RGB`] or the 256-colour cube when the terminal advertised 16 or 256
/// colours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Color(u32);

impl Default for Color {
    fn default() -> Self {
        Self::rgb(0, 0, 0)
    }
}

impl Color {
    pub(crate) const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self(((r as u32) << 16) | ((g as u32) << 8) | (b as u32))
    }
    pub(crate) const fn channels(self) -> (u8, u8, u8) {
        (
            ((self.0 >> 16) & 0xFF) as u8,
            ((self.0 >> 8) & 0xFF) as u8,
            (self.0 & 0xFF) as u8,
        )
    }

    pub(crate) fn ansi16_index(self) -> u8 {
        let (r, g, b) = self.channels();
        nearest_ansi16(r, g, b)
    }

    pub(crate) fn ansi256_index(self) -> u8 {
        let (r, g, b) = self.channels();
        rgb_to_ansi256(r, g, b)
    }

    pub(crate) fn quantize(self, mode: ColorMode) -> Self {
        match mode {
            ColorMode::Mono | ColorMode::Ansi16 => {
                let index = self.ansi16_index();
                match ANSI16_RGB.get(usize::from(index)) {
                    Some(&(r, g, b)) => Self::rgb(r, g, b),
                    None => Self::rgb(0, 0, 0),
                }
            }
            ColorMode::Ansi256 => {
                let (r, g, b) = ansi256_to_rgb(self.ansi256_index());
                Self::rgb(r, g, b)
            }
            ColorMode::TrueColor => self,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Modifier(u8);

impl Modifier {
    pub(crate) const BOLD: Self = Self(1 << 0);
    pub(crate) const DIM: Self = Self(1 << 1);
    pub(crate) const REVERSE: Self = Self(1 << 2);

    fn contains(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    fn dropped_by(self, next: Self) -> bool {
        self.0 & !next.0 != 0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Style {
    fg: Option<Color>,
    bg: Option<Color>,
    modifiers: Modifier,
}

impl Style {
    pub(crate) fn fg(mut self, color: Color) -> Self {
        self.fg = Some(color);
        self
    }

    pub(crate) fn bg(mut self, color: Color) -> Self {
        self.bg = Some(color);
        self
    }

    fn add_modifier(mut self, modifier: Modifier) -> Self {
        self.modifiers = self.modifiers.union(modifier);
        self
    }

    pub(crate) fn bold(self) -> Self {
        self.add_modifier(Modifier::BOLD)
    }

    pub(crate) fn dim(self) -> Self {
        self.add_modifier(Modifier::DIM)
    }

    pub(crate) fn patch(self, overlay: Self) -> Self {
        Self {
            fg: overlay.fg.or(self.fg),
            bg: overlay.bg.or(self.bg),
            modifiers: self.modifiers.union(overlay.modifiers),
        }
    }

    pub(crate) fn quantize(self, mode: ColorMode) -> Self {
        Self {
            fg: self.fg.map(|color| color.quantize(mode)),
            bg: self.bg.map(|color| color.quantize(mode)),
            modifiers: self.modifiers,
        }
    }

    pub(crate) fn foreground(self) -> Option<Color> {
        self.fg
    }

    pub(crate) fn background(self) -> Option<Color> {
        self.bg
    }

    pub(crate) fn has_background(self) -> bool {
        self.bg.is_some()
    }

    pub(crate) fn has_modifier(self, modifier: Modifier) -> bool {
        self.modifiers.contains(modifier)
    }

    pub(crate) fn drops_modifiers(self, next: Self) -> bool {
        self.modifiers.dropped_by(next.modifiers)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ColorMode {
    Mono,
    #[default]
    Ansi16,
    Ansi256,
    TrueColor,
}

const ANSI16_RGB: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (170, 0, 0),
    (0, 170, 0),
    (170, 85, 0),
    (0, 0, 170),
    (170, 0, 170),
    (0, 170, 170),
    (170, 170, 170),
    (85, 85, 85),
    (255, 85, 85),
    (85, 255, 85),
    (255, 255, 85),
    (85, 85, 255),
    (255, 85, 255),
    (85, 255, 255),
    (255, 255, 255),
];

fn color_distance(r: u8, g: u8, b: u8, pr: u8, pg: u8, pb: u8) -> i32 {
    let dr = i32::from(r) - i32::from(pr);
    let dg = i32::from(g) - i32::from(pg);
    let db = i32::from(b) - i32::from(pb);
    dr * dr + dg * dg + db * db
}

fn nearest_ansi16(r: u8, g: u8, b: u8) -> u8 {
    ANSI16_RGB
        .iter()
        .copied()
        .enumerate()
        .min_by_key(|&(_, (pr, pg, pb))| {
            let chroma = i32::from(pr.max(pg).max(pb)) - i32::from(pr.min(pg).min(pb));
            (color_distance(r, g, b, pr, pg, pb), -chroma)
        })
        .map_or(0, |(index, _)| index as u8)
}

fn cube_level(channel: u8) -> u8 {
    if channel < 48 {
        0
    } else if channel < 115 {
        1
    } else {
        ((u16::from(channel) - 35) / 40).min(5) as u8
    }
}

fn cube_value(level: u8) -> u8 {
    if level == 0 { 0 } else { 55 + 40 * level }
}

fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    if r == g && g == b {
        if r < 8 {
            return 16;
        }
        if r > 238 {
            return 231;
        }
        return 232 + ((u16::from(r) - 8) / 10) as u8;
    }
    let ri = cube_level(r);
    let gi = cube_level(g);
    let bi = cube_level(b);
    let cube = 16 + 36 * ri + 6 * gi + bi;
    let cube_dist = color_distance(r, g, b, cube_value(ri), cube_value(gi), cube_value(bi));
    let indexed = nearest_ansi16(r, g, b);
    let Some(&(ir, ig, ib)) = ANSI16_RGB.get(indexed as usize) else {
        return cube;
    };
    if color_distance(r, g, b, ir, ig, ib) <= cube_dist {
        indexed
    } else {
        cube
    }
}

fn ansi256_to_rgb(index: u8) -> (u8, u8, u8) {
    if index < 16 {
        return ANSI16_RGB
            .get(usize::from(index))
            .copied()
            .unwrap_or((0, 0, 0));
    }
    if index >= 232 {
        let value = 8 + 10 * (index - 232);
        return (value, value, value);
    }
    let cube = index - 16;
    let ri = cube / 36;
    let gi = (cube % 36) / 6;
    let bi = cube % 6;
    (cube_value(ri), cube_value(gi), cube_value(bi))
}
