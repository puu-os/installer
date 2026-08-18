// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! Timezone selection and the world-map framebuffer painter.
//!
//! The map is sampled into a 32-bit RGB window (four pixels per cell) and
//! only converted to console glyphs when that window is copied into the
//! offscreen [`Frame`]. The camera origin is a fractional map-x so panning
//! moves by millidegrees rather than whole source columns.

use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::tui::message::Effect;
use crate::tui::render::{Color, ColorMode, Frame, Modifier, Rect, Style, color_mode};
use crate::tui::state::{AppState, Route};
use crate::tui::theme;

const VIEW_LON_SPAN: f64 = 180.0;
const LAT_MIN: f64 = -60.0;
const LAT_MAX: f64 = 78.0;
const PAN_TAU: f64 = 0.12;
const PAN_MIN_SPEED: f64 = 12.0;
const PAN_MAX_SPEED: f64 = 900.0;
const PAN_SNAP: f64 = 0.02;
const PAN_KEY_STEP: f64 = 10.0;
const PAN_MAX_LEAD: f64 = 20.0;
const MAX_FRAME_DT: f64 = 0.05;
pub(crate) const VISIBLE_ZONE_ROWS: usize = 6;
const MAP_W: usize = 720;
const MAP_H: usize = 360;
const ROW_BYTES: usize = MAP_W / 8;
const INTEGRAL_STRIDE: usize = MAP_W + 1;
const LON_SCALE: f64 = MAP_W as f64 / 360.0;
const LAT_SCALE: f64 = MAP_H as f64 / 180.0;
const ZONE_TAB: &str = include_str!("zone1970.tab");
const WORLD_MASK: &[u8] = include_bytes!("world_mask.bin");
/// Solar elevation (degrees) above which a cell is fully day.
const DAY_ELEVATION_DEG: f64 = 0.0;
/// Half-width of the twilight band drawn along the terminator.
const TWILIGHT_HALF_WIDTH_DEG: f64 = 4.0;
/// Minimum map size for city labels (marker + short name).
const CITY_LABEL_MIN_WIDTH: usize = 40;
const CITY_LABEL_MIN_HEIGHT: usize = 12;
/// Minimum map size for the bottom-left legend strip.
const LEGEND_MIN_WIDTH: usize = 28;
const LEGEND_MIN_HEIGHT: usize = 8;
/// Curated major cities used as optional map annotations.
/// Short ASCII labels keep the overlay readable on narrow terminals.
const CITY_LABELS: &[(&str, f64, f64)] = &[
    ("Anchorage", 61.2181, -149.9003),
    ("Honolulu", 21.3069, -157.8583),
    ("LA", 34.0522, -118.2428),
    ("Denver", 39.7392, -104.9842),
    ("Chicago", 41.8500, -87.6500),
    ("NYC", 40.7142, -74.0064),
    ("Toronto", 43.6500, -79.3833),
    ("Mexico", 19.4000, -99.1500),
    ("SaoPaulo", -23.5333, -46.6167),
    ("BsAs", -34.6000, -58.4500),
    ("London", 51.5083, -0.1253),
    ("Paris", 48.8667, 2.3333),
    ("Madrid", 40.4000, -3.6833),
    ("Berlin", 52.5000, 13.3667),
    ("Rome", 41.9000, 12.4833),
    ("Helsinki", 60.1667, 24.9667),
    ("Istanbul", 41.0167, 28.9667),
    ("Moscow", 55.7558, 37.6178),
    ("Cairo", 30.0500, 31.2500),
    ("Lagos", 6.4500, 3.4000),
    ("Joburg", -26.2500, 28.0000),
    ("Dubai", 25.3000, 55.3000),
    ("Kolkata", 22.5333, 88.3667),
    ("Bangkok", 13.7500, 100.5167),
    ("SGP", 1.2833, 103.8500),
    ("Jakarta", -6.1667, 106.8000),
    ("HK", 22.2833, 114.1500),
    ("Shanghai", 31.2333, 121.4667),
    ("Seoul", 37.5500, 126.9667),
    ("Tokyo", 35.6544, 139.7447),
    ("Sydney", -33.8667, 151.2167),
    ("Auckland", -36.8667, 174.7667),
];

#[derive(Clone, Debug)]
pub(crate) struct Zone {
    pub(crate) name: String,
    search_name: String,
    countries: String,
    comment: String,
    lat: f64,
    lon: f64,
    has_coord: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct MapPoint {
    lat: f64,
    lon: f64,
    /// Short label drawn beside the selection pin when space allows.
    label: String,
}

/// One visible timezone list entry with optional search-match span.
#[derive(Clone, Debug)]
pub(crate) struct VisibleZone {
    pub(crate) name: &'static str,
    pub(crate) selected: bool,
    /// Inclusive-exclusive character range of the first filter match, if any.
    pub(crate) match_range: Option<(usize, usize)>,
}

/// Details for the currently selected timezone.
#[derive(Clone, Debug)]
pub(crate) struct ZoneDetails {
    pub(crate) name: String,
    pub(crate) countries: String,
    pub(crate) comment: String,
    pub(crate) coords: Option<(f64, f64)>,
    pub(crate) match_index: usize,
    pub(crate) match_total: usize,
}

fn parse_zone_line(line: &str) -> Option<Zone> {
    let line = line.trim_end();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut columns = line.split('\t');
    let countries = columns.next()?.to_string();
    let coord = columns.next()?;
    let name = columns.next()?.to_string();
    let comment = columns.next().unwrap_or("").to_string();
    let search_name = name.to_ascii_lowercase();
    let coord =
        iso6709parse::parse_string_representation::<iso6709parse::ISO6709Coord>(coord).ok()?;
    Some(Zone {
        name,
        search_name,
        countries,
        comment,
        lat: coord.lat,
        lon: coord.lon,
        has_coord: true,
    })
}

fn zones() -> &'static [Zone] {
    static ZONES: OnceLock<Vec<Zone>> = OnceLock::new();
    ZONES.get_or_init(|| {
        let mut zones: Vec<Zone> = ZONE_TAB.lines().filter_map(parse_zone_line).collect();
        zones.sort_by(|left, right| left.name.cmp(&right.name));
        zones.insert(
            0,
            Zone {
                name: "UTC".to_string(),
                search_name: "utc".to_string(),
                countries: String::new(),
                comment: "Coordinated Universal Time".to_string(),
                lat: 0.0,
                lon: 0.0,
                has_coord: false,
            },
        );
        zones
    })
}

fn wrap_lon(lon: f64) -> f64 {
    (lon + 180.0).rem_euclid(360.0) - 180.0
}

fn lon_delta(from: f64, to: f64) -> f64 {
    wrap_lon(to - from)
}

/// Current Unix time used for map lighting.
fn map_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// Unix minute used for the day/night terminator (stable within a minute).
pub(crate) fn map_sun_minute() -> u64 {
    map_unix_secs() / 60
}

/// Subsolar latitude (declination) and longitude for the given unix time.
///
/// Approximate geometric model: mean solar longitude from day fraction, and
/// a simple sinusoidal declination. Accurate enough for a terminal-map
/// terminator and free of extra dependencies.
fn subsolar_point(unix_secs: u64) -> (f64, f64) {
    let day = unix_secs as f64 / 86_400.0;
    // Day-of-year in [0, 365).
    let day_of_year = day.rem_euclid(365.2425);
    // Approximate solar declination (degrees). Peak near June 21 (day ~172).
    let declination = -23.44 * ((360.0 / 365.0) * (day_of_year + 10.0)).to_radians().cos();
    // Subsolar longitude: sun is overhead at local solar noon.
    let day_fraction = (unix_secs % 86_400) as f64 / 86_400.0;
    let longitude = wrap_lon(180.0 - 360.0 * day_fraction);
    (declination, longitude)
}

/// Sine of the solar elevation angle at (lat, lon) for a given subsolar point.
fn solar_elevation_sin(lat: f64, lon: f64, sun_lat: f64, sun_lon: f64) -> f64 {
    let lat_r = lat.to_radians();
    let sun_lat_r = sun_lat.to_radians();
    let hour_angle = lon_delta(sun_lon, lon).to_radians();
    lat_r.sin() * sun_lat_r.sin() + lat_r.cos() * sun_lat_r.cos() * hour_angle.cos()
}

fn lighting_factor(lat: f64, lon: f64, sun_lat: f64, sun_lon: f64) -> f64 {
    let elevation = solar_elevation_sin(lat, lon, sun_lat, sun_lon)
        .clamp(-1.0, 1.0)
        .asin()
        .to_degrees();
    let lo = DAY_ELEVATION_DEG - TWILIGHT_HALF_WIDTH_DEG;
    let hi = DAY_ELEVATION_DEG + TWILIGHT_HALF_WIDTH_DEG;
    let span = hi - lo;
    if span <= f64::EPSILON {
        if elevation >= DAY_ELEVATION_DEG {
            1.0
        } else {
            0.0
        }
    } else {
        ((elevation - lo) / span).clamp(0.0, 1.0)
    }
}

const fn pack_rgb(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

fn unpack_rgb(pixel: u32) -> (u8, u8, u8) {
    (
        ((pixel >> 16) & 0xFF) as u8,
        ((pixel >> 8) & 0xFF) as u8,
        (pixel & 0xFF) as u8,
    )
}

const LAND_RGB: u32 = pack_rgb(0, 170, 0);
const COAST_RGB: u32 = pack_rgb(85, 255, 85);
const SEA_RGB: u32 = pack_rgb(0, 0, 170);
const SEA_SHIMMER_RGB: u32 = pack_rgb(85, 85, 255);
const GUIDE_RGB: u32 = pack_rgb(0, 170, 170);
const LAND_NIGHT_RGB: u32 = pack_rgb(0, 85, 0);
const COAST_NIGHT_RGB: u32 = pack_rgb(0, 85, 0);
const SEA_NIGHT_RGB: u32 = pack_rgb(0, 0, 0);
const GUIDE_NIGHT_RGB: u32 = pack_rgb(0, 0, 85);
const LAND_TWILIGHT_RGB: u32 = pack_rgb(0, 85, 0);
const SEA_TWILIGHT_RGB: u32 = pack_rgb(0, 0, 85);
const GUIDE_TWILIGHT_RGB: u32 = pack_rgb(0, 85, 85);
const ACCENT_RGB: u32 = pack_rgb(85, 255, 255);

const THEME_SWATCHES: [(Color, u32); 10] = [
    (theme::LAND, LAND_RGB),
    (theme::COAST, COAST_RGB),
    (theme::SEA, SEA_RGB),
    (theme::SEA_SHIMMER, SEA_SHIMMER_RGB),
    (theme::GUIDE, GUIDE_RGB),
    (theme::LAND_NIGHT, LAND_NIGHT_RGB),
    (theme::COAST_NIGHT, COAST_NIGHT_RGB),
    (theme::SEA_NIGHT, SEA_NIGHT_RGB),
    (theme::GUIDE_NIGHT, GUIDE_NIGHT_RGB),
    (theme::ACCENT, ACCENT_RGB),
];

fn lerp_u8(a: u8, b: u8, t: f64) -> u8 {
    let t = t.clamp(0.0, 1.0);
    let mixed = f64::from(a) + (f64::from(b) - f64::from(a)) * t;
    mixed.round() as u8
}

fn lerp_rgb(a: u32, b: u32, t: f64) -> u32 {
    let (ar, ag, ab) = unpack_rgb(a);
    let (br, bg, bb) = unpack_rgb(b);
    pack_rgb(lerp_u8(ar, br, t), lerp_u8(ag, bg, t), lerp_u8(ab, bb, t))
}

fn light_rgb(night: u32, twilight: u32, day: u32, t: f64) -> u32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        lerp_rgb(night, twilight, t * 2.0)
    } else {
        lerp_rgb(twilight, day, (t - 0.5) * 2.0)
    }
}

fn snap_theme(pixel: u32) -> Color {
    let (r, g, b) = unpack_rgb(pixel);
    THEME_SWATCHES
        .iter()
        .min_by_key(|(_, rgb)| {
            let (pr, pg, pb) = unpack_rgb(*rgb);
            let dr = i32::from(r) - i32::from(pr);
            let dg = i32::from(g) - i32::from(pg);
            let db = i32::from(b) - i32::from(pb);
            dr * dr + dg * dg + db * db
        })
        .map_or(theme::SEA, |(color, _)| *color)
}

fn snap_pixel(pixel: u32) -> Color {
    let (r, g, b) = unpack_rgb(pixel);
    match color_mode() {
        ColorMode::Mono | ColorMode::Ansi16 => snap_theme(pixel),
        mode => Color::from_rgb(r, g, b, mode),
    }
}

fn sample_pixel(coverage: f64, light: f64, lon: f64, on_parallel: bool, lon_tolerance: f64) -> u32 {
    if coverage <= 0.01 {
        let on_meridian = near_grid(lon, 30.0, lon_tolerance);
        if on_parallel && on_meridian {
            light_rgb(GUIDE_NIGHT_RGB, GUIDE_TWILIGHT_RGB, ACCENT_RGB, light)
        } else if on_parallel || on_meridian {
            light_rgb(GUIDE_NIGHT_RGB, GUIDE_TWILIGHT_RGB, GUIDE_RGB, light)
        } else {
            light_rgb(SEA_NIGHT_RGB, SEA_TWILIGHT_RGB, SEA_RGB, light)
        }
    } else if coverage >= 0.99 {
        light_rgb(LAND_NIGHT_RGB, LAND_TWILIGHT_RGB, LAND_RGB, light)
    } else {
        let span = (coverage - 0.01) / 0.98;
        let day = lerp_rgb(COAST_RGB, LAND_RGB, span);
        let night = lerp_rgb(COAST_NIGHT_RGB, LAND_NIGHT_RGB, span);
        let twilight = lerp_rgb(SEA_TWILIGHT_RGB, LAND_TWILIGHT_RGB, span);
        light_rgb(night, twilight, day, light)
    }
}

struct MapView {
    center_lon: f64,
    target_lon: f64,
}

impl MapView {
    fn new(selected: Option<&Zone>) -> Self {
        let center_lon = selected
            .filter(|zone| zone.has_coord)
            .map_or(0.0, |zone| wrap_lon(zone.lon));
        Self {
            center_lon,
            target_lon: center_lon,
        }
    }

    fn follow(&mut self, zone: &Zone) {
        if zone.has_coord {
            self.target_lon = wrap_lon(zone.lon);
        }
    }

    fn nudge(&mut self, direction: f64) {
        let lead = (lon_delta(self.center_lon, self.target_lon) + direction * PAN_KEY_STEP)
            .clamp(-PAN_MAX_LEAD, PAN_MAX_LEAD);
        self.target_lon = wrap_lon(self.center_lon + lead);
    }

    fn page_pan(&mut self, direction: f64) {
        self.target_lon = wrap_lon(self.target_lon + VIEW_LON_SPAN * 0.5 * direction);
    }

    fn tick(&mut self, elapsed: Duration) -> bool {
        let delta = lon_delta(self.center_lon, self.target_lon);
        if delta.abs() <= PAN_SNAP {
            self.center_lon = self.target_lon;
            return false;
        }
        let dt = elapsed.as_secs_f64().min(MAX_FRAME_DT);
        let step = (delta.abs() * (1.0 - (-dt / PAN_TAU).exp()))
            .max(PAN_MIN_SPEED * dt)
            .min(PAN_MAX_SPEED * dt)
            .min(delta.abs());
        self.center_lon = wrap_lon(self.center_lon + delta.signum() * step);
        true
    }
}

struct TzPick {
    filtered: Vec<usize>,
    selected: usize,
    top: usize,
    filter: String,
}

impl TzPick {
    fn new(current: &str) -> Self {
        let filtered: Vec<usize> = (0..zones().len()).collect();
        let selected = filtered
            .iter()
            .position(|&index| zones()[index].name == current)
            .unwrap_or(0);
        let mut pick = Self {
            filtered,
            selected,
            top: 0,
            filter: String::new(),
        };
        pick.ensure_visible();
        pick
    }

    fn selected(&self) -> Option<&'static Zone> {
        self.filtered
            .get(self.selected)
            .map(|&index| &zones()[index])
    }

    fn refilter(&mut self) {
        let needle = self.filter.to_ascii_lowercase();
        let retained = self.filtered.get(self.selected).copied();
        self.filtered = zones()
            .iter()
            .enumerate()
            .filter(|(_, zone)| needle.is_empty() || zone.search_name.contains(&needle))
            .map(|(index, _)| index)
            .collect();
        self.selected = retained
            .and_then(|index| self.filtered.iter().position(|&current| current == index))
            .unwrap_or(0);
        if self.selected >= self.filtered.len() && !self.filtered.is_empty() {
            self.selected = self.filtered.len() - 1;
        }
        self.ensure_visible();
    }

    fn ensure_visible(&mut self) {
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + VISIBLE_ZONE_ROWS {
            self.top = self.selected + 1 - VISIBLE_ZONE_ROWS;
        }
        self.top = self
            .top
            .min(self.filtered.len().saturating_sub(VISIBLE_ZONE_ROWS));
    }

    fn move_by(&mut self, direction: isize) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected =
            (self.selected as isize + direction).rem_euclid(self.filtered.len() as isize) as usize;
        self.ensure_visible();
    }
}

fn reconstruct_pick(state: &AppState) -> TzPick {
    TzPick {
        filtered: state.timezone.filtered.clone(),
        selected: state.timezone.selected,
        top: state.timezone.top,
        filter: state.timezone.filter.clone(),
    }
}

fn reconstruct_view(state: &AppState) -> MapView {
    MapView {
        center_lon: state.timezone.map_center_lon,
        target_lon: state.timezone.map_target_lon,
    }
}

fn store(state: &mut AppState, pick: &TzPick, view: &MapView) {
    state.timezone.filtered.clone_from(&pick.filtered);
    state.timezone.selected = pick.selected;
    state.timezone.top = pick.top;
    state.timezone.filter.clone_from(&pick.filter);
    state.timezone.map_center_lon = view.center_lon;
    state.timezone.map_target_lon = view.target_lon;
}

pub(crate) fn prepare(state: &mut AppState, current: &str) {
    let pick = TzPick::new(current);
    let view = MapView::new(pick.selected());
    store(state, &pick, &view);
}

pub(crate) fn update_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    let mut pick = reconstruct_pick(state);
    let mut view = reconstruct_view(state);
    match key.code {
        KeyCode::Esc => state.route = Route::Identity,
        KeyCode::Enter => {
            if let Some(zone) = pick.selected() {
                state.route = Route::SecureBoot;
                let name = zone.name.clone();
                store(state, &pick, &view);
                return vec![Effect::SelectTimezone { name }, Effect::DetectFirmware];
            }
        }
        KeyCode::Up => {
            pick.move_by(-1);
            if let Some(zone) = pick.selected() {
                view.follow(zone);
            }
        }
        KeyCode::Down => {
            pick.move_by(1);
            if let Some(zone) = pick.selected() {
                view.follow(zone);
            }
        }
        KeyCode::Left => view.nudge(-1.0),
        KeyCode::Right => view.nudge(1.0),
        KeyCode::PageUp => view.page_pan(-1.0),
        KeyCode::PageDown => view.page_pan(1.0),
        KeyCode::Home | KeyCode::Char('.') => {
            if let Some(zone) = pick.selected() {
                view.follow(zone);
            }
        }
        KeyCode::Backspace => {
            pick.filter.pop();
            pick.refilter();
            if let Some(zone) = pick.selected() {
                view.follow(zone);
            }
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            pick.filter.clear();
            pick.refilter();
            if let Some(zone) = pick.selected() {
                view.follow(zone);
            }
        }
        KeyCode::Char(ch)
            if ch.is_ascii_graphic() && !key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            pick.filter.push(ch);
            pick.refilter();
            if let Some(zone) = pick.selected() {
                view.follow(zone);
            }
        }
        _ => {}
    }
    store(state, &pick, &view);
    Vec::new()
}

/// Select a filtered zone by absolute list index (not visible-row index).
pub(crate) fn select_index(state: &mut AppState, index: usize) {
    if index >= state.timezone.filtered.len() {
        return;
    }
    let mut pick = reconstruct_pick(state);
    let mut view = reconstruct_view(state);
    pick.selected = index;
    pick.ensure_visible();
    if let Some(zone) = pick.selected() {
        view.follow(zone);
    }
    store(state, &pick, &view);
}

/// Nudge or page-pan the world map from mouse input.
pub(crate) fn pan_map(state: &mut AppState, direction: f64, page: bool) {
    let pick = reconstruct_pick(state);
    let mut view = reconstruct_view(state);
    if page {
        view.page_pan(direction);
    } else {
        view.nudge(direction);
    }
    store(state, &pick, &view);
}

pub(crate) fn map_is_panning(state: &AppState) -> bool {
    lon_delta(state.timezone.map_center_lon, state.timezone.map_target_lon).abs() > PAN_SNAP
}

pub(crate) fn tick(state: &mut AppState, elapsed: Duration) -> bool {
    let mut view = reconstruct_view(state);
    view.tick(elapsed);
    if lon_delta(state.timezone.map_center_lon, view.center_lon).abs() == 0.0 {
        return false;
    }
    state.timezone.map_center_lon = view.center_lon;
    true
}

fn first_match_range(name: &str, filter: &str) -> Option<(usize, usize)> {
    if filter.is_empty() {
        return None;
    }
    let needle = filter.to_ascii_lowercase().chars().collect::<Vec<_>>();
    let haystack = name.to_ascii_lowercase().chars().collect::<Vec<_>>();
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    let limit = haystack.len() - needle.len();
    for start in 0..=limit {
        if haystack[start..start + needle.len()] == needle[..] {
            return Some((start, start + needle.len()));
        }
    }
    None
}

pub(crate) fn visible_zones(state: &AppState) -> Vec<VisibleZone> {
    let filter = state.timezone.filter.as_str();
    state
        .timezone
        .filtered
        .iter()
        .skip(state.timezone.top)
        .take(VISIBLE_ZONE_ROWS)
        .enumerate()
        .map(|(row, &index)| {
            let name = zones()[index].name.as_str();
            VisibleZone {
                name,
                selected: state.timezone.top + row == state.timezone.selected,
                match_range: first_match_range(name, filter),
            }
        })
        .collect()
}

pub(crate) fn selected_details(state: &AppState) -> Option<ZoneDetails> {
    let total = state.timezone.filtered.len();
    if total == 0 {
        return None;
    }
    let index = *state.timezone.filtered.get(state.timezone.selected)?;
    let zone = zones().get(index)?;
    Some(ZoneDetails {
        name: zone.name.clone(),
        countries: zone.countries.clone(),
        comment: zone.comment.clone(),
        coords: zone.has_coord.then_some((zone.lat, zone.lon)),
        match_index: state.timezone.selected + 1,
        match_total: total,
    })
}

pub(crate) fn selected_zone(state: &AppState) -> Option<MapPoint> {
    state
        .timezone
        .filtered
        .get(state.timezone.selected)
        .and_then(|&index| zones().get(index))
        .filter(|zone| zone.has_coord)
        .map(|zone| MapPoint {
            lat: zone.lat,
            lon: zone.lon,
            label: city_short_label(&zone.name),
        })
}

/// Prefer the city portion of a zone id ("Europe/Helsinki" -> "Helsinki").
fn city_short_label(zone_name: &str) -> String {
    let city = zone_name.rsplit('/').next().unwrap_or(zone_name);
    // Underscores are common in zone city names; keep labels ASCII-friendly.
    city.replace('_', " ")
}

struct LandIntegral {
    values: Vec<u32>,
}

impl LandIntegral {
    fn new() -> Self {
        let mut values = vec![0; INTEGRAL_STRIDE * (MAP_H + 1)];
        for y in 0..MAP_H {
            let mut row_sum = 0;
            for x in 0..MAP_W {
                let byte = WORLD_MASK[y * ROW_BYTES + x / 8];
                row_sum += u32::from(byte & (0x80 >> (x % 8)) != 0);
                values[(y + 1) * INTEGRAL_STRIDE + x + 1] =
                    values[y * INTEGRAL_STRIDE + x + 1] + row_sum;
            }
        }
        Self { values }
    }

    fn at(&self, x: f64, y: f64) -> f64 {
        let x = x.clamp(0.0, MAP_W as f64);
        let y = y.clamp(0.0, MAP_H as f64);
        let left = x.floor() as usize;
        let top = y.floor() as usize;
        let right = (left + 1).min(MAP_W);
        let bottom = (top + 1).min(MAP_H);
        let x_fraction = x - left as f64;
        let y_fraction = y - top as f64;
        let top_left = f64::from(self.values[top * INTEGRAL_STRIDE + left]);
        let top_right = f64::from(self.values[top * INTEGRAL_STRIDE + right]);
        let bottom_left = f64::from(self.values[bottom * INTEGRAL_STRIDE + left]);
        let bottom_right = f64::from(self.values[bottom * INTEGRAL_STRIDE + right]);
        let top_value = top_left + (top_right - top_left) * x_fraction;
        let bottom_value = bottom_left + (bottom_right - bottom_left) * x_fraction;
        top_value + (bottom_value - top_value) * y_fraction
    }

    fn rectangle(&self, left: f64, top: f64, right: f64, bottom: f64) -> f64 {
        self.at(right, bottom) - self.at(left, bottom) - self.at(right, top) + self.at(left, top)
    }

    fn wrapped_rectangle(&self, left: f64, top: f64, right: f64, bottom: f64) -> f64 {
        let width = right - left;
        let left = left.rem_euclid(MAP_W as f64);
        let right = left + width;
        if right <= MAP_W as f64 {
            self.rectangle(left, top, right, bottom)
        } else {
            self.rectangle(left, top, MAP_W as f64, bottom)
                + self.rectangle(0.0, top, right - MAP_W as f64, bottom)
        }
    }
}

fn land_integral() -> &'static LandIntegral {
    static LAND: OnceLock<LandIntegral> = OnceLock::new();
    LAND.get_or_init(LandIntegral::new)
}

fn land_fraction(left: f64, top: f64, right: f64, bottom: f64) -> f64 {
    let area = (right - left) * (bottom - top);
    if area <= f64::EPSILON {
        return 0.0;
    }
    land_integral().wrapped_rectangle(left, top, right, bottom) / area
}
fn near_grid(value: f64, spacing: f64, tolerance: f64) -> bool {
    (value - (value / spacing).round() * spacing).abs() <= tolerance
}

/// Cached 32-bit RGB window for one map view: four pixels per cell (2×2).
///
/// Rebuilt only when the fractional origin, viewport geometry, or solar minute
/// changes. Glyph conversion happens when the window is copied into the frame.
pub(crate) struct MapCache {
    origin_x: f64,
    width: usize,
    height: usize,
    /// Unix minute used when the day/night terminator was baked in.
    sun_minute: u64,
    pixels: Vec<u32>,
}

impl MapCache {
    fn cell_width(&self) -> f64 {
        VIEW_LON_SPAN * LON_SCALE / self.width.max(1) as f64
    }

    fn pixel_width(&self) -> usize {
        self.width.saturating_mul(2)
    }

    /// Project a geographic point through the cached window's origin and cell size.
    fn project(&self, lat: f64, lon: f64) -> Option<MapProj> {
        if !(LAT_MIN..=LAT_MAX).contains(&lat) {
            return None;
        }
        let width_f = self.width as f64;
        let height_f = self.height as f64;
        if width_f == 0.0 || height_f == 0.0 {
            return None;
        }
        let cell_width = self.cell_width();
        if cell_width <= 0.0 {
            return None;
        }
        let map_x = (wrap_lon(lon) + 180.0) * LON_SCALE;
        let x = (map_x - self.origin_x).rem_euclid(MAP_W as f64) / cell_width;
        let y = (LAT_MAX - lat) / (LAT_MAX - LAT_MIN) * height_f;
        if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 || x >= width_f || y >= height_f {
            return None;
        }
        let cell_x = isize::try_from(x as usize).ok()?;
        let cell_y = isize::try_from(y as usize).ok()?;
        Some(MapProj {
            cell_x,
            cell_y,
            frac_x: x - x.floor(),
            frac_y: y - y.floor(),
        })
    }
}

fn map_origin_x(center_lon: f64) -> f64 {
    let raw = (wrap_lon(center_lon - VIEW_LON_SPAN / 2.0) + 180.0) * LON_SCALE;
    raw.rem_euclid(MAP_W as f64)
}

fn origin_delta(left: f64, right: f64) -> f64 {
    let width = MAP_W as f64;
    if width <= 0.0 {
        return 0.0;
    }
    let delta = (left - right).rem_euclid(width);
    delta.min(width - delta)
}

fn same_origin(left: f64, right: f64) -> bool {
    origin_delta(left, right) <= f64::EPSILON
}

fn pixel_index(width: usize, x: usize, py: usize) -> Option<usize> {
    width.checked_mul(py).and_then(|row| row.checked_add(x))
}

fn pixel_at(pixels: &[u32], width: usize, x: usize, py: usize) -> Option<u32> {
    pixel_index(width, x, py).and_then(|index| pixels.get(index).copied())
}

fn build_map_cache(origin_x: f64, width: usize, height: usize, unix_secs: u64) -> MapCache {
    let cell_width = VIEW_LON_SPAN * LON_SCALE / width as f64;
    let half_width = cell_width * 0.5;
    let row_span = LAT_MAX - LAT_MIN;
    let pixel_cols = width.saturating_mul(2);
    let pixel_rows = height.saturating_mul(2);
    let pixel_height = row_span * LAT_SCALE / pixel_rows.max(1) as f64;
    let start_y = (90.0 - LAT_MAX) * LAT_SCALE;
    let lon_tolerance = VIEW_LON_SPAN / pixel_cols.max(1) as f64 * 0.4;
    let lat_tolerance = row_span / pixel_rows.max(1) as f64 * 0.18;
    let (sun_lat, sun_lon) = subsolar_point(unix_secs);
    let mut pixels = vec![0; pixel_cols.saturating_mul(pixel_rows)];

    for py in 0..pixel_rows {
        let top = start_y + py as f64 * pixel_height;
        let lat = LAT_MAX - (py as f64 + 0.5) / pixel_rows as f64 * row_span;
        let on_parallel = near_grid(lat, 30.0, lat_tolerance);
        for px in 0..pixel_cols {
            let left = origin_x + px as f64 * half_width;
            let lon = wrap_lon((left + half_width * 0.5) / LON_SCALE - 180.0);
            let coverage =
                land_fraction(left, top, left + half_width, top + pixel_height).clamp(0.0, 1.0);
            let light = lighting_factor(lat, lon, sun_lat, sun_lon);
            if let Some(index) = pixel_index(pixel_cols, px, py) {
                if let Some(slot) = pixels.get_mut(index) {
                    *slot = sample_pixel(coverage, light, lon, on_parallel, lon_tolerance);
                }
            }
        }
    }

    MapCache {
        origin_x,
        width,
        height,
        sun_minute: unix_secs / 60,
        pixels,
    }
}

/// Paint the world map into `rect` of the offscreen frame.
///
/// The 32-bit window is cached in `cache` and only rebuilt when the fractional
/// origin, viewport geometry, or solar minute changes. Copying the window into
/// cells is what produces Unicode half-blocks.
pub(crate) fn paint_world_map(
    frame: &mut Frame,
    rect: Rect,
    cache: &mut Option<MapCache>,
    center_lon: f64,
    selected_zone: Option<&MapPoint>,
) {
    let width = usize::from(rect.width);
    let height = usize::from(rect.height);
    if width == 0 || height == 0 {
        return;
    }
    let unix_secs = map_unix_secs();
    let sun_minute = unix_secs / 60;
    let origin_x = map_origin_x(center_lon);
    let cache_matches = cache.as_ref().is_some_and(|cached| {
        same_origin(cached.origin_x, origin_x)
            && cached.width == width
            && cached.height == height
            && cached.sun_minute == sun_minute
    });
    if !cache_matches {
        *cache = Some(build_map_cache(origin_x, width, height, unix_secs));
    }
    let Some(cached) = cache.as_ref() else {
        return;
    };
    paint_map_cells(frame, rect, cached);
    draw_map_overlays(frame, rect, selected_zone, cached);
}

fn mono_glyph(pixel: u32) -> char {
    let color = snap_theme(pixel);
    if color == theme::LAND || color == theme::LAND_NIGHT {
        '#'
    } else if color == theme::COAST
        || color == theme::COAST_NIGHT
        || color == theme::GUIDE
        || color == theme::GUIDE_NIGHT
        || color == theme::ACCENT
    {
        '+'
    } else if color == theme::SEA_NIGHT {
        '.'
    } else {
        '~'
    }
}

fn mono_rank(glyph: char) -> u8 {
    match glyph {
        '#' => 4,
        '+' => 3,
        '*' => 2,
        '~' => 1,
        _ => 0,
    }
}

fn cell_quad(cache: &MapCache, x: usize, y: usize) -> Option<[u32; 4]> {
    let pixel_width = cache.pixel_width();
    let px = x.saturating_mul(2);
    let py = y.saturating_mul(2);
    let tl = pixel_at(&cache.pixels, pixel_width, px, py)?;
    let tr = pixel_at(&cache.pixels, pixel_width, px.saturating_add(1), py).unwrap_or(tl);
    let bl = pixel_at(&cache.pixels, pixel_width, px, py.saturating_add(1)).unwrap_or(tl);
    let br = pixel_at(
        &cache.pixels,
        pixel_width,
        px.saturating_add(1),
        py.saturating_add(1),
    )
    .unwrap_or(tr);
    Some([tl, tr, bl, br])
}

fn half_block(quad: [u32; 4]) -> (char, Option<Color>, Option<Color>) {
    let [tl, tr, bl, br] = quad;
    if color_mode() == ColorMode::Mono {
        let glyphs = [
            mono_glyph(tl),
            mono_glyph(tr),
            mono_glyph(bl),
            mono_glyph(br),
        ];
        let glyph = glyphs
            .into_iter()
            .max_by_key(|glyph| mono_rank(*glyph))
            .unwrap_or('~');
        return (glyph, None, None);
    }
    let top_left = snap_pixel(tl);
    let top_right = snap_pixel(tr);
    let bottom_left = snap_pixel(bl);
    let bottom_right = snap_pixel(br);
    if top_left == top_right && top_left == bottom_left && top_left == bottom_right {
        return ('█', Some(top_left), Some(top_left));
    }
    if top_left == top_right && bottom_left == bottom_right {
        return ('▀', Some(top_left), Some(bottom_left));
    }
    if top_left == bottom_left && top_right == bottom_right {
        return ('▌', Some(top_left), Some(top_right));
    }
    if bottom_left == bottom_right && top_left != top_right {
        return ('▄', Some(bottom_left), Some(top_left));
    }
    if top_right == bottom_right && top_left != bottom_left {
        return ('▐', Some(top_right), Some(top_left));
    }
    let vertical = u8::from(top_left == top_right) + u8::from(bottom_left == bottom_right);
    let horizontal = u8::from(top_left == bottom_left) + u8::from(top_right == bottom_right);
    if vertical >= horizontal {
        ('▀', Some(top_left), Some(bottom_left))
    } else {
        ('▌', Some(top_left), Some(top_right))
    }
}

fn paint_map_cells(frame: &mut Frame, rect: Rect, cache: &MapCache) {
    let mut buffer = frame.buffer_mut();
    for y in 0..cache.height {
        let Ok(row) = u16::try_from(y) else {
            break;
        };
        let abs_y = rect.y.saturating_add(row);
        for x in 0..cache.width {
            let Some(quad) = cell_quad(cache, x, y) else {
                continue;
            };
            let (glyph, fg, bg) = half_block(quad);
            let Ok(col) = u16::try_from(x) else {
                continue;
            };
            let abs_x = rect.x.saturating_add(col);
            if let Some(cell) = buffer.cell_mut((abs_x, abs_y)) {
                let mut style = Style::default();
                if let Some(fg) = fg {
                    style = style.fg(fg);
                }
                if let Some(bg) = bg {
                    style = style.bg(bg);
                }
                cell.set_char(glyph).set_style(style);
            }
        }
    }
}

/// Fractional cell projection of a geographic point in the current view.
#[derive(Clone, Copy)]
struct MapProj {
    cell_x: isize,
    cell_y: isize,
    /// Offset within the cell, in `[0, 1)`.
    frac_x: f64,
    frac_y: f64,
}

/// Axis-aligned occupancy used to avoid stacking city labels on top of each other.
#[derive(Clone, Copy)]
struct OccupiedRect {
    x0: isize,
    y0: isize,
    x1: isize,
    y1: isize,
}

impl OccupiedRect {
    fn new(x: isize, y: isize, width: usize, height: usize) -> Self {
        Self {
            x0: x,
            y0: y,
            x1: x + width as isize,
            y1: y + height as isize,
        }
    }

    fn intersects(self, other: Self) -> bool {
        self.x0 < other.x1 && other.x0 < self.x1 && self.y0 < other.y1 && other.y0 < self.y1
    }
}

fn cell_quad_colors(cache: &MapCache, x: isize, y: isize) -> [Color; 4] {
    let Ok(x) = usize::try_from(x) else {
        return [theme::SEA; 4];
    };
    let Ok(y) = usize::try_from(y) else {
        return [theme::SEA; 4];
    };
    let Some([tl, tr, bl, br]) = cell_quad(cache, x, y) else {
        return [theme::SEA; 4];
    };
    [
        snap_pixel(tl),
        snap_pixel(tr),
        snap_pixel(bl),
        snap_pixel(br),
    ]
}

fn cell_background(cache: &MapCache, x: isize, y: isize) -> Color {
    let Ok(x) = usize::try_from(x) else {
        return theme::SEA;
    };
    let Ok(y) = usize::try_from(y) else {
        return theme::SEA;
    };
    let Some(quad) = cell_quad(cache, x, y) else {
        return theme::SEA;
    };
    half_block(quad).2.unwrap_or(theme::SEA)
}

fn marker_background(cache: &MapCache, proj: MapProj, glyph: char) -> Color {
    let [tl, tr, bl, br] = cell_quad_colors(cache, proj.cell_x, proj.cell_y);
    match glyph {
        '\u{2584}' | '\u{2590}' => tl,
        '\u{258c}' => tr,
        '\u{2580}' => bl,
        _ => br,
    }
}

/// Pick a CP437 half-block so a marker can sit on a half-cell.
fn marker_glyph(frac_x: f64, frac_y: f64) -> char {
    let dx = frac_x - 0.5;
    let dy = frac_y - 0.5;
    if dx.abs() <= 0.25 && dy.abs() <= 0.25 {
        return '\u{2588}';
    }
    if dy.abs() >= dx.abs() {
        if dy < 0.0 { '\u{2580}' } else { '\u{2584}' }
    } else if dx < 0.0 {
        '\u{258c}'
    } else {
        '\u{2590}'
    }
}

fn put_marker(
    frame: &mut Frame,
    cache: &MapCache,
    rect: Rect,
    proj: MapProj,
    fg: Color,
    bold: bool,
) {
    let Ok(row) = u16::try_from(proj.cell_y) else {
        return;
    };
    let Ok(col) = u16::try_from(proj.cell_x) else {
        return;
    };
    let abs_y = rect.y.saturating_add(row);
    let abs_x = rect.x.saturating_add(col);
    let mono = color_mode() == ColorMode::Mono;
    let (glyph, style) = if mono {
        let glyph = if bold { 'X' } else { '*' };
        let style = if bold {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        (glyph, style)
    } else {
        let glyph = marker_glyph(proj.frac_x, proj.frac_y);
        let bg = marker_background(cache, proj, glyph);
        let mut style = Style::default().fg(fg).bg(bg);
        if bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        (glyph, style)
    };
    if let Some(cell) = frame.buffer_mut().cell_mut((abs_x, abs_y)) {
        cell.set_char(glyph).set_style(style);
    }
}

fn put_text(
    frame: &mut Frame,
    cache: &MapCache,
    rect: Rect,
    pos: (isize, isize),
    text: &str,
    fg: Color,
) {
    let (x, y) = pos;
    let Ok(row) = u16::try_from(y) else {
        return;
    };
    let abs_y = rect.y.saturating_add(row);
    let mut buffer = frame.buffer_mut();
    for (offset, ch) in text.chars().enumerate() {
        let local_x = x.saturating_add(offset as isize);
        let Ok(col) = u16::try_from(local_x) else {
            continue;
        };
        let abs_x = rect.x.saturating_add(col);
        if let Some(cell) = buffer.cell_mut((abs_x, abs_y)) {
            let style = if color_mode() == ColorMode::Mono {
                Style::default()
            } else {
                let cell_bg = cell_background(cache, local_x, y);
                Style::default().fg(fg).bg(cell_bg)
            };
            cell.set_char(ch).set_style(style);
        }
    }
}

#[allow(clippy::too_many_lines)]
fn draw_map_overlays(
    frame: &mut Frame,
    rect: Rect,
    selected_zone: Option<&MapPoint>,
    cache: &MapCache,
) {
    let width = cache.width;
    let height = cache.height;
    let mut occupied = Vec::new();

    // Reserve the selected pin first so city labels defer to it.
    let selected_proj = selected_zone.and_then(|zone| cache.project(zone.lat, zone.lon));
    if let Some(proj) = selected_proj {
        let (x, y) = (proj.cell_x, proj.cell_y);
        occupied.push(OccupiedRect::new(x, y, 1, 1));
        if let Some(zone) = selected_zone
            && !zone.label.is_empty()
        {
            // Leading space separates the pin from the city name.
            let label_width = zone.label.chars().count().saturating_add(1);
            if x + 1 < width as isize {
                let avail = (width as isize - (x + 1)) as usize;
                let take = label_width.min(avail);
                if take > 0 {
                    occupied.push(OccupiedRect::new(x + 1, y, take, 1));
                }
            }
        }
    }

    // Legend occupies the bottom-left strip when the map is large enough.
    let show_legend = width >= LEGEND_MIN_WIDTH && height >= LEGEND_MIN_HEIGHT;
    if show_legend {
        // Glyph key matching the overlays painted above the basemap.
        let legend = if color_mode() == ColorMode::Mono {
            "X pin  * city"
        } else {
            "\u{2588} pin  \u{2588} city"
        };
        let legend_width = legend.chars().count();
        if legend_width <= width {
            let y = y_local(height, height as isize - 1);
            occupied.push(OccupiedRect::new(0, y, legend_width, 1));
            put_text(frame, cache, rect, (0, y), legend, theme::MUTED);
        }
    }

    let show_cities = width >= CITY_LABEL_MIN_WIDTH && height >= CITY_LABEL_MIN_HEIGHT;
    if show_cities {
        // Density scales with map width so compact views stay sparse.
        let min_gap = if width >= 70 { 5 } else { 7 };
        for &(label, lat, lon) in CITY_LABELS {
            let Some(proj) = cache.project(lat, lon) else {
                continue;
            };
            let (x, y) = (proj.cell_x, proj.cell_y);
            // Require room for marker + full short name; skip rather than paint a bare mark.
            let text_width = label.chars().count().saturating_add(1) as isize;
            if x + text_width > width as isize {
                continue;
            }
            let placed = OccupiedRect::new(x, y, text_width as usize, 1);
            // Keep a soft gap around already-placed labels.
            let padded = OccupiedRect {
                x0: placed.x0 - min_gap as isize / 2,
                y0: placed.y0 - 1,
                x1: placed.x1 + min_gap as isize / 2,
                y1: placed.y1 + 1,
            };
            if occupied.iter().any(|other| other.intersects(padded)) {
                continue;
            }
            occupied.push(placed);
            put_marker(frame, cache, rect, proj, theme::ACCENT, false);
            put_text(frame, cache, rect, (x + 1, y), label, theme::ACCENT);
        }
    }

    // Selection pin is painted last so it always wins over city labels.
    if let (Some(zone), Some(proj)) = (selected_zone, selected_proj) {
        put_marker(frame, cache, rect, proj, theme::WARNING, true);
        let (x, y) = (proj.cell_x, proj.cell_y);
        if !zone.label.is_empty() && x + 1 < width as isize {
            let avail = (width as isize - (x + 1)) as usize;
            let mut label = String::from(" ");
            label.extend(zone.label.chars().take(avail.saturating_sub(1)));
            if label.chars().count() > 1 {
                put_text(frame, cache, rect, (x + 1, y), &label, theme::WARNING);
            }
        }
    }
}

/// Clamp a local y coordinate into the visible map band.
fn y_local(height: usize, value: isize) -> isize {
    let max = isize::try_from(height.saturating_sub(1)).unwrap_or(0);
    value.clamp(0, max)
}
