//! Nested, squarified treemap of the current directory (QDirStat /
//! WinDirStat style): every rectangle's area is proportional to its
//! represented size, and directories contain their children.

use gpui::{Bounds, Hsla, Pixels, Point, Window, fill, hsla, point, px, size};

use crate::model::{Model, NodeId};

const MAX_DEPTH: usize = 8;
const MAX_TILES: usize = 30_000;
/// Inner padding of a directory tile, and room for its label on top.
const PAD: f32 = 2.0;
const HEADER: f32 = 15.0;
/// Below this size a directory is drawn as a plain block.
const MIN_NEST_W: f32 = 36.0;
const MIN_NEST_H: f32 = 30.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }
}

#[derive(Clone, Debug)]
pub struct Tile {
    pub node: NodeId,
    pub rect: Rect,
    pub depth: usize,
    pub hue: f32,
    pub special: bool,
    pub has_children: bool,
    /// Directories drawn with their children inside them.
    pub nested: bool,
}

/// Worst aspect ratio of a row of `areas` laid along a side of length `side`.
fn worst(areas: &[f32], side: f32) -> f32 {
    let sum: f32 = areas.iter().sum();
    let (min, max) = areas
        .iter()
        .fold((f32::MAX, 0f32), |(lo, hi), &a| (lo.min(a), hi.max(a)));
    let s2 = sum * sum;
    let w2 = side * side;
    (w2 * max / s2).max(s2 / (w2 * min))
}

/// Squarified layout (Bruls, Huizing, van Wijk) of `areas`, which must be
/// sorted in decreasing order and add up to the area of `rect`.
pub fn squarify(areas: &[f32], mut rect: Rect) -> Vec<Rect> {
    let mut out = Vec::with_capacity(areas.len());
    let mut start = 0;
    while start < areas.len() {
        let side = rect.w.min(rect.h);
        if side <= 0.0 {
            out.extend(std::iter::repeat_n(
                Rect {
                    w: 0.0,
                    h: 0.0,
                    ..rect
                },
                areas.len() - start,
            ));
            break;
        }
        let mut end = start + 1;
        while end < areas.len()
            && worst(&areas[start..=end], side) <= worst(&areas[start..end], side)
        {
            end += 1;
        }
        let row = &areas[start..end];
        let sum: f32 = row.iter().sum();
        let thickness = sum / side;
        let mut offset = 0.0;
        if rect.w >= rect.h {
            // Vertical column on the left.
            for &a in row {
                let len = if thickness > 0.0 { a / thickness } else { 0.0 };
                out.push(Rect {
                    x: rect.x,
                    y: rect.y + offset,
                    w: thickness,
                    h: len,
                });
                offset += len;
            }
            rect = Rect {
                x: rect.x + thickness,
                w: (rect.w - thickness).max(0.0),
                ..rect
            };
        } else {
            // Horizontal row on top.
            for &a in row {
                let len = if thickness > 0.0 { a / thickness } else { 0.0 };
                out.push(Rect {
                    x: rect.x + offset,
                    y: rect.y,
                    w: len,
                    h: thickness,
                });
                offset += len;
            }
            rect = Rect {
                y: rect.y + thickness,
                h: (rect.h - thickness).max(0.0),
                ..rect
            };
        }
        start = end;
    }
    out
}

pub fn layout(m: &Model, center: NodeId, width: f32, height: f32) -> Vec<Tile> {
    let mut tiles = Vec::new();
    if width < 4.0 || height < 4.0 {
        return tiles;
    }
    place(
        m,
        center,
        Rect {
            x: 0.0,
            y: 0.0,
            w: width,
            h: height,
        },
        1,
        None,
        &mut tiles,
    );
    tiles
}

fn place(
    m: &Model,
    node: NodeId,
    rect: Rect,
    depth: usize,
    hue: Option<f32>,
    tiles: &mut Vec<Tile>,
) {
    let n = &m.nodes[node as usize];
    let mut children: Vec<NodeId> = n
        .children
        .values()
        .copied()
        .filter(|&c| m.nodes[c as usize].represented > 0)
        .collect();
    children.sort_by(|&a, &b| {
        let (na, nb) = (&m.nodes[a as usize], &m.nodes[b as usize]);
        nb.represented
            .cmp(&na.represented)
            .then_with(|| na.name.cmp(&nb.name))
    });
    let total: u64 = children
        .iter()
        .map(|&c| m.nodes[c as usize].represented)
        .sum();
    if total == 0 {
        return;
    }
    let scale = rect.w * rect.h / total as f32;
    let areas: Vec<f32> = children
        .iter()
        .map(|&c| m.nodes[c as usize].represented as f32 * scale)
        .collect();
    for (ix, (&child, r)) in children.iter().zip(squarify(&areas, rect)).enumerate() {
        if tiles.len() >= MAX_TILES {
            return;
        }
        if r.w < 1.0 || r.h < 1.0 {
            continue;
        }
        let c = &m.nodes[child as usize];
        // Golden-angle hues for top-level entries; descendants inherit.
        let hue = hue.unwrap_or((ix as f32 * 0.618_034).fract());
        let has_children = !c.children.is_empty();
        let nested = has_children && depth < MAX_DEPTH && r.w >= MIN_NEST_W && r.h >= MIN_NEST_H;
        tiles.push(Tile {
            node: child,
            rect: r,
            depth,
            hue,
            special: c.is_special(),
            has_children,
            nested,
        });
        if nested {
            let inner = Rect {
                x: r.x + PAD,
                y: r.y + HEADER,
                w: r.w - 2.0 * PAD,
                h: r.h - HEADER - PAD,
            };
            place(m, child, inner, depth + 1, Some(hue), tiles);
        }
    }
}

/// Deepest tile under `pos` (relative to the chart's origin).
pub fn hit(tiles: &[Tile], x: f32, y: f32) -> Option<usize> {
    tiles.iter().rposition(|t| t.rect.contains(x, y))
}

pub fn color(tile: &Tile, highlighted: bool) -> Hsla {
    let (sat, base) = if tile.special {
        (0.1, 0.36)
    } else {
        (0.5, 0.42)
    };
    let mut light = if tile.nested {
        0.17 + tile.depth as f32 * 0.025
    } else {
        base + tile.depth as f32 * 0.03 + if tile.has_children { -0.06 } else { 0.0 }
    };
    if highlighted {
        light += 0.12;
    }
    let hue = if tile.special { 0.1 } else { tile.hue };
    hsla(hue, sat, light.min(0.8), 1.0)
}

pub fn paint(
    bounds: Bounds<Pixels>,
    tiles: &[Tile],
    hovered: Option<NodeId>,
    selected: Option<NodeId>,
    window: &mut Window,
) {
    let origin = bounds.origin;
    let to_bounds = |r: &Rect, inset: f32| {
        Bounds::new(
            point(origin.x + px(r.x + inset), origin.y + px(r.y + inset)),
            size(
                px((r.w - 2.0 * inset).max(0.0)),
                px((r.h - 2.0 * inset).max(0.0)),
            ),
        )
    };
    for t in tiles {
        let lit = hovered == Some(t.node);
        let inset = if t.rect.w > 3.0 && t.rect.h > 3.0 {
            0.5
        } else {
            0.0
        };
        window.paint_quad(fill(to_bounds(&t.rect, inset), color(t, lit)));
    }
    for t in tiles
        .iter()
        .filter(|t| selected == Some(t.node) || hovered == Some(t.node))
    {
        let quad = fill(to_bounds(&t.rect, 0.0), hsla(0., 0., 0., 0.))
            .border_widths(px(2.))
            .border_color(hsla(0.6, 0.9, 0.85, 1.0));
        window.paint_quad(quad);
    }
}

pub fn relative(bounds: Bounds<Pixels>, pos: Point<Pixels>) -> (f32, f32) {
    (
        f32::from(pos.x - bounds.origin.x),
        f32::from(pos.y - bounds.origin.y),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squarify_fills_rect() {
        let areas = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0];
        let rects = squarify(
            &areas,
            Rect {
                x: 0.0,
                y: 0.0,
                w: 6.0,
                h: 4.0,
            },
        );
        assert_eq!(rects.len(), areas.len());
        for (a, r) in areas.iter().zip(&rects) {
            assert!((r.w * r.h - a).abs() < 1e-3, "{r:?} should have area {a}");
            assert!(
                r.x >= -1e-4 && r.y >= -1e-4 && r.x + r.w <= 6.0 + 1e-3 && r.y + r.h <= 4.0 + 1e-3
            );
        }
        let total: f32 = rects.iter().map(|r| r.w * r.h).sum();
        assert!((total - 24.0).abs() < 1e-3);
    }
}
