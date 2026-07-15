// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! Retained render tree.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::tui::render::{Block, Frame, Paragraph, Rect, Size, Style};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct NodeId(String);

impl NodeId {
    pub(crate) fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Direction {
    Row,
    #[default]
    Column,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[expect(
    dead_code,
    reason = "reserved for non-stretch alignment in future route components"
)]
pub(crate) enum Align {
    #[default]
    Stretch,
    Start,
    Center,
    End,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[expect(dead_code, reason = "reserved for route component distribution")]
pub(crate) enum Justify {
    #[default]
    Start,
    Center,
    End,
    SpaceBetween,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Position {
    #[default]
    Flow,
    Absolute,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[expect(
    dead_code,
    reason = "reserved for overflow-visible rendering in future route components"
)]
pub(crate) enum Overflow {
    #[default]
    Clip,
    Visible,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Length {
    #[default]
    Auto,
    Cells(u16),
    Percent(u8),
    Fill(u16),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Edges {
    pub(crate) top: u16,
    pub(crate) right: u16,
    pub(crate) bottom: u16,
    pub(crate) left: u16,
}

impl Edges {
    pub(crate) const fn all(value: u16) -> Self {
        Self {
            top: value,
            right: value,
            bottom: value,
            left: value,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LayoutStyle {
    pub(crate) direction: Direction,
    pub(crate) width: Length,
    pub(crate) height: Length,
    pub(crate) min_width: u16,
    pub(crate) min_height: u16,
    pub(crate) max_width: Option<u16>,
    pub(crate) max_height: Option<u16>,
    pub(crate) padding: Edges,
    pub(crate) margin: Edges,
    pub(crate) gap: u16,
    pub(crate) align: Align,
    pub(crate) justify: Justify,
    pub(crate) flex_grow: u16,
    pub(crate) flex_shrink: u16,
    pub(crate) position: Position,
    pub(crate) inset: Edges,
    pub(crate) overflow: Overflow,
}

impl Default for LayoutStyle {
    fn default() -> Self {
        Self {
            direction: Direction::Column,
            width: Length::Auto,
            height: Length::Auto,
            min_width: 0,
            min_height: 0,
            max_width: None,
            max_height: None,
            padding: Edges::default(),
            margin: Edges::default(),
            gap: 0,
            align: Align::Stretch,
            justify: Justify::Start,
            flex_grow: 0,
            flex_shrink: 1,
            position: Position::Flow,
            inset: Edges::default(),
            overflow: Overflow::Clip,
        }
    }
}

impl LayoutStyle {
    fn same_geometry(self, other: Self) -> bool {
        Self {
            overflow: Overflow::Clip,
            ..self
        } == Self {
            overflow: Overflow::Clip,
            ..other
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ViewKind {
    Box,
    Text(String),
}

#[derive(Clone, Debug)]
pub(crate) struct ViewNode {
    pub(crate) id: NodeId,
    pub(crate) kind: ViewKind,
    pub(crate) layout: LayoutStyle,
    pub(crate) style: Style,
    pub(crate) visible: bool,
    pub(crate) focusable: bool,
    pub(crate) children: Vec<Self>,
}

impl ViewNode {
    pub(crate) fn box_(id: impl Into<String>) -> Self {
        Self {
            id: NodeId::new(id),
            kind: ViewKind::Box,
            layout: LayoutStyle::default(),
            style: Style::default(),
            visible: true,
            focusable: false,
            children: Vec::new(),
        }
    }

    pub(crate) fn text(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            kind: ViewKind::Text(content.into()),
            ..Self::box_(id)
        }
    }

    pub(crate) fn layout(mut self, layout: LayoutStyle) -> Self {
        self.layout = layout;
        self
    }

    pub(crate) fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    #[expect(dead_code, reason = "reserved for the centralized input router migration")]
    pub(crate) fn focusable(mut self, focusable: bool) -> Self {
        self.focusable = focusable;
        self
    }

    pub(crate) fn children(mut self, children: impl IntoIterator<Item = Self>) -> Self {
        self.children = children.into_iter().collect();
        self
    }
}

#[derive(Clone, Debug)]
struct RenderNode {
    id: NodeId,
    kind: ViewKind,
    layout: LayoutStyle,
    style: Style,
    visible: bool,
    focusable: bool,
    rect: Rect,
    children: Vec<Self>,
    measured: Size,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RenderCommand {
    PushClip(Rect),
    PaintBox {
        rect: Rect,
        style: Style,
    },
    PaintText {
        rect: Rect,
        content: String,
        style: Style,
    },
    PopClip,
}

#[derive(Clone, Copy, Debug, Default)]
struct Dirty {
    layout: bool,
    tree: bool,
}

impl RenderNode {
    fn from_view(view: ViewNode) -> Self {
        let mut node = Self {
            id: view.id,
            kind: view.kind,
            layout: view.layout,
            style: view.style,
            visible: view.visible,
            focusable: view.focusable,
            rect: Rect::new(0, 0, 0, 0),
            children: view.children.into_iter().map(Self::from_view).collect(),
            measured: Size {
                width: 0,
                height: 0,
            },
        };
        node.measured = measure_node(&node);
        node
    }

    fn reconcile(&mut self, view: ViewNode) -> Dirty {
        let next_ids: Vec<NodeId> = view.children.iter().map(|child| child.id.clone()).collect();
        let current_ids: Vec<NodeId> = self.children.iter().map(|child| child.id.clone()).collect();
        let structure_changed = current_ids != next_ids;
        let kind_changed = self.kind != view.kind;
        let mut dirty = Dirty {
            layout: !self.layout.same_geometry(view.layout)
                || self.visible != view.visible
                || structure_changed,
            tree: kind_changed
                || self.layout != view.layout
                || self.style != view.style
                || self.visible != view.visible
                || structure_changed,
        };
        let previous_measure = self.measured;

        self.kind = view.kind;
        self.layout = view.layout;
        self.style = view.style;
        self.visible = view.visible;
        self.focusable = view.focusable;

        let mut old: HashMap<NodeId, Self> = self
            .children
            .drain(..)
            .map(|child| (child.id.clone(), child))
            .collect();
        self.children = view
            .children
            .into_iter()
            .map(|child| {
                if let Some(mut retained) = old.remove(&child.id) {
                    let child_dirty = retained.reconcile(child);
                    dirty.layout |= child_dirty.layout;
                    dirty.tree |= child_dirty.tree;
                    retained
                } else {
                    dirty.layout = true;
                    dirty.tree = true;
                    Self::from_view(child)
                }
            })
            .collect();
        if !old.is_empty() {
            dirty.layout = true;
            dirty.tree = true;
        }

        if kind_changed || matches!(&self.kind, ViewKind::Box) {
            self.measured = measure_node(self);
        }
        dirty.layout |= (self.layout.width == Length::Auto
            && previous_measure.width != self.measured.width)
            || (self.layout.height == Length::Auto
                && previous_measure.height != self.measured.height);
        dirty
    }

    fn collect_focusable(&self, ids: &mut Vec<NodeId>) {
        if !self.visible {
            return;
        }
        if self.focusable {
            ids.push(self.id.clone());
        }
        for child in &self.children {
            child.collect_focusable(ids);
        }
    }

    fn contains_id(&self, id: &NodeId) -> bool {
        self.id == *id || self.children.iter().any(|child| child.contains_id(id))
    }
}

pub(crate) struct RenderTree {
    root: Option<RenderNode>,
    focused: Option<NodeId>,
    layout_dirty: bool,
    last_area: Option<Rect>,
    tree_generation: u64,
    layout_generation: u64,
    commands_tree_generation: u64,
    commands_layout_generation: u64,
    commands: Vec<RenderCommand>,
}

impl RenderTree {
    pub(crate) const fn new() -> Self {
        Self {
            root: None,
            focused: None,
            layout_dirty: true,
            last_area: None,
            tree_generation: 0,
            layout_generation: 0,
            commands_tree_generation: u64::MAX,
            commands_layout_generation: u64::MAX,
            commands: Vec::new(),
        }
    }

    pub(crate) fn reconcile(&mut self, view: ViewNode) {
        let dirty = match self.root.as_mut() {
            Some(root) if root.id == view.id => root.reconcile(view),
            _ => {
                self.root = Some(RenderNode::from_view(view));
                Dirty {
                    layout: true,
                    tree: true,
                }
            }
        };
        self.layout_dirty |= dirty.layout;
        if dirty.tree {
            self.tree_generation = self.tree_generation.wrapping_add(1);
        }
        if self
            .focused
            .as_ref()
            .is_some_and(|id| !self.root.as_ref().is_some_and(|root| root.contains_id(id)))
        {
            self.focused = None;
        }
    }

    pub(crate) fn render(&mut self, frame: &mut Frame) {
        self.render_in(frame, frame.area());
    }

    pub(crate) fn render_in(&mut self, frame: &mut Frame, area: Rect) {
        let Some(root) = self.root.as_mut() else {
            return;
        };
        self.layout_dirty |= self.last_area != Some(area);
        if self.layout_dirty {
            let root_rect = resolved_rect(root, area);
            layout_node(root, root_rect);
            self.layout_dirty = false;
            self.last_area = Some(area);
            self.layout_generation = self.layout_generation.wrapping_add(1);
        }
        if self.commands_tree_generation != self.tree_generation
            || self.commands_layout_generation != self.layout_generation
        {
            self.commands.clear();
            flatten_render_list(root, area, &mut self.commands);
            self.commands_tree_generation = self.tree_generation;
            self.commands_layout_generation = self.layout_generation;
        }
        paint_commands(&self.commands, frame, area);
    }

    #[expect(dead_code, reason = "reserved for the centralized input router migration")]
    pub(crate) fn focus_next(&mut self) {
        self.move_focus(1);
    }

    #[expect(dead_code, reason = "reserved for the centralized input router migration")]
    pub(crate) fn focus_previous(&mut self) {
        self.move_focus(-1);
    }

    fn move_focus(&mut self, delta: isize) {
        let mut ids = Vec::new();
        if let Some(root) = &self.root {
            root.collect_focusable(&mut ids);
        }
        if ids.is_empty() {
            self.focused = None;
            return;
        }
        let current = self
            .focused
            .as_ref()
            .and_then(|focused| ids.iter().position(|id| id == focused));
        let next = match (current, delta) {
            (Some(index), d) if d < 0 => index.checked_sub(1).unwrap_or(ids.len() - 1),
            (Some(index), _) => (index + 1) % ids.len(),
            (None, d) if d < 0 => ids.len() - 1,
            (None, _) => 0,
        };
        self.focused = Some(ids[next].clone());
    }

    #[expect(dead_code, reason = "reserved for the centralized input router migration")]
    pub(crate) fn focused(&self) -> Option<&NodeId> {
        self.focused.as_ref()
    }
}

fn inset(rect: Rect, edges: Edges) -> Rect {
    let x = rect
        .x
        .saturating_add(edges.left)
        .min(rect.x.saturating_add(rect.width));
    let y = rect
        .y
        .saturating_add(edges.top)
        .min(rect.y.saturating_add(rect.height));
    let width = rect
        .width
        .saturating_sub(edges.left.saturating_add(edges.right));
    let height = rect
        .height
        .saturating_sub(edges.top.saturating_add(edges.bottom));
    Rect::new(x, y, width, height)
}

fn intersect(a: Rect, b: Rect) -> Rect {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let right = a.x.saturating_add(a.width).min(b.x.saturating_add(b.width));
    let bottom =
        a.y.saturating_add(a.height)
            .min(b.y.saturating_add(b.height));
    Rect::new(x, y, right.saturating_sub(x), bottom.saturating_sub(y))
}

fn resolve(length: Length, available: u16, intrinsic: u16) -> u16 {
    match length {
        Length::Auto => intrinsic,
        Length::Cells(value) => value,
        Length::Percent(percent) => {
            (u32::from(available) * u32::from(percent.min(100)) / 100) as u16
        }
        Length::Fill(_) => available,
    }
}

fn measure_text(content: &str) -> Size {
    Size {
        width: content
            .lines()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(0) as u16,
        height: content.lines().count().max(1) as u16,
    }
}

fn measure_node(node: &RenderNode) -> Size {
    if let ViewKind::Text(content) = &node.kind {
        return measure_text(content);
    }
    let flow: Vec<&RenderNode> = node
        .children
        .iter()
        .filter(|child| child.visible && child.layout.position == Position::Flow)
        .collect();
    let gap = node
        .layout
        .gap
        .saturating_mul(flow.len().saturating_sub(1) as u16);
    let (children_width, children_height) = if node.layout.direction == Direction::Row {
        (
            flow.iter()
                .fold(0u16, |size, child| {
                    size.saturating_add(child.measured.width)
                })
                .saturating_add(gap),
            flow.iter()
                .map(|child| child.measured.height)
                .max()
                .unwrap_or(0),
        )
    } else {
        (
            flow.iter()
                .map(|child| child.measured.width)
                .max()
                .unwrap_or(0),
            flow.iter()
                .fold(0u16, |size, child| {
                    size.saturating_add(child.measured.height)
                })
                .saturating_add(gap),
        )
    };
    Size {
        width: children_width
            .saturating_add(node.layout.padding.left)
            .saturating_add(node.layout.padding.right),
        height: children_height
            .saturating_add(node.layout.padding.top)
            .saturating_add(node.layout.padding.bottom),
    }
}

fn intrinsic(node: &RenderNode, horizontal: bool) -> u16 {
    if horizontal {
        node.measured.width
    } else {
        node.measured.height
    }
}

fn clamp_size(value: u16, min: u16, max: Option<u16>) -> u16 {
    value.max(min).min(max.unwrap_or(u16::MAX))
}

fn resolved_rect(node: &RenderNode, available: Rect) -> Rect {
    Rect::new(
        available.x,
        available.y,
        clamp_size(
            resolve(node.layout.width, available.width, intrinsic(node, true)),
            node.layout.min_width,
            node.layout.max_width,
        ),
        clamp_size(
            resolve(node.layout.height, available.height, intrinsic(node, false)),
            node.layout.min_height,
            node.layout.max_height,
        ),
    )
}

fn main_edges(edges: Edges, horizontal: bool) -> (u16, u16) {
    if horizontal {
        (edges.left, edges.right)
    } else {
        (edges.top, edges.bottom)
    }
}

fn cross_edges(edges: Edges, horizontal: bool) -> (u16, u16) {
    if horizontal {
        (edges.top, edges.bottom)
    } else {
        (edges.left, edges.right)
    }
}

#[allow(clippy::too_many_lines)]
fn layout_node(node: &mut RenderNode, assigned: Rect) {
    if !node.visible {
        node.rect = Rect::new(assigned.x, assigned.y, 0, 0);
        return;
    }
    node.rect = assigned;

    let content = inset(node.rect, node.layout.padding);
    let horizontal = node.layout.direction == Direction::Row;
    let main_available = if horizontal {
        content.width
    } else {
        content.height
    };
    let cross_available = if horizontal {
        content.height
    } else {
        content.width
    };
    let flow_indices: Vec<usize> = node
        .children
        .iter()
        .enumerate()
        .filter(|(_, child)| child.visible && child.layout.position == Position::Flow)
        .map(|(index, _)| index)
        .collect();
    let gaps = node
        .layout
        .gap
        .saturating_mul(flow_indices.len().saturating_sub(1) as u16);
    let mut sizes = Vec::with_capacity(flow_indices.len());
    let mut margins = 0u16;
    let mut total = 0u16;
    let mut grow_total = 0u16;

    for &index in &flow_indices {
        let child = &node.children[index];
        let length = if horizontal {
            child.layout.width
        } else {
            child.layout.height
        };
        let intrinsic = intrinsic(child, horizontal);
        let min = if horizontal {
            child.layout.min_width
        } else {
            child.layout.min_height
        };
        let max = if horizontal {
            child.layout.max_width
        } else {
            child.layout.max_height
        };
        let basis = match length {
            Length::Fill(_) => min,
            other => resolve(other, main_available, intrinsic),
        };
        let base = clamp_size(basis, min, max);
        let (before, after) = main_edges(child.layout.margin, horizontal);
        let grow = match length {
            Length::Fill(weight) => weight.max(1),
            _ => child.layout.flex_grow,
        };
        sizes.push(base);
        total = total.saturating_add(base);
        margins = margins.saturating_add(before.saturating_add(after));
        grow_total = grow_total.saturating_add(grow);
    }

    let usable = main_available.saturating_sub(gaps).saturating_sub(margins);
    if total < usable && grow_total > 0 {
        let mut extra = usable - total;
        while extra > 0 {
            let active_grow: u32 = sizes
                .iter()
                .zip(&flow_indices)
                .map(|(size, &index)| {
                    let child = &node.children[index];
                    let length = if horizontal {
                        child.layout.width
                    } else {
                        child.layout.height
                    };
                    let grow = match length {
                        Length::Fill(weight) => weight.max(1),
                        _ => child.layout.flex_grow,
                    };
                    let max = if horizontal {
                        child.layout.max_width
                    } else {
                        child.layout.max_height
                    }
                    .unwrap_or(main_available);
                    if *size < max { u32::from(grow) } else { 0 }
                })
                .sum();
            if active_grow == 0 {
                break;
            }
            let before = extra;
            for (slot, &index) in sizes.iter_mut().zip(&flow_indices) {
                let child = &node.children[index];
                let length = if horizontal {
                    child.layout.width
                } else {
                    child.layout.height
                };
                let grow = match length {
                    Length::Fill(weight) => weight.max(1),
                    _ => child.layout.flex_grow,
                };
                let max = if horizontal {
                    child.layout.max_width
                } else {
                    child.layout.max_height
                }
                .unwrap_or(main_available);
                if grow == 0 || *slot >= max {
                    continue;
                }
                let share = ((u32::from(extra) * u32::from(grow) / active_grow) as u16).max(1);
                let addition = share.min(max - *slot).min(extra);
                *slot += addition;
                extra -= addition;
                if extra == 0 {
                    break;
                }
            }
            if extra == before {
                break;
            }
        }
    } else if total > usable {
        let mut deficit = total - usable;
        while deficit > 0 {
            let shrink_total: u32 = sizes
                .iter()
                .zip(&flow_indices)
                .map(|(size, &index)| {
                    let child = &node.children[index];
                    let min = if horizontal {
                        child.layout.min_width
                    } else {
                        child.layout.min_height
                    };
                    if *size > min {
                        u32::from(child.layout.flex_shrink) * u32::from(*size)
                    } else {
                        0
                    }
                })
                .sum();
            if shrink_total == 0 {
                break;
            }
            let before = deficit;
            for (slot, &index) in sizes.iter_mut().zip(&flow_indices) {
                let child = &node.children[index];
                let min = if horizontal {
                    child.layout.min_width
                } else {
                    child.layout.min_height
                };
                let weight = u32::from(child.layout.flex_shrink) * u32::from(*slot);
                if weight == 0 || *slot <= min {
                    continue;
                }
                let share = ((u32::from(deficit) * weight / shrink_total) as u16).max(1);
                let reduction = share.min(slot.saturating_sub(min)).min(deficit);
                *slot -= reduction;
                deficit -= reduction;
                if deficit == 0 {
                    break;
                }
            }
            if deficit == before {
                break;
            }
        }
    }

    let occupied = sizes
        .iter()
        .fold(margins.saturating_add(gaps), |sum, size| {
            sum.saturating_add(*size)
        });
    let free = main_available.saturating_sub(occupied);
    let (mut cursor, distributed_gap) = match node.layout.justify {
        Justify::Center => (free / 2, 0),
        Justify::End => (free, 0),
        Justify::SpaceBetween if flow_indices.len() > 1 => {
            (0, free / (flow_indices.len() as u16 - 1))
        }
        Justify::Start | Justify::SpaceBetween => (0, 0),
    };

    for ((&index, main), sequence) in flow_indices.iter().zip(sizes).zip(0..) {
        let child = &mut node.children[index];
        let (main_before, main_after) = main_edges(child.layout.margin, horizontal);
        let (cross_before, cross_after) = cross_edges(child.layout.margin, horizontal);
        cursor = cursor.saturating_add(main_before);
        let cross_space = cross_available.saturating_sub(cross_before.saturating_add(cross_after));
        let cross_length = if horizontal {
            child.layout.height
        } else {
            child.layout.width
        };
        let cross_intrinsic = intrinsic(child, !horizontal);
        let cross_min = if horizontal {
            child.layout.min_height
        } else {
            child.layout.min_width
        };
        let cross_max = if horizontal {
            child.layout.max_height
        } else {
            child.layout.max_width
        };
        let cross = match (cross_length, node.layout.align) {
            (Length::Auto | Length::Fill(_), Align::Stretch) => cross_space,
            (other, _) => clamp_size(
                resolve(other, cross_space, cross_intrinsic),
                cross_min,
                cross_max,
            ),
        };
        let cross_offset = cross_before
            + match node.layout.align {
                Align::Stretch | Align::Start => 0,
                Align::Center => cross_space.saturating_sub(cross) / 2,
                Align::End => cross_space.saturating_sub(cross),
            };
        let rect = if horizontal {
            Rect::new(content.x + cursor, content.y + cross_offset, main, cross)
        } else {
            Rect::new(content.x + cross_offset, content.y + cursor, cross, main)
        };
        layout_node(child, rect);
        cursor = cursor
            .saturating_add(main)
            .saturating_add(main_after)
            .saturating_add(node.layout.gap)
            .saturating_add(if sequence + 1 < flow_indices.len() {
                distributed_gap
            } else {
                0
            });
    }

    for child in node
        .children
        .iter_mut()
        .filter(|child| child.visible && child.layout.position == Position::Absolute)
    {
        let available = inset(content, child.layout.inset);
        let mut rect = resolved_rect(child, available);
        rect.x = available.x;
        rect.y = available.y;
        layout_node(child, rect);
    }
}

fn flatten_render_list(node: &RenderNode, parent_clip: Rect, commands: &mut Vec<RenderCommand>) {
    if !node.visible {
        return;
    }
    let node_clip = intersect(node.rect, parent_clip);
    if node_clip.width == 0 || node_clip.height == 0 {
        return;
    }

    let clips_children = node.layout.overflow == Overflow::Clip && !node.children.is_empty();
    if clips_children {
        commands.push(RenderCommand::PushClip(node_clip));
    }
    match &node.kind {
        ViewKind::Box => commands.push(RenderCommand::PaintBox {
            rect: node.rect,
            style: node.style,
        }),
        ViewKind::Text(content) => commands.push(RenderCommand::PaintText {
            rect: node.rect,
            content: content.clone(),
            style: node.style,
        }),
    }
    let child_clip = if node.layout.overflow == Overflow::Clip {
        node_clip
    } else {
        parent_clip
    };
    for child in &node.children {
        flatten_render_list(child, child_clip, commands);
    }
    if clips_children {
        commands.push(RenderCommand::PopClip);
    }
}

fn paint_commands(commands: &[RenderCommand], frame: &mut Frame, area: Rect) {
    let mut clips = vec![area];
    for command in commands {
        match command {
            RenderCommand::PushClip(rect) => {
                let parent = clips.last().copied().unwrap_or(area);
                clips.push(intersect(parent, *rect));
            }
            RenderCommand::PaintBox { rect, style } => {
                let clip = clips.last().copied().unwrap_or(area);
                frame.render_widget(Block::default().style(*style), intersect(*rect, clip));
            }
            RenderCommand::PaintText {
                rect,
                content,
                style,
            } => {
                let clip = clips.last().copied().unwrap_or(area);
                frame.render_widget(
                    Paragraph::new(content.as_str()).style(*style),
                    intersect(*rect, clip),
                );
            }
            RenderCommand::PopClip => {
                debug_assert!(clips.len() > 1, "unbalanced render clip stack");
                clips.pop();
            }
        }
    }
    debug_assert_eq!(clips.len(), 1, "unbalanced render clip stack");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Schedule {
    Idle,
    RenderNow,
    RenderAt(Instant),
}

pub(crate) struct FrameScheduler {
    dirty: bool,
    live: usize,
    frame_interval: Duration,
    next_frame: Instant,
}

impl FrameScheduler {
    pub(crate) fn new(now: Instant, max_fps: u32) -> Self {
        Self {
            dirty: true,
            live: 0,
            frame_interval: Duration::from_secs_f64(1.0 / f64::from(max_fps.max(1))),
            next_frame: now,
        }
    }

    pub(crate) fn request_render(&mut self) {
        self.dirty = true;
    }

    pub(crate) fn acquire_live(&mut self) {
        self.live = self.live.saturating_add(1);
        self.dirty = true;
    }

    pub(crate) fn release_live(&mut self) {
        self.live = self.live.saturating_sub(1);
    }

    pub(crate) fn schedule(&self, now: Instant) -> Schedule {
        if !self.dirty && self.live == 0 {
            Schedule::Idle
        } else if now >= self.next_frame {
            Schedule::RenderNow
        } else {
            Schedule::RenderAt(self.next_frame)
        }
    }

    pub(crate) fn rendered(&mut self, now: Instant) {
        self.dirty = false;
        self.next_frame = now + self.frame_interval;
        if self.live > 0 {
            self.dirty = true;
        }
    }
}
