//! The gpui front-end.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    Bounds, ClickEvent, ClipboardItem, Context, FocusHandle, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, Pixels, ScrollStrategy, SharedString, UniformListScrollHandle,
    Window, canvas, div, hsla, prelude::*, px, relative, rgb, uniform_list,
};

use crate::model::{Model, NodeId, ROOT, Status};
use crate::session::{Mount, Session, btrfs_mounts};
use crate::sunburst::{self, Segment};
use crate::treemap::{self, Tile};

mod theme {
    pub const BG: u32 = 0x1b1c1f;
    pub const PANEL: u32 = 0x232428;
    pub const RAISED: u32 = 0x2c2e33;
    pub const BORDER: u32 = 0x36383e;
    pub const TEXT: u32 = 0xe2e3e6;
    pub const MUTED: u32 = 0x8b8e96;
    pub const FAINT: u32 = 0x5d6068;
    pub const ACCENT: u32 = 0x5b8ef0;
    pub const SELECTED: u32 = 0x2b3a57;
    pub const HOVER: u32 = 0x282a2f;
    pub const SPECIAL: u32 = 0xdfa94e;
    pub const BAR_TRACK: u32 = 0x2a2c31;
    pub const BAR_SHARED: u32 = 0x2f4366;
    pub const BAR_REPRESENTED: u32 = 0x4a76d4;
    pub const BAR_EXCLUSIVE: u32 = 0x8db6ff;
    pub const DANGER: u32 = 0xe36d6d;
    pub const OK: u32 = 0x6cc28a;
}

const ROW_HEIGHT: f32 = 26.0;
const NUM_COL: f32 = 92.0;
const BAR_COL: f32 = 150.0;
/// Precision goals the progress bar can aim at (fraction of the shown directory).
const TARGETS: [f64; 4] = [0.05, 0.01, 0.005, 0.001];
const TICK: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Rings,
    Treemap,
    List,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Represented,
    Distributed,
    Exclusive,
    Shared,
    Name,
}

impl SortKey {
    fn label(self) -> &'static str {
        match self {
            SortKey::Represented => "Represented",
            SortKey::Distributed => "Distributed",
            SortKey::Exclusive => "Exclusive",
            SortKey::Shared => "Shared",
            SortKey::Name => "Name",
        }
    }
}

/// A child of the current directory, snapshotted for one frame.
#[derive(Clone)]
struct Row {
    id: NodeId,
    name: SharedString,
    special: bool,
    has_children: bool,
    represented: u64,
    distributed: f64,
    exclusive: u64,
    shared: u64,
}

pub struct BtduApp {
    focus: FocusHandle,
    session: Option<Arc<Session>>,
    mounts: Vec<Mount>,
    current: NodeId,
    selected: Option<NodeId>,
    sort: SortKey,
    rows: Vec<Row>,
    scroll: UniformListScrollHandle,
    seen_version: u64,
    pending_scroll: bool,
    view: View,
    target: usize,
    segments: Rc<Vec<Segment>>,
    tiles: Rc<Vec<Tile>>,
    /// Size the treemap was laid out for, to redo it on resize.
    tiles_size: (f32, f32),
    hovered: Option<NodeId>,
    chart_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

pub fn format_size(bytes: f64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut v = bytes.max(0.0);
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{v:.0} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[unit])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[unit])
    } else {
        format!("{v:.2} {}", UNITS[unit])
    }
}

fn describe(name: &str) -> Option<&'static str> {
    Some(match name {
        "<UNALLOCATED>" => {
            "Device space not yet assigned to any block group. btrfs allocates it on demand for data or metadata."
        }
        "<UNUSED>" => {
            "Space inside allocated block groups that holds nothing. A balance (e.g. `btrfs balance start -dusage=50`) can give it back to <UNALLOCATED>."
        }
        "<DATA>" => "Free space inside data block groups.",
        "<METADATA>" => {
            "btrfs internal trees: file system trees of each subvolume, checksums, extent tree, etc. Includes DUP/RAID copies."
        }
        "<SYSTEM>" => {
            "System block groups, holding the chunk tree that maps logical to physical addresses."
        }
        "<UNREACHABLE>" => {
            "Parts of data extents that no file references anymore (partially overwritten \"bookend\" extents). Rewriting or defragmenting the files that still use these extents frees it."
        }
        "<ORPHAN>" => "Data owned by subvolumes that were deleted but not cleaned up yet.",
        "<UNLINKED>" => {
            "Data of files whose path could not be resolved: deleted while still open, or reachable only through extended hard links."
        }
        "<ERROR>" => "Samples that could not be resolved. The child name is the error.",
        "<SUBVOLUMES>" => "Metadata of each subvolume's file system tree.",
        "<TREE>" => "Metadata of this subvolume's own file system tree.",
        "<TOP LEVEL>" => "Metadata of the top-level subvolume (id 5).",
        "<CSUM TREE>" => "Checksums of all data blocks.",
        "<EXTENT TREE>" => "Reference counts and back references of every extent.",
        "<FREE SPACE TREE>" => "Free space cache.",
        "<UNKNOWN OWNER>" => "Tree blocks whose owner could not be determined.",
        _ => return None,
    })
}

impl BtduApp {
    pub fn new(
        path: Option<PathBuf>,
        view: View,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window);

        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                if this.update(cx, |this, cx| this.tick(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        let mut app = Self {
            focus,
            session: None,
            mounts: btrfs_mounts(),
            current: ROOT,
            selected: None,
            sort: SortKey::Represented,
            rows: Vec::new(),
            scroll: UniformListScrollHandle::new(),
            seen_version: u64::MAX,
            pending_scroll: false,
            view,
            target: 1,
            segments: Rc::new(Vec::new()),
            tiles: Rc::new(Vec::new()),
            tiles_size: (0.0, 0.0),
            hovered: None,
            chart_bounds: Rc::new(Cell::new(None)),
        };
        if let Some(path) = path {
            app.open_session(path, window);
        }
        app
    }

    fn open_session(&mut self, path: PathBuf, window: &mut Window) {
        window.set_window_title(&format!("btdui — {}", path.display()));
        self.session = Some(Session::start(path));
        self.current = ROOT;
        self.selected = None;
        self.seen_version = u64::MAX;
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else { return };
        let mut m = session.model.lock().unwrap();
        let now = Instant::now();
        let (since, at) = m.rate_mark;
        let elapsed = now.duration_since(since).as_secs_f64();
        if elapsed >= 1.0 {
            m.rate = (m.samples - at) as f64 / elapsed;
            m.rate_mark = (now, m.samples);
        }
        let resized = self.view == View::Treemap
            && self.chart_bounds.get().is_some_and(|b| {
                (f32::from(b.size.width), f32::from(b.size.height)) != self.tiles_size
            });
        if m.version != self.seen_version || resized {
            self.seen_version = m.version;
            cx.notify();
        }
    }

    fn model(&self) -> Option<std::sync::MutexGuard<'_, Model>> {
        self.session.as_ref().map(|s| s.model.lock().unwrap())
    }

    fn refresh_rows(&mut self) {
        let Some(session) = self.session.clone() else {
            self.rows.clear();
            return;
        };
        let m = session.model.lock().unwrap();
        let node = &m.nodes[self.current as usize];
        self.rows = node
            .children
            .values()
            .map(|&id| {
                let n = &m.nodes[id as usize];
                Row {
                    id,
                    name: SharedString::from(n.name.to_string()),
                    special: n.is_special(),
                    has_children: !n.children.is_empty(),
                    represented: n.represented,
                    distributed: n.distributed,
                    exclusive: n.exclusive,
                    shared: n.shared,
                }
            })
            .collect();
        let sort = self.sort;
        self.rows.sort_by(|a, b| {
            let by_name = || a.name.cmp(&b.name);
            match sort {
                SortKey::Represented => b.represented.cmp(&a.represented).then_with(by_name),
                SortKey::Distributed => b.distributed.total_cmp(&a.distributed).then_with(by_name),
                SortKey::Exclusive => b.exclusive.cmp(&a.exclusive).then_with(by_name),
                SortKey::Shared => b.shared.cmp(&a.shared).then_with(by_name),
                SortKey::Name => by_name(),
            }
        });
        if self
            .selected
            .is_none_or(|s| !self.rows.iter().any(|r| r.id == s))
        {
            self.selected = self.rows.first().map(|r| r.id);
        }
    }

    fn selected_index(&self) -> Option<usize> {
        self.selected
            .and_then(|s| self.rows.iter().position(|r| r.id == s))
    }

    fn navigate(&mut self, dir: NodeId, select: Option<NodeId>, cx: &mut Context<Self>) {
        self.current = dir;
        self.hovered = None;
        self.selected = select;
        self.pending_scroll = true;
        cx.notify();
    }

    fn enter(&mut self, id: NodeId, cx: &mut Context<Self>) {
        let has_children = self
            .model()
            .is_some_and(|m| !m.nodes[id as usize].children.is_empty());
        if has_children {
            self.navigate(id, None, cx);
        }
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        if self.current == ROOT {
            return;
        }
        let parent = self
            .model()
            .map_or(ROOT, |m| m.nodes[self.current as usize].parent);
        let from = self.current;
        self.navigate(parent, Some(from), cx);
    }

    /// Jumps to `node` in its parent directory.
    fn reveal(&mut self, node: NodeId, cx: &mut Context<Self>) {
        let parent = self.model().map_or(ROOT, |m| m.nodes[node as usize].parent);
        self.navigate(parent, Some(node), cx);
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            return;
        }
        let cur = self.selected_index().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, self.rows.len() as isize - 1) as usize;
        self.selected = Some(self.rows[next].id);
        self.scroll.scroll_to_item(next, ScrollStrategy::Center);
        cx.notify();
    }

    fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            let paused = session.model.lock().unwrap().paused;
            session.set_paused(!paused);
            cx.notify();
        }
    }

    fn set_sort(&mut self, sort: SortKey, cx: &mut Context<Self>) {
        self.sort = sort;
        self.pending_scroll = true;
        cx.notify();
    }

    fn copy_selected_path(&self, cx: &mut Context<Self>) {
        if let (Some(m), Some(sel)) = (self.model(), self.selected) {
            cx.write_to_clipboard(ClipboardItem::new_string(m.path_of(sel)));
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        if ks.modifiers.control && ks.key == "q" {
            cx.quit();
            return;
        }
        if self.session.is_none() {
            return;
        }
        match ks.key.as_str() {
            "up" | "k" => self.move_selection(-1, cx),
            "down" | "j" => self.move_selection(1, cx),
            "pageup" => self.move_selection(-20, cx),
            "pagedown" => self.move_selection(20, cx),
            "home" => self.move_selection(isize::MIN / 2, cx),
            "end" => self.move_selection(isize::MAX / 2, cx),
            "enter" | "right" | "l" => {
                if let Some(sel) = self.selected {
                    self.enter(sel, cx);
                }
            }
            "backspace" | "left" | "h" => self.go_up(cx),
            "space" | "p" => self.toggle_pause(cx),
            "1" => self.set_sort(SortKey::Represented, cx),
            "2" => self.set_sort(SortKey::Distributed, cx),
            "3" => self.set_sort(SortKey::Exclusive, cx),
            "4" => self.set_sort(SortKey::Shared, cx),
            "5" => self.set_sort(SortKey::Name, cx),
            "c" if ks.modifiers.control => self.copy_selected_path(cx),
            "v" | "tab" => self.toggle_view(cx),
            "escape" => {
                if let Some(s) = &self.session
                    && !matches!(
                        s.model.lock().unwrap().status,
                        Status::Starting | Status::Running
                    )
                {
                    self.session = None;
                    window.set_window_title("btdui");
                    cx.notify();
                }
            }
            _ => {}
        }
    }
}

impl BtduApp {
    fn toggle_view(&mut self, cx: &mut Context<Self>) {
        let next = match self.view {
            View::Rings => View::Treemap,
            View::Treemap => View::List,
            View::List => View::Rings,
        };
        self.set_view(next, cx);
    }

    fn set_view(&mut self, view: View, cx: &mut Context<Self>) {
        self.view = view;
        self.hovered = None;
        self.chart_bounds.set(None);
        self.pending_scroll = true;
        cx.notify();
    }

    /// What the pointer is on in the current chart: `Some(None)` is the
    /// center of the rings, `Some(Some((node, has_children)))` an entry.
    fn chart_hit(&self, pos: gpui::Point<Pixels>) -> Option<Option<(NodeId, bool)>> {
        let bounds = self.chart_bounds.get()?;
        match self.view {
            View::Rings => sunburst::Geometry::new(bounds)
                .hit(&self.segments, pos)
                .map(|hit| hit.map(|ix| (self.segments[ix].node, self.segments[ix].has_children))),
            View::Treemap => {
                let (x, y) = treemap::relative(bounds, pos);
                treemap::hit(&self.tiles, x, y)
                    .map(|ix| Some((self.tiles[ix].node, self.tiles[ix].has_children)))
            }
            View::List => None,
        }
    }

    fn on_chart_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let hovered = self.chart_hit(ev.position).flatten().map(|(node, _)| node);
        if hovered != self.hovered {
            self.hovered = hovered;
            cx.notify();
        }
    }

    fn on_chart_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window);
        match self.chart_hit(ev.position) {
            Some(None) => self.go_up(cx),
            Some(Some((node, true))) => self.navigate(node, None, cx),
            Some(Some((node, false))) => {
                self.selected = Some(node);
                cx.notify();
            }
            None => {}
        }
    }

    fn render_chart(&self, m: &Model, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let segments = self.segments.clone();
        let tiles = self.tiles.clone();
        let view = self.view;
        let (hovered, selected) = (self.hovered, self.selected);
        let bounds_cell = self.chart_bounds.clone();
        let center_color = hsla(0.62, 0.12, if hovered.is_none() { 0.24 } else { 0.2 }, 1.0);
        let chart = canvas(
            move |bounds, _, _| bounds_cell.set(Some(bounds)),
            move |bounds, (), window, _| match view {
                View::Treemap => treemap::paint(bounds, &tiles, hovered, selected, window),
                _ => sunburst::paint(bounds, &segments, hovered, selected, center_color, window),
            },
        )
        .size_full();

        // Names on the tiles that have room for one.
        let tile_labels: Vec<_> = if view == View::Treemap {
            self.tiles
                .iter()
                .filter(|t| t.rect.w >= 48.0 && (t.rect.h >= 15.0))
                .take(400)
                .map(|t| {
                    let n = &m.nodes[t.node as usize];
                    let text = if t.nested || t.rect.h < 30.0 {
                        n.name.to_string()
                    } else {
                        format!("{}\n{}", n.name, format_size(m.bytes(n.represented)))
                    };
                    div()
                        .absolute()
                        .left(px(t.rect.x + 4.0))
                        .top(px(t.rect.y + 1.0))
                        .w(px(t.rect.w - 8.0))
                        .max_h(px(t.rect.h - 2.0))
                        .overflow_hidden()
                        .text_xs()
                        .line_height(px(13.))
                        .text_color(rgb(if t.nested { theme::MUTED } else { theme::TEXT }))
                        .children(
                            text.lines()
                                .map(|l| div().truncate().child(l.to_owned()))
                                .collect::<Vec<_>>(),
                        )
                })
                .collect()
        } else {
            Vec::new()
        };

        let current = &m.nodes[self.current as usize];
        let center_name = if self.current == ROOT {
            "/".to_owned()
        } else {
            current.name.to_string()
        };
        let center = div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .child(
                div()
                    .max_w(px(140.))
                    .truncate()
                    .text_sm()
                    .text_color(rgb(theme::TEXT))
                    .child(center_name),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child(format_size(m.bytes(current.represented))),
            )
            .when(self.current != ROOT, |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::FAINT))
                        .child("click to go up"),
                )
            });
        let center = (view == View::Rings).then_some(center);

        let caption = self.hovered.map(|id| {
            let n = &m.nodes[id as usize];
            let pct = frac(n.represented, current.represented) * 100.0;
            div()
                .absolute()
                .bottom_2()
                .left_4()
                .px_2()
                .py_1()
                .rounded_md()
                .bg(rgb(theme::RAISED))
                .border_1()
                .border_color(rgb(theme::BORDER))
                .text_sm()
                .child(format!(
                    "{} · {} · {pct:.1}%",
                    n.name,
                    format_size(m.bytes(n.represented))
                ))
        });

        div()
            .id("chart")
            .relative()
            .flex_1()
            .min_h_0()
            .cursor_pointer()
            .on_mouse_move(cx.listener(Self::on_chart_move))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_chart_down))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _: &MouseDownEvent, _, cx| this.go_up(cx)),
            )
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered && this.hovered.take().is_some() {
                    cx.notify();
                }
            }))
            .child(chart)
            .children(tile_labels)
            .children(center)
            .children(caption)
    }
}

fn label(text: impl Into<SharedString>, color: u32) -> gpui::Div {
    div().text_color(rgb(color)).child(text.into())
}

fn button(
    id: &'static str,
    text: impl Into<SharedString>,
    active: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(rgb(if active { theme::ACCENT } else { theme::BORDER }))
        .bg(rgb(if active {
            theme::SELECTED
        } else {
            theme::RAISED
        }))
        .text_color(rgb(theme::TEXT))
        .text_sm()
        .cursor_pointer()
        .hover(|s| s.bg(rgb(theme::HOVER)).border_color(rgb(theme::ACCENT)))
        .child(text.into())
}

fn fmt_pct(fraction: f64) -> String {
    let pct = fraction * 100.0;
    if pct >= 10.0 {
        format!("{pct:.0}%")
    } else if pct >= 1.0 {
        format!("{pct:.1}%")
    } else {
        format!("{pct:.2}%")
    }
}

fn frac(part: u64, whole: u64) -> f32 {
    if whole == 0 {
        0.0
    } else {
        (part as f64 / whole as f64).clamp(0.0, 1.0) as f32
    }
}

fn usage_bar(row_shared: u64, represented: u64, exclusive: u64, scale: u64) -> gpui::Div {
    let layer = |f: f32, color: u32| {
        div()
            .absolute()
            .top_0()
            .left_0()
            .h_full()
            .w(relative(f))
            .rounded_sm()
            .bg(rgb(color))
    };
    div()
        .relative()
        .w(px(BAR_COL))
        .h(px(10.))
        .rounded_sm()
        .bg(rgb(theme::BAR_TRACK))
        .child(layer(frac(row_shared, scale), theme::BAR_SHARED))
        .child(layer(frac(represented, scale), theme::BAR_REPRESENTED))
        .child(layer(frac(exclusive, scale), theme::BAR_EXCLUSIVE))
}

impl BtduApp {
    fn render_picker(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let mut list = div().flex().flex_col().gap_2().w(px(560.));
        if self.mounts.is_empty() {
            list = list.child(label("No mounted btrfs filesystem found.", theme::MUTED));
        }
        for (ix, mount) in self.mounts.iter().enumerate() {
            let path = mount.mountpoint.clone();
            list = list.child(
                div()
                    .id(("mount", ix))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .bg(rgb(theme::PANEL))
                    .cursor_pointer()
                    .hover(|s| s.border_color(rgb(theme::ACCENT)).bg(rgb(theme::HOVER)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.open_session(path.clone(), window);
                        cx.notify();
                    }))
                    .child(label(mount.mountpoint.display().to_string(), theme::TEXT))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::MUTED))
                            .child(format!("{}  ·  subvolume {}", mount.source, mount.subvol)),
                    ),
            );
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .child(div().text_2xl().text_color(rgb(theme::TEXT)).child("btdui"))
            .child(label("Sampling disk usage profiler for btrfs. Pick a filesystem:", theme::MUTED))
            .child(list)
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::FAINT))
                    .max_w(px(560.))
                    .child(
                        "Any mount of a filesystem shows all its subvolumes. Reading the filesystem's internal \
                         trees requires root: you will be asked to authenticate (pkexec).",
                    ),
            )
    }

    fn render_header(&self, m: &Model, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let (status_text, status_color) = match (&m.status, m.paused) {
            (Status::Starting, _) => ("Starting…".to_owned(), theme::MUTED),
            (Status::Running, true) => ("Paused".to_owned(), theme::SPECIAL),
            (Status::Running, false) => (format!("Sampling · {:.0}/s", m.rate), theme::OK),
            (Status::Finished(_), _) => ("Stopped".to_owned(), theme::MUTED),
            (Status::Failed(_), _) => ("Failed".to_owned(), theme::DANGER),
        };
        let resolution = if m.samples > 0 {
            m.total_bytes() as f64 / m.samples as f64
        } else {
            0.0
        };
        let stat = |name: &'static str, value: String| {
            div()
                .flex()
                .flex_col()
                .child(div().text_xs().text_color(rgb(theme::FAINT)).child(name))
                .child(div().text_sm().text_color(rgb(theme::TEXT)).child(value))
        };
        let path = self
            .session
            .as_ref()
            .map(|s| s.path.display().to_string())
            .unwrap_or_default();
        div()
            .flex()
            .items_center()
            .gap_6()
            .px_4()
            .py_2()
            .bg(rgb(theme::PANEL))
            .border_b_1()
            .border_color(rgb(theme::BORDER))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(div().text_color(rgb(theme::TEXT)).child("btdui"))
                    .child(div().text_xs().text_color(rgb(theme::MUTED)).child(path)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().size(px(8.)).rounded_full().bg(rgb(status_color)))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(status_color))
                            .child(status_text),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .justify_center()
                    .child(self.render_progress(m, cx)),
            )
            .child(stat("Total", format_size(m.total_bytes() as f64)))
            .child(stat("Samples", m.samples.to_string()))
            .child(stat("Resolution", format_size(resolution)))
            .child(
                button("pause", if m.paused { "Resume" } else { "Pause" }, false)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_pause(cx))),
            )
    }

    fn render_progress(&self, m: &Model, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let target = TARGETS[self.target];
        let precision = m.precision(self.current, target);
        let progress = precision.map_or(0.0, |(_, needed)| (m.samples as f64 / needed).min(1.0));
        let done = progress >= 1.0;
        let detail = match precision {
            None => "waiting for samples".to_owned(),
            Some(_) if done => "goal reached".to_owned(),
            Some((_, needed)) if m.rate > 0.0 && !m.paused => {
                let secs = (needed - m.samples as f64) / m.rate;
                if secs < 90.0 {
                    format!("~{secs:.0} s left")
                } else {
                    format!("~{:.0} min left", secs / 60.0)
                }
            }
            Some(_) => "paused".to_owned(),
        };
        let err = precision.map_or("–".to_owned(), |(e, _)| format!("±{}", fmt_pct(e)));
        let dir = if self.current == ROOT {
            "/".to_owned()
        } else {
            m.nodes[self.current as usize].name.to_string()
        };
        div()
            .id("progress")
            .w(px(360.))
            .flex()
            .flex_col()
            .gap_1()
            .cursor_pointer()
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.target = (this.target + 1) % TARGETS.len();
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .child(format!("Precision {err} of {dir}")),
                    )
                    .child(format!("goal ±{} · {detail}", fmt_pct(target))),
            )
            .child(
                div()
                    .h(px(6.))
                    .rounded_full()
                    .bg(rgb(theme::BAR_TRACK))
                    .child(
                        div()
                            .h_full()
                            .rounded_full()
                            .w(relative(progress as f32))
                            .bg(rgb(if done { theme::OK } else { theme::ACCENT })),
                    ),
            )
    }

    fn render_breadcrumb(&self, m: &Model, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let mut bar = div()
            .flex_1()
            .flex()
            .items_center()
            .gap_1()
            .text_sm()
            .overflow_hidden();
        for (ix, id) in m.ancestry(self.current).into_iter().enumerate() {
            if ix > 0 {
                bar = bar.child(label("›", theme::FAINT));
            }
            let name = if id == ROOT {
                "/".to_owned()
            } else {
                m.nodes[id as usize].name.to_string()
            };
            let is_current = id == self.current;
            bar = bar.child(
                div()
                    .id(("crumb", ix))
                    .px_1()
                    .rounded_sm()
                    .text_color(rgb(if is_current {
                        theme::TEXT
                    } else {
                        theme::MUTED
                    }))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::HOVER)).text_color(rgb(theme::TEXT)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if id != this.current {
                            let from = this.model().map(|m| {
                                let chain = m.ancestry(this.current);
                                chain.get(ix + 1).copied()
                            });
                            this.navigate(id, from.flatten(), cx);
                        }
                    }))
                    .child(name),
            );
        }
        let view = self.view;
        let tab = |id: &'static str, text: &'static str, which: View| {
            button(id, text, view == which).on_click(cx.listener(
                move |this, _: &ClickEvent, _, cx| {
                    if this.view != which {
                        this.set_view(which, cx);
                    }
                },
            ))
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_4()
            .py_2()
            .child(bar)
            .child(tab("view-rings", "Rings", View::Rings))
            .child(tab("view-treemap", "Treemap", View::Treemap))
            .child(tab("view-list", "List", View::List))
    }

    fn render_column_header(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let col = |key: SortKey, width: Option<f32>| {
            let active = self.sort == key;
            let text = if active {
                format!("{} ▾", key.label())
            } else {
                key.label().to_owned()
            };
            let mut d = div()
                .id(key.label())
                .text_xs()
                .text_color(rgb(if active { theme::TEXT } else { theme::MUTED }))
                .cursor_pointer()
                .hover(|s| s.text_color(rgb(theme::TEXT)))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_sort(key, cx)))
                .child(text);
            d = match width {
                Some(w) => d.w(px(w)).flex().justify_end(),
                None => d.flex_1(),
            };
            d
        };
        div()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .h(px(24.))
            .border_b_1()
            .border_color(rgb(theme::BORDER))
            .child(col(SortKey::Name, None))
            .child(col(SortKey::Represented, Some(NUM_COL)))
            .child(col(SortKey::Distributed, Some(NUM_COL)))
            .child(col(SortKey::Exclusive, Some(NUM_COL)))
            .child(col(SortKey::Shared, Some(NUM_COL)))
            .child(div().w(px(BAR_COL)))
    }

    fn render_rows(
        &mut self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let Some(session) = self.session.clone() else {
            return Vec::new();
        };
        let m = session.model.lock().unwrap();
        let scale = m.nodes[self.current as usize].shared.max(1);
        let size = |c: u64| format_size(m.bytes(c));
        range
            .filter_map(|ix| self.rows.get(ix).cloned().map(|r| (ix, r)))
            .map(|(ix, row)| {
                let selected = self.selected == Some(row.id);
                let id = row.id;
                let num = |text: String, color: u32| {
                    div()
                        .w(px(NUM_COL))
                        .flex()
                        .justify_end()
                        .text_color(rgb(color))
                        .child(text)
                };
                let name_color = if row.special {
                    theme::SPECIAL
                } else {
                    theme::TEXT
                };
                div()
                    .id(ix)
                    .w_full()
                    .h(px(ROW_HEIGHT))
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .text_sm()
                    .when(selected, |d| d.bg(rgb(theme::SELECTED)))
                    .when(!selected, |d| d.hover(|s| s.bg(rgb(theme::HOVER))))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                        this.selected = Some(id);
                        if ev.click_count() >= 2 {
                            this.enter(id, cx);
                        }
                        this.focus.focus(window);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .items_center()
                            .gap_2()
                            .overflow_hidden()
                            .child(
                                div()
                                    .w(px(10.))
                                    .text_color(rgb(theme::FAINT))
                                    .child(if row.has_children { "›" } else { "" }),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .truncate()
                                    .text_color(rgb(name_color))
                                    .child(row.name.clone()),
                            ),
                    )
                    .child(num(size(row.represented), theme::TEXT))
                    .child(num(format_size(m.bytes_f(row.distributed)), theme::MUTED))
                    .child(num(size(row.exclusive), theme::MUTED))
                    .child(num(size(row.shared), theme::MUTED))
                    .child(usage_bar(row.shared, row.represented, row.exclusive, scale))
                    .into_any_element()
            })
            .collect()
    }

    fn render_details(&self, m: &Model, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let mut panel = div()
            .id("details")
            .w(px(360.))
            .h_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .bg(rgb(theme::PANEL))
            .border_l_1()
            .border_color(rgb(theme::BORDER))
            .overflow_y_scroll();
        let node_id = self.hovered.or(self.selected).unwrap_or(self.current);
        let node = &m.nodes[node_id as usize];
        let title = if node_id == ROOT {
            "/".to_owned()
        } else {
            node.name.to_string()
        };
        let path = m.path_of(node_id);
        panel = panel
            .child(
                div()
                    .text_color(rgb(if node.is_special() {
                        theme::SPECIAL
                    } else {
                        theme::TEXT
                    }))
                    .child(title),
            )
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(rgb(theme::MUTED))
                            .child(format!("/{path}")),
                    )
                    .child(button("copy", "Copy", false).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.copy_selected_path(cx)),
                    )),
            );

        let row = |name: &'static str, help: &'static str, count: u64, swatch: u32| {
            let pct = frac(count, m.samples) * 100.0;
            div()
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .rounded_md()
                .bg(rgb(theme::RAISED))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().size(px(8.)).rounded_sm().bg(rgb(swatch)))
                        .child(
                            div()
                                .flex_1()
                                .text_sm()
                                .text_color(rgb(theme::TEXT))
                                .child(name),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(theme::TEXT))
                                .child(format_size(m.bytes(count))),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .text_xs()
                        .text_color(rgb(theme::FAINT))
                        .child(div().flex_1().child(help))
                        .child(format!(
                            "± {} · {pct:.2}% · {count} samples",
                            format_size(m.margin(count))
                        )),
                )
        };
        panel = panel
            .child(row(
                "Represented",
                "attributed here",
                node.represented,
                theme::BAR_REPRESENTED,
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .rounded_md()
                    .bg(rgb(theme::RAISED))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().size(px(8.)).rounded_sm().bg(rgb(theme::FAINT)))
                            .child(
                                div()
                                    .flex_1()
                                    .text_sm()
                                    .text_color(rgb(theme::TEXT))
                                    .child("Distributed"),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(theme::TEXT))
                                    .child(format_size(m.bytes_f(node.distributed))),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::FAINT))
                            .child("shared data split evenly between its references"),
                    ),
            )
            .child(row(
                "Exclusive",
                "freed if deleted",
                node.exclusive,
                theme::BAR_EXCLUSIVE,
            ))
            .child(row("Shared", "like du", node.shared, theme::BAR_SHARED));

        if let Some(text) = describe(&node.name) {
            panel = panel.child(div().text_sm().text_color(rgb(theme::MUTED)).child(text));
        }

        if let Some(also) = node.also_seen_at.as_deref().filter(|a| !a.is_empty()) {
            let mut list = div().flex().flex_col().gap_1().child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::FAINT))
                    .child("Also referenced from (snapshots, reflinks, hard links):"),
            );
            for (ix, &other) in also.iter().enumerate() {
                list = list.child(
                    div()
                        .id(("also", ix))
                        .text_xs()
                        .text_color(rgb(theme::ACCENT))
                        .cursor_pointer()
                        .hover(|s| s.text_color(rgb(theme::TEXT)))
                        .on_click(
                            cx.listener(move |this, _: &ClickEvent, _, cx| this.reveal(other, cx)),
                        )
                        .child(format!("/{}", m.path_of(other))),
                );
            }
            panel = panel.child(list);
        }
        panel
    }

    fn render_failure(&self, message: &str, log: &[String]) -> impl IntoElement + use<> {
        let mut box_ = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_6()
            .max_w(px(640.))
            .rounded_lg()
            .border_1()
            .border_color(rgb(theme::DANGER))
            .bg(rgb(theme::PANEL))
            .child(
                div()
                    .text_color(rgb(theme::DANGER))
                    .child("The sampler stopped"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(theme::TEXT))
                    .child(message.to_owned()),
            );
        for line in log.iter().rev().take(8).rev() {
            box_ = box_.child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child(line.clone()),
            );
        }
        box_ = box_.child(
            div()
                .text_xs()
                .text_color(rgb(theme::FAINT))
                .child("Press Escape to go back. Set BTDUI_ELEVATE=\"sudo -A\" to use another elevation method."),
        );
        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .child(box_)
    }
}

impl Render for BtduApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div()
            .key_context("btdu")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::BG))
            .text_color(rgb(theme::TEXT));

        let Some(session) = self.session.clone() else {
            return root.child(self.render_picker(cx));
        };

        self.refresh_rows();
        if self.pending_scroll {
            self.pending_scroll = false;
            if let Some(ix) = self.selected_index() {
                self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
            }
        }

        let m = session.model.lock().unwrap();
        let header = self.render_header(&m, cx);
        if let Status::Failed(msg) = &m.status
            && m.samples == 0
        {
            let failure = self.render_failure(msg, &m.log);
            return root.child(header).child(failure);
        }
        let breadcrumb = self.render_breadcrumb(&m, cx);
        if self.view == View::Rings {
            self.segments = Rc::new(sunburst::layout(&m, self.current));
            if self
                .hovered
                .is_some_and(|h| !self.segments.iter().any(|s| s.node == h))
            {
                self.hovered = None;
            }
        }
        let details = self.render_details(&m, cx);
        let empty = m.samples == 0;
        if self.view == View::Treemap {
            let (w, h) = self.chart_bounds.get().map_or((0.0, 0.0), |b| {
                (f32::from(b.size.width), f32::from(b.size.height))
            });
            self.tiles = Rc::new(treemap::layout(&m, self.current, w, h));
            self.tiles_size = (w, h);
            if self
                .hovered
                .is_some_and(|h| !self.tiles.iter().any(|t| t.node == h))
            {
                self.hovered = None;
            }
        }
        let chart = (self.view != View::List && !empty).then(|| self.render_chart(&m, cx));
        drop(m);

        let list = if let Some(chart) = chart {
            chart.into_any_element()
        } else if empty {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(label(
                    "Waiting for the first samples… (authenticate if prompted)",
                    theme::MUTED,
                ))
                .into_any_element()
        } else {
            uniform_list(
                "rows",
                self.rows.len(),
                cx.processor(|this, range, _window, cx| this.render_rows(range, cx)),
            )
            .track_scroll(self.scroll.clone())
            .flex_1()
            .w_full()
            .into_any_element()
        };

        let columns = (self.view == View::List).then(|| self.render_column_header(cx));
        root.child(header).child(breadcrumb).child(
            div()
                .flex_1()
                .flex()
                .min_h_0()
                .child(div().flex_1().flex().flex_col().min_h_0().children(columns).child(list))
                .child(details),
        )
        .child(
            div()
                .px_4()
                .py_1()
                .border_t_1()
                .border_color(rgb(theme::BORDER))
                .text_xs()
                .text_color(rgb(theme::FAINT))
                .child(
                    "click open · right-click/⌫ up · ↑↓ select · ⏎ open · v switch view · 1-5 sort · space pause · ctrl-c copy path · ctrl-q quit",
                ),
        )
    }
}
