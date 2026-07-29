// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! Timezone selection and the iocraft world-map widget.

use std::sync::OnceLock;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use iocraft::prelude::*;

use crate::tui::message::Effect;
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
const VISIBLE_ZONE_ROWS: usize = 6;
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

#[derive(Clone, Debug)]
pub(crate) struct Zone {
    pub(crate) name: String,
    search_name: String,
    lat: f64,
    lon: f64,
    has_coord: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct MapPoint {
    lat: f64,
    lon: f64,
}

fn parse_zone_line(line: &str) -> Option<Zone> {
    let line = line.trim_end();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut columns = line.split('\t');
    let _codes = columns.next()?;
    let coord = columns.next()?;
    let name = columns.next()?.to_string();
    let search_name = name.to_ascii_lowercase();
    let coord =
        iso6709parse::parse_string_representation::<iso6709parse::ISO6709Coord>(coord).ok()?;
    Some(Zone {
        name,
        search_name,
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

pub(crate) fn visible_names(state: &AppState) -> Vec<(&'static str, bool)> {
    state
        .timezone
        .filtered
        .iter()
        .skip(state.timezone.top)
        .take(VISIBLE_ZONE_ROWS)
        .enumerate()
        .map(|(row, &index)| {
            (
                zones()[index].name.as_str(),
                state.timezone.top + row == state.timezone.selected,
            )
        })
        .collect()
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
        })
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

fn map_text_style(color: Color, weight: Weight, invert: bool) -> CanvasTextStyle {
    let mut style = CanvasTextStyle::default();
    style.color = Some(color);
    style.weight = weight;
    style.invert = invert;
    style
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

struct MapCache {
    start_column: i32,
    width: usize,
    height: usize,
    rows: Vec<MapRow>,
}

fn build_map_cache(start_column: i32, width: usize, height: usize) -> MapCache {
    let start_x = f64::from(start_column);
    let cell_width = VIEW_LON_SPAN * LON_SCALE / width as f64;
    let row_span = LAT_MAX - LAT_MIN;
    let half_height = row_span * LAT_SCALE / (height * 2) as f64;
    let start_y = (90.0 - LAT_MAX) * LAT_SCALE;
    let lon_tolerance = VIEW_LON_SPAN / width as f64 * 0.4;
    let lat_tolerance = row_span / height as f64 * 0.18;
    let mut rows = Vec::with_capacity(height);

    for y in 0..height {
        let top = start_y + (y * 2) as f64 * half_height;
        let lat = LAT_MAX - (y as f64 + 0.5) / height as f64 * row_span;
        let on_parallel = near_grid(lat, 30.0, lat_tolerance);
        let mut runs = Vec::new();
        let mut run_text = String::with_capacity(width * 3);
        let mut run_color = None;
        let mut run_start = 0;

        for x in 0..width {
            let left = start_x + x as f64 * cell_width;
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
            let (glyph, color) = if coverage <= 0.01 {
                let lon = wrap_lon((left + cell_width * 0.5) / LON_SCALE - 180.0);
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
        rows.push(MapRow {
            background: if y % 8 < 2 {
                theme::SEA_SHIMMER
            } else {
                theme::SEA
            },
            runs,
        });
    }

    MapCache {
        start_column,
        width,
        height,
        rows,
    }
}

#[with_layout_style_props]
#[derive(Default, Props)]
pub(crate) struct WorldMapProps {
    pub(crate) center_lon_milli: i32,
    pub(crate) selected_zone: Option<MapPoint>,
}

#[derive(Default)]
pub(crate) struct WorldMap {
    center_lon: f64,
    selected_zone: Option<MapPoint>,
    cache: Option<MapCache>,
}

impl Component for WorldMap {
    type Props<'a> = WorldMapProps;

    fn new(props: &Self::Props<'_>) -> Self {
        Self {
            center_lon: f64::from(props.center_lon_milli) / 1000.0,
            selected_zone: props.selected_zone,
            cache: None,
        }
    }

    fn update(
        &mut self,
        props: &mut Self::Props<'_>,
        _hooks: Hooks,
        updater: &mut ComponentUpdater,
    ) {
        self.center_lon = f64::from(props.center_lon_milli) / 1000.0;
        self.selected_zone = props.selected_zone;
        updater.set_layout_style(props.layout_style().into());
    }

    fn draw(&mut self, drawer: &mut ComponentDrawer<'_>) {
        let size = drawer.size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        let width = usize::from(size.width);
        let height = usize::from(size.height);
        let start_column =
            ((wrap_lon(self.center_lon - VIEW_LON_SPAN / 2.0) + 180.0) * LON_SCALE).round() as i32;
        let start_column = start_column.rem_euclid(MAP_W as i32);
        let cache_matches = self.cache.as_ref().is_some_and(|cache| {
            cache.start_column == start_column && cache.width == width && cache.height == height
        });
        if !cache_matches {
            self.cache = Some(build_map_cache(start_column, width, height));
        }

        let mut canvas = drawer.canvas();
        if let Some(cache) = self.cache.as_ref() {
            for (y, row) in cache.rows.iter().enumerate() {
                canvas.set_background_color(0, y as isize, width, 1, row.background);
                for run in &row.runs {
                    canvas.set_text(
                        run.x,
                        y as isize,
                        &run.text,
                        map_text_style(run.color, Weight::Normal, false),
                    );
                }
            }
        }

        let row_span = LAT_MAX - LAT_MIN;
        if let Some(zone) = self
            .selected_zone
            .filter(|zone| (LAT_MIN..=LAT_MAX).contains(&zone.lat))
        {
            let x = ((lon_delta(self.center_lon, zone.lon) + VIEW_LON_SPAN / 2.0) / VIEW_LON_SPAN
                * width as f64)
                .floor() as isize;
            let y = ((LAT_MAX - zone.lat) / row_span * height as f64).floor() as usize;
            let y = y.min(height.saturating_sub(1)) as isize;
            if x >= 0 && x < width as isize {
                canvas.set_text(
                    x,
                    y,
                    "X",
                    map_text_style(theme::WARNING, Weight::Bold, true),
                );
            }
        }
    }
}
