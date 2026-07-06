// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::prelude::*;
use ratatui::symbols::Marker;
use ratatui::widgets::Paragraph;
use ratatui::widgets::canvas::{Canvas, Circle, Map, MapResolution, Points};

use crate::engine::util::terminal_is_utf8;
use crate::tui::app::{Installer, ScreenAction};
use crate::tui::screens::{ACCENT, BG, ERR, WARN, highlight_style};

const TICK: Duration = Duration::from_millis(80);
/// Fraction of the remaining longitude gap closed each tick when following a selection.
const EASE: f64 = 0.2;
/// Longitude advanced per tick while idle (~7.5°/s at the 80 ms tick).
const IDLE_DRIFT_DEG: f64 = 0.6;
/// Idle period after the last keypress before the map starts drifting.
const IDLE_AFTER: Duration = Duration::from_secs(3);
/// Quiet period after the last keypress before the UTC offset is (re)computed.
const OFFSET_DEBOUNCE: Duration = Duration::from_millis(250);

/// A selectable timezone with its representative location.
#[derive(Clone)]
pub(crate) struct Zone {
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    pub has_coord: bool,
}

/// Wrap a longitude into `[-180, 180)`.
fn wrap_deg(x: f64) -> f64 {
    (x + 180.0).rem_euclid(360.0) - 180.0
}

/// IANA tz database `zone1970.tab`, vendored verbatim (tzdb release 2026b).
/// See <https://data.iana.org/time-zones/>.
const ZONE_TAB: &str = include_str!("zone1970.tab");

/// Parse one `zone1970.tab` data row (`codes \t coord \t name \t comment`) into a
/// `Zone`. Returns `None` for comment/blank/short lines. The coordinate is a
/// compact ISO 6709 pair (`±DDMM±DDDMM` / `±DDMMSS±DDDMMSS`).
fn parse_zone_line(line: &str) -> Option<Zone> {
    let line = line.trim_end();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut cols = line.split('\t');
    let _codes = cols.next()?;
    let coord = cols.next()?;
    let name = cols.next()?;
    let c = iso6709parse::parse_string_representation::<iso6709parse::ISO6709Coord>(coord).ok()?;
    Some(Zone {
        name: name.to_string(),
        lat: c.lat,
        lon: c.lon,
        has_coord: true,
    })
}

/// Build the timezone list from the embedded `zone1970.tab`. `UTC` (absent from
/// tzdata's zone table) is injected and pinned first; the rest are sorted A-to-Z
/// by name (the table itself is grouped by country code, not name).
pub(crate) fn load_zones() -> Vec<Zone> {
    let mut rest: Vec<Zone> = ZONE_TAB.lines().filter_map(parse_zone_line).collect();
    rest.sort_by(|a, b| a.name.cmp(&b.name));
    let mut zones = Vec::with_capacity(rest.len() + 1);
    zones.push(Zone {
        name: "UTC".to_string(),
        lat: 0.0,
        lon: 0.0,
        has_coord: false,
    });
    zones.extend(rest);
    zones
}

/// Choose a canvas marker the terminal can actually render. The Linux VT console
/// font has no braille glyphs, so it must fall back to half blocks.
fn detect_map_marker() -> Marker {
    if std::env::var("TERM").as_deref() == Ok("linux") {
        return Marker::HalfBlock;
    }
    if terminal_is_utf8() {
        Marker::Braille
    } else {
        Marker::HalfBlock
    }
}

/// Format a `date +%z` value like `+0200` as `UTC+2` / `UTC+5:30` (empty on failure).
fn format_offset(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    if bytes.len() != 5 {
        return None;
    }
    let sign = raw.as_bytes()[0] as char;
    if sign != '+' && sign != '-' {
        return None;
    }
    let hours: i32 = raw.get(1..3)?.parse().ok()?;
    let minutes: i32 = raw.get(3..5)?.parse().ok()?;
    if hours == 0 && minutes == 0 {
        return Some("UTC".to_string());
    }
    if minutes == 0 {
        Some(format!("UTC{sign}{hours}"))
    } else {
        Some(format!("UTC{sign}{hours}:{minutes:02}"))
    }
}

/// Ask the system `date` for a zone's current UTC offset. Cosmetic; empty if
/// `date` is missing or fails.
fn tz_offset(name: &str) -> String {
    std::process::Command::new("date")
        .env("TZ", name)
        .arg("+%z")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| format_offset(String::from_utf8_lossy(&o.stdout).trim()))
        .unwrap_or_default()
}

/// Vertical layout of the timezone screen: `(map_h, filter_y, list_top, list_h)`.
fn layout(height: u16, use_map: bool) -> (u16, u16, u16, u16) {
    let bottom = height.saturating_sub(1); // footer row; content occupies rows 2..bottom
    if use_map {
        // Keep at least a filter row plus three list rows below the map band.
        let max_map = bottom.saturating_sub(2 + 1 + 3);
        let map_h = 7.min(max_map);
        if map_h > 0 {
            let filter_y = 2 + map_h;
            let list_top = filter_y + 1;
            return (
                map_h,
                filter_y,
                list_top,
                bottom.saturating_sub(list_top).max(1),
            );
        }
    }
    (0, 2, 3, bottom.saturating_sub(3).max(1))
}

/// The scrollable, type-to-filter picker state. `sel`/`top` index into `filtered`.
struct TzPick {
    all: Vec<Zone>,
    filter: String,
    filtered: Vec<usize>,
    sel: usize,
    top: usize,
    lon_offset: f64,
    last_input: Instant,
    offset_zone: String,
    offset_str: String,
}

impl TzPick {
    fn new(all: Vec<Zone>, current: &str) -> Self {
        let filtered: Vec<usize> = (0..all.len()).collect();
        let sel = filtered
            .iter()
            .position(|&i| all[i].name == current)
            .unwrap_or(0);
        let lon_offset = filtered
            .get(sel)
            .map_or(0.0, |&i| if all[i].has_coord { all[i].lon } else { 0.0 });
        Self {
            all,
            filter: String::new(),
            filtered,
            sel,
            top: 0,
            lon_offset,
            last_input: Instant::now(),
            offset_zone: String::new(),
            offset_str: String::new(),
        }
    }

    fn selected(&self) -> Option<&Zone> {
        self.filtered.get(self.sel).map(|&i| &self.all[i])
    }

    fn refilter(&mut self) {
        let needle = self.filter.to_ascii_lowercase();
        let keep = self.filtered.get(self.sel).copied();
        self.filtered = self
            .all
            .iter()
            .enumerate()
            .filter(|(_, z)| needle.is_empty() || z.name.to_ascii_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        self.sel = keep
            .and_then(|k| self.filtered.iter().position(|&i| i == k))
            .unwrap_or(0);
        if self.filtered.is_empty() {
            self.sel = 0;
        } else if self.sel >= self.filtered.len() {
            self.sel = self.filtered.len() - 1;
        }
        self.top = self.top.min(self.sel);
    }

    fn move_up(&mut self) {
        if !self.filtered.is_empty() {
            self.sel = self.sel.checked_sub(1).unwrap_or(self.filtered.len() - 1);
            self.last_input = Instant::now();
        }
    }

    fn move_down(&mut self) {
        if !self.filtered.is_empty() {
            self.sel = (self.sel + 1) % self.filtered.len();
            self.last_input = Instant::now();
        }
    }

    fn push_filter(&mut self, c: char) {
        self.filter.push(c);
        self.refilter();
        self.last_input = Instant::now();
    }

    fn backspace(&mut self) {
        self.filter.pop();
        self.refilter();
        self.last_input = Instant::now();
    }

    /// Keep the selected row inside a `visible`-row window.
    fn reframe(&mut self, visible: usize) {
        if self.filtered.is_empty() || visible == 0 {
            self.top = 0;
            return;
        }
        self.top = self.top.min(self.sel);
        if self.sel >= self.top + visible {
            self.top = self.sel + 1 - visible;
        }
    }

    fn on_tick(&mut self, now: Instant) {
        if now.duration_since(self.last_input) > IDLE_AFTER {
            self.lon_offset = wrap_deg(self.lon_offset + IDLE_DRIFT_DEG);
        } else if let Some(&i) = self.filtered.get(self.sel) {
            if self.all[i].has_coord {
                let gap = wrap_deg(self.all[i].lon - self.lon_offset);
                self.lon_offset = wrap_deg(self.lon_offset + gap * EASE);
            }
        }
        if now.duration_since(self.last_input) > OFFSET_DEBOUNCE {
            self.update_offset();
        }
    }

    fn update_offset(&mut self) {
        let Some(name) = self.selected().map(|z| z.name.clone()) else {
            return;
        };
        if name != self.offset_zone {
            self.offset_str = tz_offset(&name);
            self.offset_zone = name;
        }
    }
}

/// Draw the scrolling world map into `rect`, tiling two offset windows so the
/// date-line seam never leaves a blank edge.
fn draw_map(f: &mut Frame, pick: &TzPick, marker: Marker, rect: Rect) {
    let (has, lon, lat) = match pick.selected() {
        Some(z) if z.has_coord => (true, z.lon, z.lat),
        _ => (false, 0.0, 0.0),
    };
    let (x_min, x_max) = (pick.lon_offset - 180.0, pick.lon_offset + 180.0);
    let mut windows: Vec<[f64; 2]> = Vec::new();
    if x_min < -180.0 {
        windows.push([x_min, x_max]);
        windows.push([x_max, x_max + 360.0]);
    } else if x_max > 180.0 {
        windows.push([-360.0 + x_min, x_min]);
        windows.push([x_min, x_max]);
    } else {
        windows.push([x_min, x_max]);
    }

    for bounds in windows {
        let canvas = Canvas::default()
            .marker(marker)
            .background_color(BG)
            .x_bounds(bounds)
            .y_bounds([-60.0, 78.0])
            .paint(move |ctx| {
                ctx.draw(&Map {
                    resolution: MapResolution::High,
                    color: ACCENT,
                });
                if has {
                    ctx.layer();
                    let dot = [(lon, lat)];
                    ctx.draw(&Points {
                        coords: &dot,
                        color: WARN,
                    });
                    ctx.draw(&Circle {
                        x: lon,
                        y: lat,
                        radius: 6.0,
                        color: WARN,
                    });
                }
            });
        f.render_widget(canvas, rect);
    }
}

/// Render the whole timezone screen (map band, filter line, scroll list).
fn draw_timezone(f: &mut Frame, pick: &TzPick, marker: Marker, use_map: bool, subtitle: &str) {
    Installer::draw_frame(
        f,
        "Select Time Zone",
        subtitle,
        "Type to filter, Up/Down select, Enter confirm, Esc back",
    );
    let area = f.area();
    let (map_h, filter_y, list_top, list_h) = layout(area.height, use_map);

    if use_map && map_h > 0 {
        draw_map(f, pick, marker, Rect::new(0, 2, area.width, map_h));
    }

    f.render_widget(
        Paragraph::new(format!(" Filter: {}\u{2588}", pick.filter))
            .style(Style::default().fg(ACCENT)),
        Rect::new(0, filter_y, area.width, 1),
    );

    if pick.filtered.is_empty() {
        f.render_widget(
            Paragraph::new("  No matching time zones").style(Style::default().fg(ERR)),
            Rect::new(0, list_top, area.width, 1),
        );
        return;
    }

    let visible = list_h as usize;
    let end = (pick.top + visible).min(pick.filtered.len());
    for (row, &idx) in pick.filtered[pick.top..end].iter().enumerate() {
        let i = pick.top + row;
        let is_sel = i == pick.sel;
        let marker_ch = if is_sel { ">" } else { " " };
        f.render_widget(
            Paragraph::new(format!("{marker_ch} {}", pick.all[idx].name))
                .style(highlight_style(is_sel)),
            Rect::new(0, list_top + row as u16, area.width, 1),
        );
    }
}

impl Installer {
    #[allow(clippy::too_many_lines)]
    pub(crate) fn timezone_screen(
        &mut self,
        term: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> ScreenAction {
        let marker = detect_map_marker();
        let use_map = terminal_is_utf8();
        let current = self.ctx.config.identity.timezone.clone();
        let mut pick = TzPick::new(load_zones(), &current);
        pick.update_offset();

        let mut last_tick = Instant::now();

        loop {
            let height = term.size().map_or(24, |s| s.height);
            let (_, _, _, list_h) = layout(height, use_map);
            pick.reframe(list_h as usize);

            let subtitle = match pick.selected() {
                Some(z) if pick.offset_str.is_empty() => z.name.clone(),
                Some(z) => format!("{}   {}", z.name, pick.offset_str),
                None => String::new(),
            };

            term.draw(|f| draw_timezone(f, &pick, marker, use_map, &subtitle))
                .ok();

            let timeout = TICK.saturating_sub(last_tick.elapsed());
            if event::poll(timeout).unwrap_or(false) {
                if let Ok(Event::Key(key)) = event::read() {
                    match key.code {
                        KeyCode::Esc => return ScreenAction::Back,
                        KeyCode::Enter => {
                            if let Some(z) = pick.selected() {
                                self.ctx.config.identity.timezone.clone_from(&z.name);
                                return ScreenAction::Continue;
                            }
                        }
                        KeyCode::Up => pick.move_up(),
                        KeyCode::Down => pick.move_down(),
                        KeyCode::Backspace => pick.backspace(),
                        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            pick.filter.clear();
                            pick.refilter();
                            pick.last_input = Instant::now();
                        }
                        KeyCode::Char(c)
                            if c.is_ascii()
                                && c >= ' '
                                && !key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            pick.push_filter(c);
                        }
                        _ => {}
                    }
                }
            }

            if last_tick.elapsed() >= TICK {
                pick.on_tick(Instant::now());
                last_tick = Instant::now();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ZONE_TAB, load_zones};

    #[test]
    fn zones_load_from_embedded_tab() {
        let zones = load_zones();

        // UTC is synthesized and pinned first.
        assert_eq!(zones[0].name, "UTC");
        assert!(!zones[0].has_coord);

        // Every data row (codes/coord/name) must decode; a coord that fails to
        // parse would be silently dropped and this count mismatch catches it.
        let data_rows = ZONE_TAB
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.is_empty() && !l.starts_with('#') && l.split('\t').count() >= 3)
            .count();
        assert_eq!(
            zones.len(),
            data_rows + 1,
            "a zone1970.tab coordinate failed to parse"
        );
        assert!(data_rows > 300, "vendored zone1970.tab looks truncated");

        // The tail (everything after UTC) is sorted ascending by name.
        assert!(zones[1..].windows(2).all(|w| w[0].name <= w[1].name));

        // Spot-check representative coordinates against tzdata.
        let coord = |name: &str| {
            let z = zones.iter().find(|z| z.name == name).expect("zone present");
            (z.lat, z.lon)
        };
        let (lat, lon) = coord("Europe/Helsinki"); // +6010+02458
        assert!((lat - 60.1667).abs() < 0.01 && (lon - 24.9667).abs() < 0.01);
        let (lat, lon) = coord("America/New_York"); // +404251-0740023
        assert!((lat - 40.7142).abs() < 0.01 && (lon + 74.0064).abs() < 0.01);
    }
}
