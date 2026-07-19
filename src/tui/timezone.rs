// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! Timezone selection and the iocraft world-map widget.

use std::sync::OnceLock;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use iocraft::prelude::*;

use crate::tui::message::Effect;
use crate::tui::state::{AppState, Route};

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
const MAP_W: usize = 720;
const MAP_H: usize = 360;
const ROW_BYTES: usize = MAP_W / 8;
const LON_SCALE: f64 = MAP_W as f64 / 360.0;
const LAT_SCALE: f64 = MAP_H as f64 / 180.0;
const ZONE_TAB: &str = include_str!("zone1970.tab");
const WORLD_MASK: &[u8] = include_bytes!("world_mask.bin");

#[derive(Clone, Debug)]
pub(crate) struct Zone {
    pub(crate) name: String,
    lat: f64,
    lon: f64,
    has_coord: bool,
}

fn parse_zone_line(line: &str) -> Option<Zone> {
    let line = line.trim_end();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut columns = line.split('\t');
    let _codes = columns.next()?;
    let coord = columns.next()?;
    let name = columns.next()?;
    let coord =
        iso6709parse::parse_string_representation::<iso6709parse::ISO6709Coord>(coord).ok()?;
    Some(Zone {
        name: name.to_string(),
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
        Self {
            filtered,
            selected,
            top: 0,
            filter: String::new(),
        }
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
            .filter(|(_, zone)| {
                needle.is_empty() || zone.name.to_ascii_lowercase().contains(&needle)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = retained
            .and_then(|index| self.filtered.iter().position(|&current| current == index))
            .unwrap_or(0);
        if self.selected >= self.filtered.len() && !self.filtered.is_empty() {
            self.selected = self.filtered.len() - 1;
        }
        self.top = self.top.min(self.selected);
    }

    fn move_by(&mut self, direction: isize) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected =
            (self.selected as isize + direction).rem_euclid(self.filtered.len() as isize) as usize;
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

pub(crate) fn tick(state: &mut AppState, elapsed: Duration) {
    let mut view = reconstruct_view(state);
    if view.tick(elapsed) {
        state.timezone.map_center_lon_milli = (view.center_lon * 1000.0).round() as i32;
    }
}

pub(crate) fn visible_names(state: &AppState) -> Vec<String> {
    let pick = reconstruct_pick(state);
    pick.filtered
        .iter()
        .skip(pick.top)
        .take(8)
        .enumerate()
        .map(|(row, &index)| {
            format!(
                "{} {}",
                if pick.top + row == pick.selected {
                    ">"
                } else {
                    " "
                },
                zones()[index].name
            )
        })
        .collect()
}

pub(crate) fn selected_zone(state: &AppState) -> Option<Zone> {
    reconstruct_pick(state).selected().cloned()
}

fn row_land_count(row: usize, start: usize, end: usize) -> u32 {
    if start >= end {
        return 0;
    }
    let row = &WORLD_MASK[row * ROW_BYTES..(row + 1) * ROW_BYTES];
    let first = start / 8;
    let last = (end - 1) / 8;
    let head = 0xffu8 >> (start % 8);
    let tail = 0xffu8 << (7 - ((end - 1) % 8));
    if first == last {
        return (row[first] & head & tail).count_ones();
    }
    (row[first] & head).count_ones()
        + row[first + 1..last]
            .iter()
            .map(|byte| byte.count_ones())
            .sum::<u32>()
        + (row[last] & tail).count_ones()
}

fn land_fraction(row: usize, start: usize, width: usize, rows: usize) -> f64 {
    let mut total = 0;
    for map_row in row..(row + rows).min(MAP_H) {
        let end = start + width;
        total += if end <= MAP_W {
            row_land_count(map_row, start, end)
        } else {
            row_land_count(map_row, start, MAP_W) + row_land_count(map_row, 0, end - MAP_W)
        };
    }
    f64::from(total) / (width * rows) as f64
}

fn map_text_style(color: Color, weight: Weight) -> CanvasTextStyle {
    let mut style = CanvasTextStyle::default();
    style.color = Some(color);
    style.weight = weight;
    style
}

#[with_layout_style_props]
#[derive(Default, Props)]
pub(crate) struct WorldMapProps {
    pub(crate) center_lon_milli: i32,
    pub(crate) selected_zone: Option<Zone>,
}

#[derive(Default)]
pub(crate) struct WorldMap {
    center_lon: f64,
    selected_zone: Option<Zone>,
}

impl Component for WorldMap {
    type Props<'a> = WorldMapProps;

    fn new(props: &Self::Props<'_>) -> Self {
        Self {
            center_lon: f64::from(props.center_lon_milli) / 1000.0,
            selected_zone: props.selected_zone.clone(),
        }
    }

    fn update(
        &mut self,
        props: &mut Self::Props<'_>,
        _hooks: Hooks,
        updater: &mut ComponentUpdater,
    ) {
        self.center_lon = f64::from(props.center_lon_milli) / 1000.0;
        self.selected_zone.clone_from(&props.selected_zone);
        updater.set_layout_style(props.layout_style().into());
    }

    fn draw(&mut self, drawer: &mut ComponentDrawer<'_>) {
        let size = drawer.size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        let width = usize::from(size.width);
        let height = usize::from(size.height);
        let start_lon = wrap_lon(self.center_lon - VIEW_LON_SPAN / 2.0);
        let lon_step = VIEW_LON_SPAN / width as f64;
        let row_span = LAT_MAX - LAT_MIN;
        let mut canvas = drawer.canvas();
        for y in 0..height {
            let sea = if y % 6 < 2 {
                Color::Blue
            } else {
                Color::DarkBlue
            };
            let top_lat = LAT_MAX - y as f64 / height as f64 * row_span;
            let raster_y = ((90.0 - top_lat) * LAT_SCALE).round() as usize;
            let raster_rows = ((row_span / height as f64) * LAT_SCALE).round().max(1.0) as usize;
            for x in 0..width {
                let lon = wrap_lon(start_lon + (x as f64 + 0.5) * lon_step);
                let raster_x = ((lon + 180.0) * LON_SCALE).round() as usize % MAP_W;
                let raster_width = (lon_step * LON_SCALE).round().max(1.0) as usize;
                let fraction = land_fraction(raster_y, raster_x, raster_width, raster_rows);
                let (glyph, color) = if fraction > 0.7 {
                    ('█', Color::Green)
                } else if fraction > 0.15 {
                    ('▓', Color::Cyan)
                } else {
                    (' ', Color::Grey)
                };
                canvas.set_background_color(x as isize, y as isize, 1, 1, sea);
                canvas.set_text(
                    x as isize,
                    y as isize,
                    &glyph.to_string(),
                    map_text_style(color, Weight::Normal),
                );
            }
        }
        if let Some(zone) = self.selected_zone.as_ref().filter(|zone| zone.has_coord) {
            let x = ((lon_delta(self.center_lon, zone.lon) + VIEW_LON_SPAN / 2.0) / VIEW_LON_SPAN
                * width as f64)
                .floor() as isize;
            let y = ((LAT_MAX - zone.lat.clamp(LAT_MIN, LAT_MAX)) / row_span * height as f64)
                .floor() as isize;
            if x >= 0 && x < width as isize && y >= 0 && y < height as isize {
                canvas.set_background_color(x, y, 1, 1, Color::Magenta);
                canvas.set_text(x, y, "X", map_text_style(Color::Black, Weight::Bold));
            }
        }
    }
}
