// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! Timezone selection and the world-map framebuffer painter.
//!
//! The map renders into the offscreen [`Frame`] cell buffer rather than a
//! widget tree: one cached row of glyph runs per visible line is painted
//! directly into the framebuffer the terminal blit diffs against. That keeps
//! panning a per-frame cost of microseconds, with nothing laid out or measured.

use std::sync::OnceLock;
use std::time::Duration;
#[cfg(not(test))]
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::tui::message::Effect;
use crate::tui::render::{Color, Frame, Modifier, Rect, Style};
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
const SHADES: [(char, f64, f64); 7] = [
    (' ', 0.00, 0.00),
    ('░', 0.25, 0.25),
    ('▒', 0.50, 0.50),
    ('▓', 0.75, 0.75),
    ('█', 1.00, 1.00),
    ('▀', 1.00, 0.00),
    ('▄', 0.00, 1.00),
];
const ZONE_TAB: &str = include_str!("zone1970.tab");
const WORLD_MASK: &[u8] = include_bytes!("world_mask.bin");
/// Solar elevation (degrees) above which a cell is fully day.
const DAY_ELEVATION_DEG: f64 = 0.0;
/// Half-width of the twilight band drawn along the terminator.
const TWILIGHT_HALF_WIDTH_DEG: f64 = 4.0;
/// Snapshot-stable solar clock: 2024-06-21 12:00:00 UTC (northern summer noon).
#[cfg(test)]
const SNAPSHOT_UNIX_SECS: u64 = 1_718_971_200;
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

/// Current unix seconds, pinned in tests for snapshot stability.
#[cfg(test)]
fn map_unix_secs() -> u64 {
    SNAPSHOT_UNIX_SECS
}

#[cfg(not(test))]
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

/// Day/night lighting class for a map cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lighting {
    Day,
    Twilight,
    Night,
}

fn lighting_at(lat: f64, lon: f64, sun_lat: f64, sun_lon: f64) -> Lighting {
    let elevation = solar_elevation_sin(lat, lon, sun_lat, sun_lon)
        .asin()
        .to_degrees();
    if elevation >= DAY_ELEVATION_DEG + TWILIGHT_HALF_WIDTH_DEG {
        Lighting::Day
    } else if elevation >= DAY_ELEVATION_DEG - TWILIGHT_HALF_WIDTH_DEG {
        Lighting::Twilight
    } else {
        Lighting::Night
    }
}

fn night_color(color: Color) -> Color {
    if color == theme::LAND {
        theme::LAND_NIGHT
    } else if color == theme::COAST {
        theme::COAST_NIGHT
    } else {
        // Guides, graticule accents, and empty sea glyphs all dim the same way.
        theme::GUIDE_NIGHT
    }
}

fn sea_background(y: usize, night: bool) -> Color {
    if night {
        if y % 8 < 2 {
            theme::SEA_NIGHT_SHIMMER
        } else {
            theme::SEA_NIGHT
        }
    } else if y % 8 < 2 {
        theme::SEA_SHIMMER
    } else {
        theme::SEA
    }
}

fn light_glyph_color(base: Color, lighting: Lighting) -> Color {
    match lighting {
        Lighting::Day => base,
        Lighting::Twilight => theme::TERMINATOR,
        Lighting::Night => night_color(base),
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
        center_lon: f64::from(state.timezone.map_center_lon_milli) / 1000.0,
        target_lon: f64::from(state.timezone.map_target_lon_milli) / 1000.0,
    }
}

fn store(state: &mut AppState, pick: &TzPick, view: &MapView) {
    state.timezone.filtered.clone_from(&pick.filtered);
    state.timezone.selected = pick.selected;
    state.timezone.top = pick.top;
    state.timezone.filter.clone_from(&pick.filter);
    state.timezone.map_center_lon_milli = (view.center_lon * 1000.0).round() as i32;
    state.timezone.map_target_lon_milli = (view.target_lon * 1000.0).round() as i32;
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

pub(crate) fn tick(state: &mut AppState, elapsed: Duration) -> bool {
    let mut view = reconstruct_view(state);
    view.tick(elapsed);
    let center_lon_milli = (view.center_lon * 1000.0).round() as i32;
    if center_lon_milli == state.timezone.map_center_lon_milli {
        return false;
    }
    state.timezone.map_center_lon_milli = center_lon_milli;
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

fn pick_glyph(top: f64, bottom: f64) -> char {
    SHADES
        .into_iter()
        .min_by(|(_, left_top, left_bottom), (_, right_top, right_bottom)| {
            let left_error = (left_top - top).powi(2) + (left_bottom - bottom).powi(2);
            let right_error = (right_top - top).powi(2) + (right_bottom - bottom).powi(2);
            left_error.total_cmp(&right_error)
        })
        .map_or(' ', |(glyph, _, _)| glyph)
}

fn near_grid(value: f64, spacing: f64, tolerance: f64) -> bool {
    (value - (value / spacing).round() * spacing).abs() <= tolerance
}

struct MapRun {
    x: isize,
    text: String,
    color: Color,
}

struct MapRow {
    background: Color,
    runs: Vec<MapRun>,
}

/// Cached projection of one map view: a row of glyph runs per visible line.
///
/// Rebuilt only when the viewport geometry or the solar minute changes; reused
/// across frames while the map is static, so a redraw costs only the framebuffer
/// paint, not the geographic sampling.
pub(crate) struct MapCache {
    start_column: i32,
    width: usize,
    height: usize,
    /// Unix minute used when the day/night terminator was baked in.
    sun_minute: u64,
    rows: Vec<MapRow>,
}

fn build_map_cache(start_column: i32, width: usize, height: usize, unix_secs: u64) -> MapCache {
    let start_x = f64::from(start_column);
    let cell_width = VIEW_LON_SPAN * LON_SCALE / width as f64;
    let row_span = LAT_MAX - LAT_MIN;
    let half_height = row_span * LAT_SCALE / (height * 2) as f64;
    let start_y = (90.0 - LAT_MAX) * LAT_SCALE;
    let lon_tolerance = VIEW_LON_SPAN / width as f64 * 0.4;
    let lat_tolerance = row_span / height as f64 * 0.18;
    let (sun_lat, sun_lon) = subsolar_point(unix_secs);
    let mut rows = Vec::with_capacity(height);

    for y in 0..height {
        let top = start_y + (y * 2) as f64 * half_height;
        let lat = LAT_MAX - (y as f64 + 0.5) / height as f64 * row_span;
        let on_parallel = near_grid(lat, 30.0, lat_tolerance);
        // Sample lighting at the row centre; sea background follows the row majority.
        let mut night_cells = 0_usize;
        let mut runs = Vec::new();
        let mut run_text = String::with_capacity(width * 3);
        let mut run_color = None;
        let mut run_start = 0;

        for x in 0..width {
            let left = start_x + x as f64 * cell_width;
            let lon = wrap_lon((left + cell_width * 0.5) / LON_SCALE - 180.0);
            let lighting = lighting_at(lat, lon, sun_lat, sun_lon);
            if lighting == Lighting::Night {
                night_cells += 1;
            }
            let top_fraction =
                land_fraction(left, top, left + cell_width, top + half_height).clamp(0.0, 1.0);
            let bottom_fraction = land_fraction(
                left,
                top + half_height,
                left + cell_width,
                top + half_height * 2.0,
            )
            .clamp(0.0, 1.0);
            let coverage = (top_fraction + bottom_fraction) * 0.5;
            let (mut glyph, base_color) = if coverage <= 0.01 {
                let on_meridian = near_grid(lon, 30.0, lon_tolerance);
                if on_parallel && on_meridian {
                    ('+', theme::ACCENT)
                } else if on_parallel {
                    ('─', theme::GUIDE)
                } else if on_meridian {
                    ('·', theme::GUIDE)
                } else {
                    (' ', theme::GUIDE)
                }
            } else if coverage >= 0.99 {
                (pick_glyph(top_fraction, bottom_fraction), theme::LAND)
            } else {
                (pick_glyph(top_fraction, bottom_fraction), theme::COAST)
            };
            // Open ocean is blank; force a mid-dot so the terminator stays visible.
            if lighting == Lighting::Twilight && glyph == ' ' {
                glyph = '·';
            }
            let color = light_glyph_color(base_color, lighting);

            if run_color.is_some_and(|current| current != color) {
                runs.push(MapRun {
                    x: run_start as isize,
                    text: std::mem::take(&mut run_text),
                    color: run_color.unwrap_or(theme::GUIDE),
                });
                run_start = x;
            }
            run_color = Some(color);
            run_text.push(glyph);
        }
        if !run_text.is_empty() {
            runs.push(MapRun {
                x: run_start as isize,
                text: run_text,
                color: run_color.unwrap_or(theme::GUIDE),
            });
        }
        let night_row = night_cells * 2 >= width;
        rows.push(MapRow {
            background: sea_background(y, night_row),
            runs,
        });
    }

    MapCache {
        start_column,
        width,
        height,
        sun_minute: unix_secs / 60,
        rows,
    }
}

/// Paint the world map into `rect` of the offscreen frame.
///
/// The projection is cached in `cache` and only rebuilt when the viewport
/// geometry or the solar minute changes, so panning costs a framebuffer blit
/// plus an occasional geographic resample rather than a full widget layout.
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
    let start_column =
        ((wrap_lon(center_lon - VIEW_LON_SPAN / 2.0) + 180.0) * LON_SCALE).round() as i32;
    let start_column = start_column.rem_euclid(MAP_W as i32);
    let cache_matches = cache.as_ref().is_some_and(|cached| {
        cached.start_column == start_column
            && cached.width == width
            && cached.height == height
            && cached.sun_minute == sun_minute
    });
    if !cache_matches {
        *cache = Some(build_map_cache(start_column, width, height, unix_secs));
    }
    let Some(cached) = cache.as_ref() else {
        return;
    };
    paint_map_rows(frame, rect, &cached.rows);
    draw_map_overlays(
        frame,
        rect,
        center_lon,
        width,
        height,
        selected_zone,
        &cached.rows,
    );
}

fn paint_map_rows(frame: &mut Frame, rect: Rect, rows: &[MapRow]) {
    let mut buffer = frame.buffer_mut();
    for (y, row) in rows.iter().enumerate() {
        let abs_y = rect.y.saturating_add(u16::try_from(y).unwrap_or(u16::MAX));
        for run in &row.runs {
            for (offset, ch) in run.text.chars().enumerate() {
                let local_x = u16::try_from(run.x.saturating_add(offset as isize)).unwrap_or(0);
                let abs_x = rect.x.saturating_add(local_x);
                if let Some(cell) = buffer.cell_mut((abs_x, abs_y)) {
                    cell.set_char(ch)
                        .set_style(Style::default().fg(run.color).bg(row.background));
                }
            }
        }
    }
}

/// Project a geographic point into map cell coordinates for the current view.
fn project_map_point(
    center_lon: f64,
    lat: f64,
    lon: f64,
    width: usize,
    height: usize,
) -> Option<(isize, isize)> {
    if !(LAT_MIN..=LAT_MAX).contains(&lat) {
        return None;
    }
    let x = ((lon_delta(center_lon, lon) + VIEW_LON_SPAN / 2.0) / VIEW_LON_SPAN * width as f64)
        .floor() as isize;
    let y = ((LAT_MAX - lat) / (LAT_MAX - LAT_MIN) * height as f64).floor() as usize;
    let y = y.min(height.saturating_sub(1)) as isize;
    if x < 0 || x >= width as isize {
        return None;
    }
    Some((x, y))
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

fn put_text(frame: &mut Frame, x: u16, y: u16, text: &str, fg: Color, bg: Color, bold: bool) {
    let mut buffer = frame.buffer_mut();
    let mut cursor_x = x;
    for ch in text.chars() {
        if let Some(cell) = buffer.cell_mut((cursor_x, y)) {
            let mut style = Style::default().fg(fg).bg(bg);
            if bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            cell.set_char(ch).set_style(style);
        }
        cursor_x = cursor_x.saturating_add(1);
    }
}

#[allow(clippy::too_many_lines)]
fn draw_map_overlays(
    frame: &mut Frame,
    rect: Rect,
    center_lon: f64,
    width: usize,
    height: usize,
    selected_zone: Option<&MapPoint>,
    rows: &[MapRow],
) {
    let mut occupied = Vec::new();

    // Reserve the selected pin first so city labels defer to it.
    let selected_cell = selected_zone
        .and_then(|zone| project_map_point(center_lon, zone.lat, zone.lon, width, height));
    if let Some((x, y)) = selected_cell {
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
        let legend = "X pin  * city  · dusk";
        let legend_width = legend.chars().count();
        if legend_width <= width {
            let y = y_local(height, height as isize - 1);
            occupied.push(OccupiedRect::new(0, y, legend_width, 1));
            let bg = rows[y as usize].background;
            put_text(
                frame,
                rect.x,
                rect.y.saturating_add(y as u16),
                legend,
                theme::MUTED,
                bg,
                false,
            );
        }
    }

    let show_cities = width >= CITY_LABEL_MIN_WIDTH && height >= CITY_LABEL_MIN_HEIGHT;
    if show_cities {
        // Density scales with map width so compact views stay sparse.
        let min_gap = if width >= 70 { 5 } else { 7 };
        for &(label, lat, lon) in CITY_LABELS {
            let Some((x, y)) = project_map_point(center_lon, lat, lon, width, height) else {
                continue;
            };
            // Require room for marker + full short name; skip rather than paint a bare "*".
            let text = format!("*{label}");
            let text_width = text.chars().count() as isize;
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
            let bg = rows[y as usize].background;
            put_text(
                frame,
                rect.x.saturating_add(x as u16),
                rect.y.saturating_add(y as u16),
                &text,
                theme::ACCENT,
                bg,
                false,
            );
        }
    }

    // Selection pin is painted last so it always wins over city labels.
    if let (Some(zone), Some((x, y))) = (selected_zone, selected_cell) {
        let bg = rows[y as usize].background;
        // Inverted highlight: the prior sea background becomes the glyph colour,
        // the warning colour fills the cell, the pin always stands out.
        put_text(
            frame,
            rect.x.saturating_add(x as u16),
            rect.y.saturating_add(y as u16),
            "X",
            bg,
            theme::WARNING,
            true,
        );
        if !zone.label.is_empty() && x + 1 < width as isize {
            let avail = (width as isize - (x + 1)) as usize;
            // Leading space keeps "X Helsinki" readable on the basemap.
            let mut label = String::from(" ");
            label.extend(zone.label.chars().take(avail.saturating_sub(1)));
            if label.chars().count() > 1 {
                put_text(
                    frame,
                    rect.x.saturating_add((x + 1) as u16),
                    rect.y.saturating_add(y as u16),
                    &label,
                    theme::WARNING,
                    bg,
                    true,
                );
            }
        }
    }
}

/// Clamp a local y coordinate into the visible map band.
fn y_local(height: usize, value: isize) -> isize {
    value.clamp(0, height as isize - 1)
}
