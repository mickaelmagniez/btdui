//! Concentric-rings ("sunburst") chart of the current directory, in the
//! spirit of GNOME Disk Usage Analyzer and Filelight.
//!
//! The current directory is the central disc; each ring outwards is one
//! level deeper. A segment's angle is proportional to its represented size.

use std::f32::consts::TAU;

use gpui::{Bounds, Hsla, PathBuilder, Pixels, Point, Window, hsla, point, px};

use crate::model::{Model, NodeId};

/// Rings drawn around the center.
pub const DEPTH: usize = 5;
/// Segments thinner than this (radians) are not drawn individually.
const MIN_ANGLE: f32 = 0.004;
/// Fraction of the radius used by the central disc.
const CENTER_FRACTION: f32 = 0.2;

#[derive(Clone, Debug)]
pub struct Segment {
    pub node: NodeId,
    /// 1 for direct children of the center.
    pub depth: usize,
    pub start: f32,
    pub end: f32,
    pub special: bool,
    pub has_children: bool,
}

/// Lays out up to [`DEPTH`] rings below `center`.
pub fn layout(m: &Model, center: NodeId) -> Vec<Segment> {
    let mut out = Vec::new();
    let total = m.nodes[center as usize].represented;
    if total == 0 {
        return out;
    }
    let mut stack = vec![(center, 0usize, 0.0f32, TAU)];
    while let Some((node, depth, start, span)) = stack.pop() {
        if depth == DEPTH {
            continue;
        }
        let n = &m.nodes[node as usize];
        let mut children: Vec<NodeId> = n.children.values().copied().collect();
        children.sort_by(|&a, &b| {
            let (na, nb) = (&m.nodes[a as usize], &m.nodes[b as usize]);
            nb.represented
                .cmp(&na.represented)
                .then_with(|| na.name.cmp(&nb.name))
        });
        let parent_total = n.represented.max(1) as f32;
        let mut angle = start;
        for child in children {
            let c = &m.nodes[child as usize];
            let sweep = span * c.represented as f32 / parent_total;
            if sweep < MIN_ANGLE {
                break; // sorted by size: everything after is smaller
            }
            out.push(Segment {
                node: child,
                depth: depth + 1,
                start: angle,
                end: angle + sweep,
                special: c.is_special(),
                has_children: !c.children.is_empty(),
            });
            stack.push((child, depth + 1, angle, sweep));
            angle += sweep;
        }
    }
    out
}

/// Geometry of the chart inside its bounds.
#[derive(Clone, Copy)]
pub struct Geometry {
    pub center: Point<Pixels>,
    pub inner: f32,
    pub ring: f32,
}

impl Geometry {
    pub fn new(bounds: Bounds<Pixels>) -> Self {
        let radius = f32::from(bounds.size.width.min(bounds.size.height)) / 2.0 - 12.0;
        let radius = radius.max(40.0);
        let inner = radius * CENTER_FRACTION;
        Self {
            center: bounds.center(),
            inner,
            ring: (radius - inner) / DEPTH as f32,
        }
    }

    /// What lies under `pos`: `Some(None)` for the center disc,
    /// `Some(Some(ix))` for a segment, `None` for nothing.
    pub fn hit(&self, segments: &[Segment], pos: Point<Pixels>) -> Option<Option<usize>> {
        let dx = f32::from(pos.x - self.center.x);
        let dy = f32::from(pos.y - self.center.y);
        let r = (dx * dx + dy * dy).sqrt();
        if r <= self.inner {
            return Some(None);
        }
        let depth = ((r - self.inner) / self.ring).floor() as usize + 1;
        if depth > DEPTH {
            return None;
        }
        // 0 at 12 o'clock, clockwise, like the drawing.
        let angle = (dx.atan2(-dy) + TAU) % TAU;
        segments
            .iter()
            .position(|s| s.depth == depth && s.start <= angle && angle < s.end)
            .map(Some)
    }
}

/// Hue follows the angle of the top-level ancestor's middle (so a subtree
/// keeps a family of colors); lightness fades with depth.
pub fn color(seg: &Segment, highlighted: bool) -> Hsla {
    let mid = (seg.start + seg.end) / 2.0;
    let lightness = 0.58 - seg.depth as f32 * 0.055 + if highlighted { 0.14 } else { 0.0 };
    if seg.special {
        return hsla(0.1, 0.12, lightness - 0.08, 1.0);
    }
    hsla(mid / TAU, 0.55, lightness, 1.0)
}

fn polar(g: &Geometry, r: f32, angle: f32) -> Point<Pixels> {
    point(
        g.center.x + px(r * angle.sin()),
        g.center.y - px(r * angle.cos()),
    )
}

/// Draws an annular sector, approximating arcs with short segments.
fn sector(g: &Geometry, r0: f32, r1: f32, start: f32, end: f32) -> Option<gpui::Path<Pixels>> {
    let steps = (((end - start) / 0.03).ceil() as usize).max(1);
    let mut b = PathBuilder::fill();
    b.move_to(polar(g, r0, start));
    for i in 0..=steps {
        b.line_to(polar(
            g,
            r1,
            start + (end - start) * i as f32 / steps as f32,
        ));
    }
    for i in (0..=steps).rev() {
        b.line_to(polar(
            g,
            r0,
            start + (end - start) * i as f32 / steps as f32,
        ));
    }
    b.close();
    b.build().ok()
}

pub fn paint(
    bounds: Bounds<Pixels>,
    segments: &[Segment],
    hovered: Option<NodeId>,
    selected: Option<NodeId>,
    center_color: Hsla,
    window: &mut Window,
) {
    let g = Geometry::new(bounds);
    if let Some(disc) = sector(&g, 0.0, g.inner - 1.0, 0.0, TAU) {
        window.paint_path(disc, center_color);
    }
    for seg in segments {
        let r0 = g.inner + (seg.depth - 1) as f32 * g.ring + 1.0;
        let r1 = r0 + g.ring - 2.0;
        // Leave a hairline gap between neighbours when there is room for it.
        let gap = (0.75 / r1).min((seg.end - seg.start) * 0.2);
        let lit = hovered == Some(seg.node) || selected == Some(seg.node);
        if let Some(path) = sector(&g, r0, r1, seg.start + gap, seg.end - gap) {
            window.paint_path(path, color(seg, lit));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Header, Message};

    #[test]
    fn layout_and_hit() {
        let mut m = Model::new();
        m.apply(Message::Header(Header {
            total_bytes: 100,
            nodesize: 4096,
            devices: 1,
            threads: 1,
        }));
        m.apply(Message::Path {
            id: 0,
            path: "a/x".into(),
        });
        m.apply(Message::Path {
            id: 1,
            path: "b".into(),
        });
        for id in [0, 0, 0, 1] {
            m.apply(Message::Sample { paths: vec![id] });
        }
        let segs = layout(&m, crate::model::ROOT);
        assert_eq!(segs.len(), 3); // a, b, a/x
        let a = &segs[0];
        assert!((a.end - a.start - TAU * 0.75).abs() < 1e-4);

        let bounds = Bounds::new(point(px(0.), px(0.)), gpui::size(px(200.), px(200.)));
        let g = Geometry::new(bounds);
        assert_eq!(g.hit(&segs, point(px(100.), px(100.))), Some(None));
        // Right of the center, first ring: angle π/2, inside "a" (0..3π/2).
        let p = point(px(100. + g.inner + g.ring / 2.), px(100.));
        assert_eq!(
            g.hit(&segs, p).flatten().map(|i| segs[i].node),
            Some(a.node)
        );
    }
}
