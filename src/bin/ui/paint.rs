//! `nexus paint` (Super+P): an infinite whiteboard. Its process lives only while
//! the window is open, and only what is on screen gets drawn: the finished items
//! are kept as one picture, which what is being drawn goes over, and which a pan
//! only slides, painting the strip that comes into view.
use super::super::{hbox, vbox};
use gtk::{cairo, gdk, gio, glib, pango, prelude::*};
use nexus_control::theme;
use serde::{Deserialize, Serialize};
use std::{
    cell::{Cell, OnceCell, RefCell},
    collections::HashMap,
    fs, io,
    path::PathBuf,
    rc::Rc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const APP_ID: &str = "io.github.vitrixxl.NexusPaint";
const UNDO_STEPS: usize = 100;
const SAVE_AFTER: Duration = Duration::from_millis(800);
const MIN_ZOOM: f64 = 0.05;
const MAX_ZOOM: f64 = 30.0;
const THUMB: (i32, i32) = (320, 200);
const GRIP: f64 = 9.0;
/// How far a link stops short of a bubble.
const LINK_GAP: f64 = 5.0;
const UNTITLED: &str = "Untitled";

const STROKES: [u32; 6] = [0x1e1e1e, 0xe03131, 0x2f9e44, 0x1971c2, 0xf08c00, 0x868e96];
const FILLS: [Option<u32>; 6] = [
    None,
    Some(0xffc9c9),
    Some(0xb2f2bb),
    Some(0xa5d8ff),
    Some(0xffec99),
    Some(0xe9ecef),
];
const WIDTHS: [f64; 3] = [1.5, 3.0, 6.0];
/// The eraser's diameters, in pixels on screen.
const ERASERS: [f64; 3] = [12.0, 28.0, 56.0];
const SIZES: [(f64, &str); 4] = [(16.0, "S"), (22.0, "M"), (32.0, "L"), (48.0, "XL")];

const CSS: &str = "
window.paint { background: @nexus_bg; color: @nexus_fg; }
.paint-card { background: @nexus_surface; color: @nexus_fg; border-radius: 10px;
  box-shadow: 0 0 0 1px @nexus_edge, 0 6px 18px -4px @nexus_shadow; padding: 4px; }
.paint-card button { background: none; border: none; box-shadow: none; color: inherit;
  min-width: 34px; min-height: 34px; padding: 0; border-radius: 7px; font-weight: 500; }
.paint-card button:hover { background: alpha(@nexus_fg, .08); }
.paint-card button:active { background: alpha(@nexus_fg, .14); }
.paint-card button:disabled { color: alpha(@nexus_fg, .3); background: none; }
.paint-card button.on { background: alpha(@nexus_accent, .24); }
.paint-card separator { background: alpha(@nexus_fg, .12); min-width: 1px; margin: 6px 3px; }
.paint-accent { color: @nexus_accent; }
.paint-panel { padding: 12px; }
.paint-panel button { min-width: 28px; min-height: 28px; }
.paint-panel button.on { box-shadow: inset 0 0 0 1.5px @nexus_accent; background: none; }
.paint-heading { font-size: 12px; color: @nexus_muted; }
.paint-pill { padding: 2px 4px; }
.paint-pill button { padding: 0 8px; }
.paint-side { margin: 0 12px 12px 0; padding: 12px; }
.paint-side-title { font-weight: 600; }
.paint-side entry { background: alpha(@nexus_fg, .07); color: @nexus_fg; border: none;
  box-shadow: none; border-radius: 7px; min-height: 34px; }
.paint-side entry:focus-within { box-shadow: inset 0 0 0 1.5px @nexus_accent; }
.paint-side list { background: none; color: inherit; }
.paint-side row { background: none; border-radius: 9px; padding: 6px; margin-bottom: 4px; color: inherit; }
.paint-side row:hover { background: alpha(@nexus_fg, .07); }
.paint-side row.on { background: alpha(@nexus_accent, .22); }
.paint-side row picture { background: #ffffff; border-radius: 6px;
  box-shadow: 0 0 0 1px @nexus_edge; }
.paint-side row button { min-width: 28px; min-height: 28px; }
.paint-editor, .paint-editor text { background: none; padding: 0; }
window.paint.dark .paint-side row picture { filter: invert(1) hue-rotate(180deg); }
";

/// `id.json`: a header on the first line, then one item per line.
fn dir() -> PathBuf {
    glib::user_data_dir().join("nexus/boards")
}
fn file(id: &str, ext: &str) -> PathBuf {
    dir().join(format!("{id}.{ext}"))
}
fn fresh_id() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    format!("{ms:x}")
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct Area {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}
impl Area {
    fn of(pts: &[[f64; 2]]) -> Self {
        let mut a = Area {
            x0: f64::MAX,
            y0: f64::MAX,
            x1: f64::MIN,
            y1: f64::MIN,
        };
        for p in pts {
            a.x0 = a.x0.min(p[0]);
            a.y0 = a.y0.min(p[1]);
            a.x1 = a.x1.max(p[0]);
            a.y1 = a.y1.max(p[1]);
        }
        a
    }
    fn union(self, o: Area) -> Self {
        Area {
            x0: self.x0.min(o.x0),
            y0: self.y0.min(o.y0),
            x1: self.x1.max(o.x1),
            y1: self.y1.max(o.y1),
        }
    }
    fn grow(self, by: f64) -> Self {
        Area {
            x0: self.x0 - by,
            y0: self.y0 - by,
            x1: self.x1 + by,
            y1: self.y1 + by,
        }
    }
    fn contains(self, p: [f64; 2]) -> bool {
        p[0] >= self.x0 && p[0] <= self.x1 && p[1] >= self.y0 && p[1] <= self.y1
    }
    fn overlaps(self, o: Area) -> bool {
        self.x0 <= o.x1 && o.x0 <= self.x1 && self.y0 <= o.y1 && o.y0 <= self.y1
    }
    fn inside(self, o: Area) -> bool {
        self.x0 >= o.x0 && self.x1 <= o.x1 && self.y0 >= o.y0 && self.y1 <= o.y1
    }
    fn width(self) -> f64 {
        self.x1 - self.x0
    }
    fn height(self) -> f64 {
        self.y1 - self.y0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Rect,
    Diamond,
    Ellipse,
    Arrow,
    Line,
    Pen,
    Text,
}
impl Kind {
    fn closed(self) -> bool {
        matches!(self, Kind::Rect | Kind::Diamond | Kind::Ellipse)
    }
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize, Debug)]
struct Style {
    stroke: u32,
    fill: Option<u32>,
    width: f64,
    /// 0 solid, 1 dashed, 2 dotted.
    dash: u8,
    size: f64,
}
const DEFAULT_STYLE: Style = Style {
    stroke: STROKES[0],
    fill: None,
    width: WIDTHS[0],
    dash: 0,
    size: SIZES[1].0,
};

/// Two opposite corners or two ends, every point of a stroke, or a text's box.
#[derive(Serialize, Deserialize)]
struct Item {
    kind: Kind,
    style: Style,
    #[serde(serialize_with = "rounded")]
    pts: Vec<[f64; 2]>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    text: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    id: u64,
    /// The bubbles an arrow or a line is tied to, by `id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    to: Option<u64>,
    #[serde(skip)]
    bounds: OnceCell<Area>,
    #[serde(skip)]
    layout: RefCell<Option<pango::Layout>>,
}
fn is_zero(v: &u64) -> bool {
    *v == 0
}
fn rounded<S: serde::Serializer>(pts: &[[f64; 2]], s: S) -> Result<S::Ok, S::Error> {
    let near = |v: f64| (v * 100.0).round() / 100.0;
    pts.iter()
        .map(|p| [near(p[0]), near(p[1])])
        .collect::<Vec<_>>()
        .serialize(s)
}

fn distance_to_segment(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len = dx * dx + dy * dy;
    let t = if len == 0.0 {
        0.0
    } else {
        (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len).clamp(0.0, 1.0)
    };
    (p[0] - a[0] - t * dx).hypot(p[1] - a[1] - t * dy)
}

impl Item {
    fn new(kind: Kind, style: Style, pts: Vec<[f64; 2]>, text: String) -> Self {
        Self {
            kind,
            style,
            pts,
            text,
            id: 0,
            from: None,
            to: None,
            bounds: OnceCell::new(),
            layout: RefCell::new(None),
        }
    }
    /// The same item, links included, with this style, shape and text.
    fn with(&self, style: Style, pts: Vec<[f64; 2]>, text: String) -> Self {
        Self {
            id: self.id,
            from: self.from,
            to: self.to,
            ..Self::new(self.kind, style, pts, text)
        }
    }
    fn bbox(&self) -> Area {
        *self.bounds.get_or_init(|| Area::of(&self.pts))
    }
    fn moved(&self, dx: f64, dy: f64) -> Self {
        let pts = self.pts.iter().map(|p| [p[0] + dx, p[1] + dy]).collect();
        self.with(self.style, pts, self.text.clone())
    }
    fn scaled(&self, anchor: [f64; 2], sx: f64, sy: f64) -> Self {
        let mut pts: Vec<[f64; 2]> = self
            .pts
            .iter()
            .map(|p| {
                [
                    anchor[0] + (p[0] - anchor[0]) * sx,
                    anchor[1] + (p[1] - anchor[1]) * sy,
                ]
            })
            .collect();
        let mut style = self.style;
        if self.kind == Kind::Text {
            let b = Area::of(&pts);
            pts = vec![[b.x0, b.y0], [b.x1, b.y1]];
            style.size = (style.size * sx.abs()).max(1.0);
        }
        self.with(style, pts, self.text.clone())
    }
    fn styled(&self, style: Style, pc: &pango::Context) -> Self {
        let mut pts = self.pts.clone();
        if self.kind == Kind::Text {
            let (w, h) = text_size(pc, &self.text, style.size);
            pts[1] = [pts[0][0] + w, pts[0][1] + h];
        }
        self.with(style, pts, self.text.clone()).fitted(pc)
    }
    /// A bubble made as large as its text needs.
    fn fitted(self, pc: &pango::Context) -> Self {
        if !self.kind.closed() || self.text.is_empty() {
            return self;
        }
        let pts = fit_label(pc, self.kind, self.style.size, &self.pts, &self.text);
        self.with(self.style, pts, self.text.clone())
    }
    /// Whether `p` is on or within a bubble, `tol` around it.
    fn encloses(&self, p: [f64; 2], tol: f64) -> bool {
        let b = self.bbox().grow(tol);
        if !b.contains(p) {
            return false;
        }
        let (rx, ry) = ((b.width() / 2.0).max(0.01), (b.height() / 2.0).max(0.01));
        let (dx, dy) = ((p[0] - b.x0 - rx) / rx, (p[1] - b.y0 - ry) / ry);
        match self.kind {
            Kind::Rect => true,
            Kind::Ellipse => dx.hypot(dy) <= 1.0,
            Kind::Diamond => dx.abs() + dy.abs() <= 1.0,
            _ => false,
        }
    }

    /// Whether `p` touches it, `tol` world units around its line.
    fn hit(&self, p: [f64; 2], tol: f64) -> bool {
        let t = tol + self.style.width / 2.0;
        let b = self.bbox();
        if !b.grow(t).contains(p) {
            return false;
        }
        let (cx, cy) = ((b.x0 + b.x1) / 2.0, (b.y0 + b.y1) / 2.0);
        let (rx, ry) = ((b.width() / 2.0).max(0.01), (b.height() / 2.0).max(0.01));
        let (dx, dy) = ((p[0] - cx) / rx, (p[1] - cy) / ry);
        let near = |d: f64| (d - 1.0).abs() * rx.min(ry) <= t;
        let solid = self.style.fill.is_some() || !self.text.is_empty();
        match self.kind {
            Kind::Text => true,
            Kind::Rect => solid || !b.grow(-t).contains(p),
            Kind::Ellipse => {
                let d = dx.hypot(dy);
                (solid && d <= 1.0) || near(d)
            }
            Kind::Diamond => {
                let d = dx.abs() + dy.abs();
                (solid && d <= 1.0) || near(d)
            }
            Kind::Line | Kind::Arrow | Kind::Pen => {
                if self.pts.len() == 1 {
                    return distance_to_segment(p, self.pts[0], self.pts[0]) <= t;
                }
                self.pts
                    .windows(2)
                    .any(|s| distance_to_segment(p, s[0], s[1]) <= t)
            }
        }
    }

    fn paint(&self, cr: &cairo::Context, zoom: f64, pc: &pango::Context, dark: bool) {
        let s = self.style;
        if self.kind == Kind::Text {
            let mut slot = self.layout.borrow_mut();
            let layout = slot.get_or_insert_with(|| text_layout(pc, &self.text, s.size));
            ink(cr, s.stroke, dark);
            cr.move_to(self.pts[0][0], self.pts[0][1]);
            pangocairo::functions::show_layout(cr, layout);
            return;
        }
        cr.set_line_width(s.width);
        cr.set_line_cap(cairo::LineCap::Round);
        cr.set_line_join(cairo::LineJoin::Round);
        match (s.dash, self.kind) {
            (1, k) if k != Kind::Pen => cr.set_dash(&[s.width * 5.0, s.width * 3.5], 0.0),
            (2, k) if k != Kind::Pen => cr.set_dash(&[0.01, s.width * 2.8], 0.0),
            _ => cr.set_dash(&[], 0.0),
        }
        let area = Area::of(&self.pts[..self.pts.len().min(2)]);
        let (w, h) = (area.width(), area.height());
        cr.new_path();
        match self.kind {
            Kind::Rect => {
                let r = 6.0_f64.min(w / 2.0).min(h / 2.0);
                use std::f64::consts::{FRAC_PI_2, PI};
                cr.arc(area.x1 - r, area.y0 + r, r, -FRAC_PI_2, 0.0);
                cr.arc(area.x1 - r, area.y1 - r, r, 0.0, FRAC_PI_2);
                cr.arc(area.x0 + r, area.y1 - r, r, FRAC_PI_2, PI);
                cr.arc(area.x0 + r, area.y0 + r, r, PI, PI + FRAC_PI_2);
                cr.close_path();
            }
            Kind::Diamond => {
                let (cx, cy) = ((area.x0 + area.x1) / 2.0, (area.y0 + area.y1) / 2.0);
                cr.move_to(cx, area.y0);
                cr.line_to(area.x1, cy);
                cr.line_to(cx, area.y1);
                cr.line_to(area.x0, cy);
                cr.close_path();
            }
            Kind::Ellipse => {
                // A zero scale would put the whole context in error.
                if w < 0.1 || h < 0.1 {
                    return;
                }
                let m = cr.matrix();
                cr.translate((area.x0 + area.x1) / 2.0, (area.y0 + area.y1) / 2.0);
                cr.scale(w / 2.0, h / 2.0);
                cr.arc(0.0, 0.0, 1.0, 0.0, std::f64::consts::TAU);
                cr.set_matrix(m);
                cr.close_path();
            }
            Kind::Line | Kind::Arrow => {
                cr.move_to(self.pts[0][0], self.pts[0][1]);
                cr.line_to(self.pts[1][0], self.pts[1][1]);
            }
            Kind::Pen => smooth(cr, &self.pts, 0.75 / zoom),
            Kind::Text => unreachable!(),
        }
        if self.kind.closed()
            && let Some(fill) = s.fill
        {
            ink(cr, fill, dark);
            let _ = cr.fill_preserve();
        }
        ink(cr, s.stroke, dark);
        let _ = cr.stroke();
        if self.kind.closed() && !self.text.is_empty() {
            let room = inner(self.kind, area);
            let mut slot = self.layout.borrow_mut();
            let layout =
                slot.get_or_insert_with(|| label_layout(pc, &self.text, s.size, room.width()));
            let height = f64::from(layout.size().1) / f64::from(pango::SCALE);
            ink(cr, s.stroke, dark);
            cr.move_to(room.x0, (area.y0 + area.y1 - height) / 2.0);
            pangocairo::functions::show_layout(cr, layout);
        }
        if self.kind == Kind::Arrow {
            cr.set_dash(&[], 0.0);
            let (a, b) = (self.pts[0], self.pts[1]);
            let angle = (b[1] - a[1]).atan2(b[0] - a[0]);
            let len = (10.0 + s.width * 3.0).min((b[0] - a[0]).hypot(b[1] - a[1]) * 0.6);
            for side in [-0.48, 0.48] {
                cr.move_to(b[0], b[1]);
                cr.line_to(
                    b[0] - len * (angle + side).cos(),
                    b[1] - len * (angle + side).sin(),
                );
            }
            let _ = cr.stroke();
        }
    }
}

/// A board's colours are kept as chosen for a light board. On a dark one the same
/// hue is shown with its lightness turned over: black ink turns light and pale
/// fills turn deep, so that the board survives a change of theme.
fn shown(color: u32, dark: bool) -> u32 {
    if !dark {
        return color;
    }
    let part = |shift: u32| f64::from((color >> shift) & 0xff) / 255.0;
    let (r, g, b) = (part(16), part(8), part(0));
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let l = (max + min) / 2.0;
    let d = max - min;
    let sat = if d == 0.0 {
        0.0
    } else {
        d / (1.0 - (2.0 * l - 1.0).abs())
    };
    let hue = if d == 0.0 {
        0.0
    } else if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    let (l2, sat) = (
        0.14 + (1.0 - l) * 0.8,
        if l > 0.7 { sat * 0.6 } else { sat },
    );
    let c = (1.0 - (2.0 * l2 - 1.0).abs()) * sat;
    let x = c * (1.0 - (hue.rem_euclid(2.0) - 1.0).abs());
    let (r1, g1, b1) = match hue as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l2 - c / 2.0;
    let byte = |v: f64| (((v + m) * 255.0).round() as u32).min(255);
    byte(r1) << 16 | byte(g1) << 8 | byte(b1)
}

fn ink(cr: &cairo::Context, color: u32, dark: bool) {
    let color = shown(color, dark);
    let part = |shift: u32| f64::from((color >> shift) & 0xff) / 255.0;
    cr.set_source_rgb(part(16), part(8), part(0));
}

/// A curve through the middles of the segments. Points closer than `skip` to the
/// last one used would not show, so a zoomed-out board costs less.
fn smooth(cr: &cairo::Context, pts: &[[f64; 2]], skip: f64) {
    let mut at = pts[0];
    let mut control = pts[0];
    cr.move_to(at[0], at[1]);
    if pts.len() == 1 {
        cr.line_to(at[0], at[1]);
        return;
    }
    for p in &pts[1..] {
        if (p[0] - control[0]).abs() + (p[1] - control[1]).abs() < skip {
            continue;
        }
        let mid = [(control[0] + p[0]) / 2.0, (control[1] + p[1]) / 2.0];
        cr.curve_to(
            at[0] + (control[0] - at[0]) * 2.0 / 3.0,
            at[1] + (control[1] - at[1]) * 2.0 / 3.0,
            mid[0] + (control[0] - mid[0]) * 2.0 / 3.0,
            mid[1] + (control[1] - mid[1]) * 2.0 / 3.0,
            mid[0],
            mid[1],
        );
        at = mid;
        control = *p;
    }
    let end = pts[pts.len() - 1];
    cr.line_to(end[0], end[1]);
}

fn text_layout(pc: &pango::Context, text: &str, size: f64) -> pango::Layout {
    let layout = pango::Layout::new(pc);
    let mut font = pango::FontDescription::new();
    font.set_family("Sans");
    font.set_absolute_size(size * f64::from(pango::SCALE));
    layout.set_font_description(Some(&font));
    layout.set_text(text);
    layout
}
/// A bubble's text: wrapped to `width`, inside a word if it must, and centered.
fn label_layout(pc: &pango::Context, text: &str, size: f64, width: f64) -> pango::Layout {
    let layout = text_layout(pc, text, size);
    layout.set_width((width * f64::from(pango::SCALE)) as i32);
    layout.set_wrap(pango::WrapMode::WordChar);
    layout.set_alignment(pango::Alignment::Center);
    layout
}
/// The part of a bubble that text may use.
fn inner(kind: Kind, a: Area) -> Area {
    let (cx, cy) = ((a.x0 + a.x1) / 2.0, (a.y0 + a.y1) / 2.0);
    let (rx, ry) = (a.width() / 2.0, a.height() / 2.0);
    let (hx, hy) = match kind {
        Kind::Ellipse => (rx * 0.707 - 4.0, ry * 0.707 - 4.0),
        Kind::Diamond => (rx * 0.5 - 2.0, ry * 0.5 - 2.0),
        _ => (rx - 10.0, ry - 8.0),
    };
    let (hx, hy) = (hx.max(1.0), hy.max(1.0));
    Area {
        x0: cx - hx,
        y0: cy - hy,
        x1: cx + hx,
        y1: cy + hy,
    }
}
/// The half size of a bubble whose text area has this half size: the opposite of [`inner`].
fn around(kind: Kind, half: f64, flat: f64) -> f64 {
    match kind {
        Kind::Ellipse => (half + 4.0) / 0.707,
        Kind::Diamond => (half + 2.0) / 0.5,
        _ => half + flat,
    }
}
/// The corners of the bubble `pts` once `text` fits: taller as needed, around its
/// center. Its width is the one drawn, unless too narrow to write anything in.
fn fit_label(
    pc: &pango::Context,
    kind: Kind,
    size: f64,
    pts: &[[f64; 2]],
    text: &str,
) -> Vec<[f64; 2]> {
    let mut a = Area::of(&pts[..2]);
    if !text.is_empty() {
        let (cx, cy) = ((a.x0 + a.x1) / 2.0, (a.y0 + a.y1) / 2.0);
        let least = size * 2.0;
        if inner(kind, a).width() < least {
            let rx = around(kind, least / 2.0, 10.0);
            (a.x0, a.x1) = (cx - rx, cx + rx);
        }
        let room = inner(kind, a);
        let height = f64::from(label_layout(pc, text, size, room.width()).size().1)
            / f64::from(pango::SCALE);
        if height > room.height() {
            let ry = around(kind, (height + 1.0) / 2.0, 8.0);
            (a.y0, a.y1) = (cy - ry, cy + ry);
        }
    }
    vec![[a.x0, a.y0], [a.x1, a.y1]]
}
/// Where the way from a bubble's center to `toward` leaves it, `gap` further out.
fn border(shape: &Item, toward: [f64; 2], gap: f64) -> [f64; 2] {
    let b = shape.bbox();
    let (rx, ry) = (
        (b.width() / 2.0 + gap).max(0.5),
        (b.height() / 2.0 + gap).max(0.5),
    );
    let (cx, cy) = (b.x0 + b.width() / 2.0, b.y0 + b.height() / 2.0);
    let (dx, dy) = (toward[0] - cx, toward[1] - cy);
    let reach = match shape.kind {
        Kind::Rect => (dx.abs() / rx).max(dy.abs() / ry),
        Kind::Ellipse => (dx / rx).hypot(dy / ry),
        _ => dx.abs() / rx + dy.abs() / ry,
    };
    if reach == 0.0 {
        return [cx + rx, cy];
    }
    [cx + dx / reach, cy + dy / reach]
}
fn center(item: &Item) -> [f64; 2] {
    let b = item.bbox();
    [(b.x0 + b.x1) / 2.0, (b.y0 + b.y1) / 2.0]
}
/// The two ends of a link: on the edges of the bubbles it is tied to, facing each other.
fn routed(
    a: [f64; 2],
    b: [f64; 2],
    from: Option<&Item>,
    to: Option<&Item>,
    gap: f64,
) -> Vec<[f64; 2]> {
    let (look_a, look_b) = (from.map_or(a, center), to.map_or(b, center));
    vec![
        from.map_or(a, |s| border(s, look_b, gap)),
        to.map_or(b, |s| border(s, look_a, gap)),
    ]
}
fn is_link(kind: Kind) -> bool {
    matches!(kind, Kind::Arrow | Kind::Line)
}
fn text_size(pc: &pango::Context, text: &str, size: f64) -> (f64, f64) {
    let (w, h) = text_layout(pc, text, size).size();
    let unit = f64::from(pango::SCALE);
    (f64::from(w) / unit, f64::from(h) / unit)
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct View {
    x: f64,
    y: f64,
    zoom: f64,
}
const HOME: View = View {
    x: 0.0,
    y: 0.0,
    zoom: 1.0,
};
impl View {
    /// This view with the zoom changed and the world point under (sx, sy) kept there.
    fn zoomed(self, zoom: f64, sx: f64, sy: f64) -> Self {
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        View {
            x: self.x + sx / self.zoom - sx / zoom,
            y: self.y + sy / self.zoom - sy / zoom,
            zoom,
        }
    }
    fn showing(items: &[Rc<Item>], w: f64, h: f64, pad: f64, most: f64) -> Self {
        let Some(all) = items.iter().map(|i| i.bbox()).reduce(Area::union) else {
            return HOME;
        };
        let zoom = ((w - 2.0 * pad) / all.width().max(1.0))
            .min((h - 2.0 * pad) / all.height().max(1.0))
            .clamp(MIN_ZOOM.min(0.001), most);
        View {
            x: (all.x0 + all.x1) / 2.0 - w / 2.0 / zoom,
            y: (all.y0 + all.y1) / 2.0 - h / 2.0 / zoom,
            zoom,
        }
    }
}

/// Paints what falls in the `region` of the screen, except the sorted `hidden` indices.
fn render(
    cr: &cairo::Context,
    items: &[Rc<Item>],
    view: View,
    region: Area,
    hidden: &[usize],
    pc: &pango::Context,
    dark: bool,
) {
    let seen = Area {
        x0: view.x + region.x0 / view.zoom,
        y0: view.y + region.y0 / view.zoom,
        x1: view.x + region.x1 / view.zoom,
        y1: view.y + region.y1 / view.zoom,
    };
    let _ = cr.save();
    cr.scale(view.zoom, view.zoom);
    cr.translate(-view.x, -view.y);
    for (i, item) in items.iter().enumerate() {
        let b = item.bbox();
        // The margin leaves room for line width and arrow heads.
        if !b.grow(item.style.width + 32.0).overlaps(seen)
            || hidden.binary_search(&i).is_ok()
            || (b.width().max(b.height()) * view.zoom < 0.4 && item.kind != Kind::Pen)
        {
            continue;
        }
        item.paint(cr, view.zoom, pc, dark);
    }
    let _ = cr.restore();
}

/// The value of an `@define-color` in `css`.
fn token(css: &str, name: &str) -> Option<gdk::RGBA> {
    let (_, rest) = css.split_once(&format!("@define-color {name} "))?;
    gdk::RGBA::parse(rest.split(';').next()?.trim()).ok()
}

#[derive(Serialize, Deserialize)]
struct Head {
    name: String,
    view: [f64; 3],
}
fn encode(head: &Head, items: &[Rc<Item>]) -> Result<String, serde_json::Error> {
    let mut out = serde_json::to_string(head)?;
    for item in items {
        out.push('\n');
        out.push_str(&serde_json::to_string(&**item)?);
    }
    out.push('\n');
    Ok(out)
}
fn decode(text: &str) -> Result<(Head, Vec<Rc<Item>>), serde_json::Error> {
    let mut lines = text.lines();
    let head = serde_json::from_str(lines.next().unwrap_or(""))?;
    let items = lines
        .map(|l| serde_json::from_str(l).map(Rc::new))
        .collect::<Result<_, _>>()?;
    Ok((head, items))
}

struct Saved {
    id: String,
    name: String,
    modified: SystemTime,
}
/// Every board, newest first. Only the first line of each file is read.
fn boards() -> Vec<Saved> {
    use io::BufRead;
    let mut out = Vec::new();
    for entry in fs::read_dir(dir()).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Ok(f) = fs::File::open(&path) else {
            continue;
        };
        let mut line = String::new();
        if io::BufReader::new(f).read_line(&mut line).is_err() {
            continue;
        }
        let name = serde_json::from_str::<Head>(&line).map_or_else(|_| UNTITLED.into(), |h| h.name);
        out.push(Saved {
            id: path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            name,
            modified: entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH),
        });
    }
    out.sort_by_key(|b| std::cmp::Reverse(b.modified));
    out
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tool {
    Hand,
    Select,
    Shape(Kind),
    Eraser,
}
const TOOLS: [(Tool, &str, char); 10] = [
    (Tool::Hand, "Hand", 'h'),
    (Tool::Select, "Selection", 'v'),
    (Tool::Shape(Kind::Rect), "Rectangle", 'r'),
    (Tool::Shape(Kind::Diamond), "Diamond", 'd'),
    (Tool::Shape(Kind::Ellipse), "Ellipse", 'o'),
    (Tool::Shape(Kind::Arrow), "Arrow", 'a'),
    (Tool::Shape(Kind::Line), "Line", 'l'),
    (Tool::Shape(Kind::Pen), "Draw", 'p'),
    (Tool::Shape(Kind::Text), "Text", 't'),
    (Tool::Eraser, "Eraser", 'e'),
];

fn path(cr: &cairo::Context, points: &[(f64, f64)], close: bool) {
    cr.move_to(points[0].0, points[0].1);
    for p in &points[1..] {
        cr.line_to(p.0, p.1);
    }
    if close {
        cr.close_path();
    }
}
/// A tool's glyph, on a 20 unit square.
fn glyph(cr: &cairo::Context, tool: Tool) {
    match tool {
        Tool::Hand => {
            for (x, top) in [(7.0, 6.5), (10.0, 3.5), (13.0, 4.5), (16.0, 7.0)] {
                cr.move_to(x, 10.5);
                cr.line_to(x, top);
            }
            cr.move_to(16.0, 7.0);
            cr.line_to(16.0, 12.5);
            cr.curve_to(16.0, 16.0, 13.5, 18.0, 10.5, 18.0);
            cr.curve_to(7.5, 18.0, 6.5, 16.5, 5.0, 14.0);
            cr.line_to(3.2, 11.0);
            cr.curve_to(2.6, 9.6, 4.6, 8.6, 5.6, 9.8);
            cr.line_to(7.0, 11.5);
        }
        Tool::Select => path(
            cr,
            &[
                (5.0, 3.0),
                (5.0, 16.5),
                (8.8, 12.9),
                (14.0, 12.6),
                (5.0, 3.0),
            ],
            true,
        ),
        Tool::Shape(Kind::Rect) => cr.rectangle(3.5, 4.5, 13.0, 11.0),
        Tool::Shape(Kind::Diamond) => path(
            cr,
            &[(10.0, 2.8), (17.2, 10.0), (10.0, 17.2), (2.8, 10.0)],
            true,
        ),
        Tool::Shape(Kind::Ellipse) => cr.arc(10.0, 10.0, 7.0, 0.0, std::f64::consts::TAU),
        Tool::Shape(Kind::Arrow) => {
            path(cr, &[(3.5, 16.5), (16.0, 4.0)], false);
            path(cr, &[(9.0, 4.0), (16.0, 4.0), (16.0, 11.0)], false);
        }
        Tool::Shape(Kind::Line) => path(cr, &[(3.5, 16.5), (16.5, 3.5)], false),
        Tool::Shape(Kind::Pen) => path(
            cr,
            &[
                (3.5, 16.5),
                (4.4, 12.6),
                (13.0, 4.0),
                (16.0, 7.0),
                (7.4, 15.6),
            ],
            true,
        ),
        Tool::Shape(Kind::Text) => {
            path(
                cr,
                &[(4.5, 6.5), (4.5, 4.0), (15.5, 4.0), (15.5, 6.5)],
                false,
            );
            path(cr, &[(10.0, 4.0), (10.0, 16.5)], false);
            path(cr, &[(7.5, 16.5), (12.5, 16.5)], false);
        }
        Tool::Eraser => {
            path(
                cr,
                &[
                    (7.8, 16.2),
                    (3.2, 11.6),
                    (11.2, 3.6),
                    (16.8, 9.2),
                    (9.8, 16.2),
                ],
                true,
            );
            path(cr, &[(6.8, 8.0), (12.4, 13.6)], false);
            path(cr, &[(9.8, 16.2), (17.0, 16.2)], false);
        }
    }
}
fn glyph_view(tool: Tool) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_content_width(20);
    area.set_content_height(20);
    area.set_halign(gtk::Align::Center);
    area.set_valign(gtk::Align::Center);
    area.set_draw_func(move |area, cr, _, _| {
        let c = area.color();
        cr.set_source_rgba(c.red().into(), c.green().into(), c.blue().into(), 1.0);
        cr.set_line_width(1.5);
        cr.set_line_cap(cairo::LineCap::Round);
        cr.set_line_join(cairo::LineJoin::Round);
        glyph(cr, tool);
        let _ = cr.stroke();
    });
    area
}
/// A flat sample of a colour, as the board shows it; `None` is struck through.
fn swatch(color: Option<u32>, dark: Rc<Cell<bool>>) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_content_width(18);
    area.set_content_height(18);
    area.set_halign(gtk::Align::Center);
    area.set_valign(gtk::Align::Center);
    area.set_draw_func(move |area, cr, w, h| {
        let (w, h) = (f64::from(w), f64::from(h));
        let edge = area.color();
        let outline = |alpha: f64| {
            cr.set_source_rgba(
                edge.red().into(),
                edge.green().into(),
                edge.blue().into(),
                alpha,
            );
        };
        cr.rectangle(0.5, 0.5, w - 1.0, h - 1.0);
        cr.set_line_width(1.0);
        match color {
            Some(c) => {
                ink(cr, c, dark.get());
                let _ = cr.fill_preserve();
                outline(0.16);
                let _ = cr.stroke();
            }
            None => {
                outline(0.32);
                let _ = cr.stroke();
                cr.set_source_rgb(0.88, 0.19, 0.19);
                cr.move_to(2.0, h - 2.0);
                cr.line_to(w - 2.0, 2.0);
                let _ = cr.stroke();
            }
        }
    });
    area
}
/// A sample of the eraser: a disc that grows with its size.
fn disc_sample(radius: f64) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_content_width(20);
    area.set_content_height(20);
    area.set_halign(gtk::Align::Center);
    area.set_valign(gtk::Align::Center);
    area.set_draw_func(move |area, cr, w, h| {
        let c = area.color();
        cr.set_source_rgba(c.red().into(), c.green().into(), c.blue().into(), 1.0);
        cr.set_line_width(1.5);
        cr.arc(
            f64::from(w) / 2.0,
            f64::from(h) / 2.0,
            radius,
            0.0,
            std::f64::consts::TAU,
        );
        let _ = cr.stroke();
    });
    area
}
/// A sample of a stroke: its width, or its dashes.
fn stroke_sample(width: f64, dash: u8) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_content_width(20);
    area.set_content_height(18);
    area.set_halign(gtk::Align::Center);
    area.set_valign(gtk::Align::Center);
    area.set_draw_func(move |area, cr, w, h| {
        let c = area.color();
        cr.set_source_rgba(c.red().into(), c.green().into(), c.blue().into(), 1.0);
        cr.set_line_width(width);
        cr.set_line_cap(cairo::LineCap::Round);
        match dash {
            1 => cr.set_dash(&[5.0, 4.5], 0.0),
            2 => cr.set_dash(&[0.01, 4.5], 0.0),
            _ => {}
        }
        let y = f64::from(h) / 2.0;
        cr.move_to(2.0, y);
        cr.line_to(f64::from(w) - 2.0, y);
        let _ = cr.stroke();
    });
    area
}

fn debounce(
    slot: &Rc<RefCell<Option<glib::SourceId>>>,
    after: Duration,
    then: impl FnOnce() + 'static,
) {
    if let Some(id) = slot.borrow_mut().take() {
        id.remove();
    }
    let again = slot.clone();
    let id = glib::timeout_add_local_once(after, move || {
        again.borrow_mut().take();
        then();
    });
    *slot.borrow_mut() = Some(id);
}

enum Grab {
    Corner { anchor: [f64; 2], grip: [f64; 2] },
    End { index: usize, which: usize },
}

enum Drag {
    Idle,
    Pan(View),
    Create,
    Band([f64; 2], [f64; 2]),
    Move([f64; 2], Vec<(usize, Rc<Item>)>),
    Resize {
        press: [f64; 2],
        anchor: [f64; 2],
        grip: [f64; 2],
        was: Vec<(usize, Rc<Item>)>,
    },
    End {
        index: usize,
        which: usize,
    },
    Erase([f64; 2]),
}

struct Scene {
    surface: cairo::ImageSurface,
    size: (i32, i32),
    scale: f64,
    view: View,
}

struct Editing {
    at: [f64; 2],
    index: Option<usize>,
    style: Style,
    /// The bubble as it was, when writing inside one.
    bubble: Option<Rc<Item>>,
}

type Check = Box<dyn Fn(&Style) -> bool>;

struct Paint {
    window: gtk::ApplicationWindow,
    area: gtk::DrawingArea,
    overlay: gtk::Overlay,
    pc: pango::Context,
    css: gtk::CssProvider,
    dark: Rc<Cell<bool>>,
    canvas: Cell<(f64, f64, f64)>,
    items: RefCell<Vec<Rc<Item>>>,
    selected: RefCell<Vec<usize>>,
    undo: RefCell<Vec<Vec<Rc<Item>>>>,
    redo: RefCell<Vec<Vec<Rc<Item>>>>,
    pending: RefCell<Option<Vec<Rc<Item>>>>,
    clip: RefCell<Vec<Rc<Item>>>,
    view: Cell<View>,
    tool: Cell<Tool>,
    style: Cell<Style>,
    drag: RefCell<Drag>,
    live: RefCell<Option<Item>>,
    next_id: Cell<u64>,
    /// The links that follow what is being moved.
    linked: RefCell<Vec<usize>>,
    /// The bubble a link would tie to if released now.
    hint: Cell<Option<usize>>,
    changed: Cell<bool>,
    start: Cell<(f64, f64)>,
    last_press: Cell<(u32, f64, f64)>,
    pinch_from: Cell<f64>,
    pointer: Cell<(f64, f64)>,
    space: Cell<bool>,
    scene: RefCell<Option<Scene>>,
    scene_stale: Cell<bool>,
    board: RefCell<String>,
    dirty: Cell<bool>,
    save_timer: Rc<RefCell<Option<glib::SourceId>>>,
    editing: RefCell<Option<Editing>>,
    editor: gtk::TextView,
    editor_css: gtk::CssProvider,
    tool_buttons: RefCell<Vec<(Tool, gtk::Button)>>,
    options: RefCell<Vec<(gtk::Button, Check)>>,
    panel: gtk::Box,
    stroke_row: gtk::Box,
    eraser_row: gtk::Box,
    eraser: Cell<f64>,
    eraser_buttons: RefCell<Vec<(gtk::Button, f64)>>,
    inside: Cell<bool>,
    fill_row: gtk::Box,
    line_row: gtk::Box,
    font_row: gtk::Box,
    order_row: gtk::Box,
    zoom_label: gtk::Button,
    undo_button: gtk::Button,
    redo_button: gtk::Button,
    save_button: gtk::Button,
    notice: gtk::Label,
    notice_timer: Rc<RefCell<Option<glib::SourceId>>>,
    side: gtk::Revealer,
    list: gtk::ListBox,
    name: gtk::Entry,
    naming: Cell<bool>,
    current_title: RefCell<Option<gtk::Label>>,
}

pub fn run() -> anyhow::Result<()> {
    glib::set_prgname(Some(APP_ID));
    let app = gtk::Application::builder().application_id(APP_ID).build();
    // Everything here is drawn by the CPU anyway, and GL would cost tens of MB of driver.
    // SAFETY: no other thread exists yet.
    unsafe { std::env::set_var("GSK_RENDERER", "cairo") };
    app.connect_activate(|app| {
        if let Some(window) = app.active_window() {
            return window.present();
        }
        Paint::new(app).window.present();
    });
    app.run_with_args::<&str>(&[]);
    Ok(())
}

fn empty(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}
fn heading(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("paint-heading");
    label.set_halign(gtk::Align::Start);
    label
}
fn flat(button: &gtk::Button) {
    button.set_focusable(false);
}
fn card(orientation: gtk::Orientation, spacing: i32) -> gtk::Box {
    let b = gtk::Box::new(orientation, spacing);
    b.add_css_class("paint-card");
    b
}

impl Paint {
    fn new(app: &gtk::Application) -> Rc<Self> {
        let css = gtk::CssProvider::new();
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Paint")
            .default_width(1280)
            .default_height(800)
            .decorated(false)
            .css_classes(["paint"])
            .build();
        let area = gtk::DrawingArea::new();
        area.set_focusable(true);
        area.add_css_class("paint-accent");
        let pc = area.create_pango_context();
        if let Ok(mut options) = cairo::FontOptions::new() {
            // Letters must keep their shape and spacing at every zoom.
            options.set_hint_metrics(cairo::HintMetrics::Off);
            options.set_hint_style(cairo::HintStyle::None);
            pangocairo::functions::context_set_font_options(&pc, Some(&options));
        }
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&area));
        window.set_child(Some(&overlay));

        let editor = gtk::TextView::new();
        editor.add_css_class("paint-editor");
        editor.set_halign(gtk::Align::Start);
        editor.set_valign(gtk::Align::Start);
        editor.set_visible(false);
        overlay.add_overlay(&editor);
        let editor_css = gtk::CssProvider::new();
        #[allow(deprecated)]
        editor
            .style_context()
            .add_provider(&editor_css, gtk::STYLE_PROVIDER_PRIORITY_USER);

        let panel = card(gtk::Orientation::Vertical, 10);
        panel.add_css_class("paint-panel");
        let zoom_label = gtk::Button::with_label("100%");
        let undo_button = gtk::Button::from_icon_name("edit-undo-symbolic");
        let redo_button = gtk::Button::from_icon_name("edit-redo-symbolic");
        let save_button = gtk::Button::with_label("Saved");
        let notice = gtk::Label::new(None);
        let list = gtk::ListBox::new();
        let name = gtk::Entry::new();

        let this = Rc::new(Self {
            window,
            area,
            overlay,
            pc,
            css,
            dark: Rc::default(),
            canvas: Cell::new((1.0, 1.0, 1.0)),
            items: RefCell::default(),
            selected: RefCell::default(),
            undo: RefCell::default(),
            redo: RefCell::default(),
            pending: RefCell::default(),
            clip: RefCell::default(),
            view: Cell::new(HOME),
            tool: Cell::new(Tool::Shape(Kind::Pen)),
            style: Cell::new(DEFAULT_STYLE),
            drag: RefCell::new(Drag::Idle),
            live: RefCell::default(),
            next_id: Cell::new(1),
            linked: RefCell::default(),
            hint: Cell::new(None),
            changed: Cell::new(false),
            start: Cell::new((0.0, 0.0)),
            last_press: Cell::new((0, -99.0, -99.0)),
            pinch_from: Cell::new(1.0),
            pointer: Cell::new((0.0, 0.0)),
            space: Cell::new(false),
            scene: RefCell::default(),
            scene_stale: Cell::new(true),
            board: RefCell::new(fresh_id()),
            dirty: Cell::new(false),
            save_timer: Rc::default(),
            editing: RefCell::default(),
            editor,
            editor_css,
            tool_buttons: RefCell::default(),
            options: RefCell::default(),
            panel,
            stroke_row: vbox(6),
            eraser_row: vbox(6),
            eraser: Cell::new(ERASERS[1]),
            eraser_buttons: RefCell::default(),
            inside: Cell::new(false),
            fill_row: vbox(6),
            line_row: vbox(10),
            font_row: vbox(6),
            order_row: vbox(6),
            zoom_label,
            undo_button,
            redo_button,
            save_button,
            notice,
            notice_timer: Rc::default(),
            side: gtk::Revealer::new(),
            list,
            name,
            naming: Cell::new(false),
            current_title: RefCell::default(),
        });
        this.apply_theme();
        this.follow_theme();
        this.build_toolbar();
        this.build_panel();
        this.build_corners();
        this.build_sidebar();
        this.wire_canvas();
        this.wire_keys();
        if let Some(latest) = boards().first() {
            this.open_board(&latest.id.clone());
        }
        let on_close = this.clone();
        this.window.connect_close_request(move |_| {
            on_close.finish_text();
            on_close.save(true);
            on_close.write_thumb();
            glib::Propagation::Proceed
        });
        this.sync();
        this.area.grab_focus();
        this
    }

    /// Takes the colours of the Nexus theme, the board's included.
    fn apply_theme(self: &Rc<Self>) {
        let settings = theme::load();
        let sheet = theme::css(&settings);
        // Only the theme's colours: the board has no use for the rest of its sheet.
        let colors: String = sheet
            .lines()
            .filter(|l| l.starts_with("@define-color"))
            .collect();
        self.css.load_from_data(&format!("{colors}{CSS}"));
        let dark = settings.dark();
        self.dark.set(dark);
        if let Some(gtk_settings) = gtk::Settings::default()
            && let Some(spec) = gtk_settings.find_property("gtk-interface-color-scheme")
            && let Some(value) = glib::EnumClass::with_type(spec.value_type())
                .and_then(|c| c.to_value_by_nick(if dark { "dark" } else { "light" }))
        {
            gtk_settings.set_property_from_value("gtk-interface-color-scheme", &value);
        }
        if dark {
            self.window.add_css_class("dark");
        } else {
            self.window.remove_css_class("dark");
        }
        if let Some(bg) = token(&colors, "nexus_bg") {
            self.canvas
                .set((bg.red().into(), bg.green().into(), bg.blue().into()));
        }
        for (button, _) in &*self.options.borrow() {
            if let Some(child) = button.child() {
                child.queue_draw();
            }
        }
        self.scene_stale.set(true);
        self.area.queue_draw();
    }

    fn follow_theme(self: &Rc<Self>) {
        let file = gio::File::for_path(theme::config_dir().join("settings.json"));
        let Ok(monitor) = file.monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)
        else {
            return;
        };
        let this = self.clone();
        monitor.connect_changed(move |_, _, _, _| this.apply_theme());
        // Lives as long as the process.
        std::mem::forget(monitor);
    }

    fn build_toolbar(self: &Rc<Self>) {
        let bar = card(gtk::Orientation::Horizontal, 2);
        bar.set_halign(gtk::Align::Center);
        bar.set_valign(gtk::Align::Start);
        bar.set_margin_top(14);
        for (i, (tool, name, key)) in TOOLS.into_iter().enumerate() {
            let button = gtk::Button::new();
            button.set_child(Some(&glyph_view(tool)));
            button.set_tooltip_text(Some(&format!("{name} · {}", key.to_ascii_uppercase())));
            flat(&button);
            let this = self.clone();
            button.connect_clicked(move |_| this.set_tool(tool));
            bar.append(&button);
            self.tool_buttons.borrow_mut().push((tool, button));
            if i == 1 {
                bar.append(&gtk::Separator::new(gtk::Orientation::Vertical));
            }
        }
        self.overlay.add_overlay(&bar);
    }

    fn option(
        self: &Rc<Self>,
        row: &gtk::Box,
        child: &impl IsA<gtk::Widget>,
        is: impl Fn(&Style) -> bool + 'static,
        set: impl Fn(&mut Style) + 'static,
    ) {
        let button = gtk::Button::new();
        button.set_child(Some(child));
        flat(&button);
        let this = self.clone();
        button.connect_clicked(move |_| {
            let mut style = this.style.get();
            set(&mut style);
            this.style.set(style);
            let picked = this.selected.borrow().clone();
            if !picked.is_empty() {
                this.begin_change();
                let mut items = this.items.borrow_mut();
                for &i in &picked {
                    let mut s = items[i].style;
                    set(&mut s);
                    items[i] = Rc::new(items[i].styled(s, &this.pc));
                }
                drop(items);
                this.reroute(None);
                this.commit_change();
            }
            this.sync();
        });
        row.append(&button);
        self.options.borrow_mut().push((button, Box::new(is)));
    }

    fn build_panel(self: &Rc<Self>) {
        let strokes = hbox(2);
        for c in STROKES {
            self.option(
                &strokes,
                &swatch(Some(c), self.dark.clone()),
                move |s| s.stroke == c,
                move |s| s.stroke = c,
            );
        }
        let fills = hbox(2);
        for c in FILLS {
            self.option(
                &fills,
                &swatch(c, self.dark.clone()),
                move |s| s.fill == c,
                move |s| s.fill = c,
            );
        }
        let widths = hbox(2);
        for w in WIDTHS {
            self.option(
                &widths,
                &stroke_sample(w, 0),
                move |s| s.width == w,
                move |s| s.width = w,
            );
        }
        let dashes = hbox(2);
        for d in 0..3u8 {
            self.option(
                &dashes,
                &stroke_sample(2.0, d),
                move |s| s.dash == d,
                move |s| s.dash = d,
            );
        }
        let fonts = hbox(2);
        for (size, name) in SIZES {
            self.option(
                &fonts,
                &gtk::Label::new(Some(name)),
                move |s| s.size == size,
                move |s| s.size = size,
            );
        }
        let order = hbox(2);
        for (name, front) in [("To front", true), ("To back", false)] {
            let button = gtk::Button::with_label(name);
            button.set_hexpand(true);
            flat(&button);
            let this = self.clone();
            button.connect_clicked(move |_| this.reorder(front));
            order.append(&button);
        }
        self.stroke_row.append(&heading("Stroke"));
        self.stroke_row.append(&strokes);
        let sizes = hbox(2);
        for (size, radius) in ERASERS.into_iter().zip([3.0, 5.5, 8.5]) {
            let button = gtk::Button::new();
            button.set_child(Some(&disc_sample(radius)));
            flat(&button);
            let this = self.clone();
            button.connect_clicked(move |_| {
                this.eraser.set(size);
                this.sync();
            });
            sizes.append(&button);
            self.eraser_buttons.borrow_mut().push((button, size));
        }
        self.eraser_row.append(&heading("Eraser size"));
        self.eraser_row.append(&sizes);
        self.fill_row.append(&heading("Background"));
        self.fill_row.append(&fills);
        let width_row = vbox(6);
        width_row.append(&heading("Stroke width"));
        width_row.append(&widths);
        let dash_row = vbox(6);
        dash_row.append(&heading("Stroke style"));
        dash_row.append(&dashes);
        self.line_row.append(&width_row);
        self.line_row.append(&dash_row);
        self.font_row.append(&heading("Font size"));
        self.font_row.append(&fonts);
        self.order_row.append(&heading("Layers"));
        self.order_row.append(&order);
        for row in [
            &self.stroke_row,
            &self.eraser_row,
            &self.fill_row,
            &self.line_row,
            &self.font_row,
            &self.order_row,
        ] {
            self.panel.append(row);
        }
        self.panel.set_halign(gtk::Align::Start);
        self.panel.set_valign(gtk::Align::Start);
        self.panel.set_margin_start(14);
        self.panel.set_margin_top(14);
        self.overlay.add_overlay(&self.panel);
    }

    fn build_corners(self: &Rc<Self>) {
        let bottom = hbox(8);
        bottom.set_halign(gtk::Align::Start);
        bottom.set_valign(gtk::Align::End);
        bottom.set_margin_start(14);
        bottom.set_margin_bottom(14);
        let zoom = card(gtk::Orientation::Horizontal, 0);
        zoom.add_css_class("paint-pill");
        let out = gtk::Button::with_label("−");
        let inn = gtk::Button::with_label("+");
        out.set_tooltip_text(Some("Zoom out · Ctrl+-"));
        inn.set_tooltip_text(Some("Zoom in · Ctrl++"));
        self.zoom_label
            .set_tooltip_text(Some("Show everything · Shift+1"));
        for (button, factor) in [(&out, 0.8), (&inn, 1.25)] {
            flat(button);
            let this = self.clone();
            button.connect_clicked(move |_| this.zoom_center(factor));
        }
        flat(&self.zoom_label);
        let this = self.clone();
        self.zoom_label.connect_clicked(move |_| this.fit());
        zoom.append(&out);
        zoom.append(&self.zoom_label);
        zoom.append(&inn);
        let history = card(gtk::Orientation::Horizontal, 0);
        history.add_css_class("paint-pill");
        for (button, tip, back) in [
            (&self.undo_button, "Undo · Ctrl+Z", true),
            (&self.redo_button, "Redo · Ctrl+Shift+Z", false),
        ] {
            button.set_tooltip_text(Some(tip));
            flat(button);
            let this = self.clone();
            button.connect_clicked(move |_| this.history(back));
            history.append(button);
        }
        bottom.append(&zoom);
        bottom.append(&history);
        self.overlay.add_overlay(&bottom);

        let top = card(gtk::Orientation::Horizontal, 2);
        top.add_css_class("paint-pill");
        top.set_halign(gtk::Align::End);
        top.set_valign(gtk::Align::Start);
        top.set_margin_end(12);
        top.set_margin_top(14);
        self.save_button.set_tooltip_text(Some("Save · Ctrl+S"));
        flat(&self.save_button);
        let this = self.clone();
        self.save_button.connect_clicked(move |_| {
            this.finish_text();
            this.save(true);
        });
        let boards = gtk::Button::with_label("Boards");
        flat(&boards);
        let this = self.clone();
        boards.connect_clicked(move |_| this.toggle_sidebar());
        top.append(&self.save_button);
        top.append(&boards);
        self.overlay.add_overlay(&top);

        self.notice.add_css_class("paint-card");
        self.notice.set_halign(gtk::Align::Center);
        self.notice.set_valign(gtk::Align::End);
        self.notice.set_margin_bottom(14);
        self.notice.set_visible(false);
        self.overlay.add_overlay(&self.notice);
    }

    fn build_sidebar(self: &Rc<Self>) {
        let side = card(gtk::Orientation::Vertical, 10);
        side.add_css_class("paint-side");
        side.set_size_request(264, -1);
        let head = hbox(6);
        let title = gtk::Label::new(Some("Boards"));
        title.add_css_class("paint-side-title");
        title.set_halign(gtk::Align::Start);
        title.set_hexpand(true);
        let add = gtk::Button::from_icon_name("list-add-symbolic");
        add.set_tooltip_text(Some("New board"));
        flat(&add);
        let this = self.clone();
        add.connect_clicked(move |_| this.new_board());
        head.append(&title);
        head.append(&add);
        self.name.set_placeholder_text(Some(UNTITLED));
        let this = self.clone();
        self.name.connect_changed(move |entry| {
            if this.naming.get() {
                return;
            }
            if let Some(label) = &*this.current_title.borrow() {
                label.set_text(&Self::title_of(&entry.text()));
            }
            this.mark_dirty();
        });
        self.list.set_selection_mode(gtk::SelectionMode::None);
        self.list.set_activate_on_single_click(true);
        let this = self.clone();
        self.list.connect_row_activated(move |_, row| {
            let id = row.widget_name().to_string();
            if id != *this.board.borrow() {
                this.open_board(&id);
                this.fill_list();
            }
        });
        let scroll = gtk::ScrolledWindow::builder()
            .child(&self.list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        side.append(&head);
        side.append(&self.name);
        side.append(&scroll);
        self.side.set_child(Some(&side));
        self.side
            .set_transition_type(gtk::RevealerTransitionType::SlideLeft);
        self.side.set_transition_duration(160);
        self.side.set_halign(gtk::Align::End);
        self.side.set_valign(gtk::Align::Fill);
        self.side.set_margin_top(66);
        self.side.set_can_target(true);
        let this = self.clone();
        // The previews are only in memory while the list is out.
        self.side.connect_child_revealed_notify(move |side| {
            if !side.reveals_child() {
                empty(&this.list);
                *this.current_title.borrow_mut() = None;
                side.set_visible(false);
            }
        });
        self.side.set_visible(false);
        self.overlay.add_overlay(&self.side);
    }

    fn toggle_sidebar(self: &Rc<Self>) {
        let open = !self.side.reveals_child();
        if open {
            self.write_thumb();
            self.fill_list();
            self.side.set_visible(true);
        }
        self.side.set_reveal_child(open);
    }

    fn title_of(name: &str) -> String {
        if name.trim().is_empty() {
            UNTITLED.into()
        } else {
            name.to_owned()
        }
    }

    // ponytail: every preview loads when the list opens; a ListView if hundreds of boards appear.
    fn fill_list(self: &Rc<Self>) {
        empty(&self.list);
        *self.current_title.borrow_mut() = None;
        let current = self.board.borrow().clone();
        let mut all = boards();
        if !all.iter().any(|b| b.id == current) {
            all.insert(
                0,
                Saved {
                    id: current.clone(),
                    name: self.name.text().to_string(),
                    modified: SystemTime::now(),
                },
            );
        }
        for board in all {
            let row = gtk::ListBoxRow::new();
            row.set_widget_name(&board.id);
            row.set_focusable(false);
            let body = vbox(6);
            let picture = gtk::Picture::for_filename(file(&board.id, "png"));
            picture.set_size_request(-1, 118);
            picture.set_content_fit(gtk::ContentFit::Contain);
            picture.set_can_shrink(true);
            let line = hbox(4);
            let title = gtk::Label::new(Some(&Self::title_of(&board.name)));
            title.set_halign(gtk::Align::Start);
            title.set_hexpand(true);
            title.set_ellipsize(pango::EllipsizeMode::End);
            let remove = gtk::Button::from_icon_name("user-trash-symbolic");
            remove.set_tooltip_text(Some("Move to Trash"));
            flat(&remove);
            let this = self.clone();
            let id = board.id.clone();
            remove.connect_clicked(move |_| this.delete_board(&id));
            line.append(&title);
            line.append(&remove);
            body.append(&picture);
            body.append(&line);
            row.set_child(Some(&body));
            if board.id == current {
                row.add_css_class("on");
                *self.current_title.borrow_mut() = Some(title);
            }
            self.list.append(&row);
        }
    }

    fn say(self: &Rc<Self>, text: &str) {
        self.notice.set_text(text);
        self.notice.set_visible(true);
        let notice = self.notice.clone();
        debounce(&self.notice_timer, Duration::from_secs(4), move || {
            notice.set_visible(false)
        });
    }

    fn mark_dirty(self: &Rc<Self>) {
        self.dirty.set(true);
        self.save_button.set_label("Save");
        self.save_button.set_sensitive(true);
        let this = self.clone();
        debounce(&self.save_timer, SAVE_AFTER, move || this.save(false));
    }

    /// Writes the board if it changed, or anyway with `force`. A board nobody drew on stays unwritten.
    fn save(self: &Rc<Self>, force: bool) {
        if !self.dirty.get() && !force {
            return;
        }
        let id = self.board.borrow().clone();
        let path = file(&id, "json");
        let v = self.view.get();
        let text = encode(
            &Head {
                name: Self::title_of(&self.name.text()),
                view: [v.x, v.y, v.zoom],
            },
            &self.items.borrow(),
        );
        let wrote = if self.items.borrow().is_empty() && !path.exists() {
            Ok(())
        } else {
            let part = file(&id, "part");
            text.map_err(io::Error::other).and_then(|text| {
                fs::create_dir_all(dir())?;
                fs::write(&part, text)?;
                fs::rename(&part, &path)
            })
        };
        match wrote {
            Ok(()) => {
                self.dirty.set(false);
                self.save_button.set_label("Saved");
                self.save_button.set_sensitive(false);
            }
            Err(e) => self.say(&format!("Could not save this board: {e}")),
        }
    }

    fn write_thumb(&self) {
        let items = self.items.borrow();
        let id = self.board.borrow().clone();
        if items.is_empty() {
            let _ = fs::remove_file(file(&id, "png"));
            return;
        }
        let (w, h) = THUMB;
        let Ok(mut surface) = cairo::ImageSurface::create(cairo::Format::Rgb24, w, h) else {
            return;
        };
        if let Ok(cr) = cairo::Context::new(&surface) {
            cr.set_source_rgb(1.0, 1.0, 1.0);
            let _ = cr.paint();
            let (fw, fh) = (f64::from(w), f64::from(h));
            let view = View::showing(&items, fw, fh, 14.0, 1.0);
            let all = Area {
                x0: 0.0,
                y0: 0.0,
                x1: fw,
                y1: fh,
            };
            render(&cr, &items, view, all, &[], &self.pc, false);
        }
        surface.flush();
        let stride = surface.stride() as usize;
        let Ok(data) = surface.data() else {
            return;
        };
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for line in data.chunks(stride) {
            for px in line[..w as usize * 4].chunks(4) {
                rgb.extend([px[2], px[1], px[0]]);
            }
        }
        let _ = fs::create_dir_all(dir());
        let _ = image::save_buffer(
            file(&id, "png"),
            &rgb,
            w as u32,
            h as u32,
            image::ExtendedColorType::Rgb8,
        );
    }

    fn show_board(self: &Rc<Self>, id: String, name: &str, items: Vec<Rc<Item>>, view: View) {
        *self.board.borrow_mut() = id;
        let top = items.iter().map(|i| i.id).max().unwrap_or(0);
        self.next_id.set(top + 1);
        *self.items.borrow_mut() = items;
        self.selected.borrow_mut().clear();
        self.scene_stale.set(true);
        self.undo.borrow_mut().clear();
        self.redo.borrow_mut().clear();
        self.view.set(view);
        self.naming.set(true);
        self.name.set_text(if name == UNTITLED { "" } else { name });
        self.naming.set(false);
        self.dirty.set(false);
        self.save_button.set_label("Saved");
        self.save_button.set_sensitive(false);
        self.sync_view();
        self.sync();
    }

    fn leave_board(self: &Rc<Self>) {
        self.finish_text();
        self.save(true);
        self.write_thumb();
    }

    fn open_board(self: &Rc<Self>, id: &str) {
        match fs::read_to_string(file(id, "json"))
            .map_err(|e| e.to_string())
            .and_then(|t| decode(&t).map_err(|e| e.to_string()))
        {
            Ok((head, items)) => {
                self.leave_board();
                let [x, y, zoom] = head.view;
                self.show_board(id.to_owned(), &head.name, items, View { x, y, zoom });
            }
            // Left as it is on disk, so that nothing overwrites what can still be repaired.
            Err(e) => self.say(&format!("Could not open this board: {e}")),
        }
    }

    fn new_board(self: &Rc<Self>) {
        self.leave_board();
        if !self.items.borrow().is_empty() {
            self.show_board(fresh_id(), UNTITLED, Vec::new(), HOME);
            self.set_tool(Tool::Shape(Kind::Pen));
        }
        if self.side.reveals_child() {
            self.fill_list();
        }
    }

    fn delete_board(self: &Rc<Self>, id: &str) {
        let path = file(id, "json");
        if path.exists()
            && let Err(e) = gio::File::for_path(&path).trash(None::<&gio::Cancellable>)
        {
            return self.say(&format!("Could not move this board to Trash: {e}"));
        }
        let _ = fs::remove_file(file(id, "png"));
        if id == *self.board.borrow() {
            self.dirty.set(false);
            match boards().first() {
                Some(next) => {
                    // This board is gone: nothing of it may be written back.
                    let (next, read) =
                        (next.id.clone(), fs::read_to_string(file(&next.id, "json")));
                    match read.ok().and_then(|t| decode(&t).ok()) {
                        Some((head, items)) => {
                            let [x, y, zoom] = head.view;
                            self.show_board(next, &head.name, items, View { x, y, zoom });
                        }
                        None => self.show_board(fresh_id(), UNTITLED, Vec::new(), HOME),
                    }
                }
                None => self.show_board(fresh_id(), UNTITLED, Vec::new(), HOME),
            }
        }
        self.fill_list();
    }

    fn set_tool(self: &Rc<Self>, tool: Tool) {
        self.finish_text();
        self.tool.set(tool);
        if tool != Tool::Select {
            self.selected.borrow_mut().clear();
        }
        self.sync();
    }

    /// Brings every control in line with the state.
    fn sync(self: &Rc<Self>) {
        let tool = self.tool.get();
        for (t, button) in &*self.tool_buttons.borrow() {
            if *t == tool {
                button.add_css_class("on");
            } else {
                button.remove_css_class("on");
            }
        }
        self.area.set_cursor_from_name(Some(self.tool_cursor()));
        let items = self.items.borrow();
        let picked = self.selected.borrow();
        let kinds: Vec<Kind> = if picked.is_empty() {
            match tool {
                Tool::Shape(k) => vec![k],
                _ => Vec::new(),
            }
        } else {
            picked.iter().map(|&i| items[i].kind).collect()
        };
        let erasing = tool == Tool::Eraser;
        self.panel.set_visible(!kinds.is_empty() || erasing);
        self.stroke_row.set_visible(!erasing);
        self.eraser_row.set_visible(erasing);
        for (button, size) in &*self.eraser_buttons.borrow() {
            if *size == self.eraser.get() {
                button.add_css_class("on");
            } else {
                button.remove_css_class("on");
            }
        }
        self.fill_row.set_visible(kinds.iter().any(|k| k.closed()));
        self.line_row
            .set_visible(kinds.iter().any(|k| *k != Kind::Text));
        self.font_row
            .set_visible(kinds.iter().any(|k| *k == Kind::Text || k.closed()));
        self.order_row.set_visible(!picked.is_empty());
        let style = picked.first().map_or(self.style.get(), |&i| items[i].style);
        for (button, is) in &*self.options.borrow() {
            if is(&style) {
                button.add_css_class("on");
            } else {
                button.remove_css_class("on");
            }
        }
        self.undo_button
            .set_sensitive(!self.undo.borrow().is_empty());
        self.redo_button
            .set_sensitive(!self.redo.borrow().is_empty());
        self.area.queue_draw();
    }

    fn tool_cursor(&self) -> &'static str {
        if self.space.get() {
            return "grab";
        }
        match self.tool.get() {
            Tool::Hand => "grab",
            Tool::Select => "default",
            Tool::Shape(Kind::Text) => "text",
            // Their cursor is the disc that `draw_brush` paints.
            Tool::Shape(Kind::Pen) | Tool::Eraser => "none",
            _ => "crosshair",
        }
    }

    fn begin_change(&self) {
        *self.pending.borrow_mut() = Some(self.items.borrow().clone());
    }
    fn commit_change(self: &Rc<Self>) {
        if let Some(before) = self.pending.borrow_mut().take() {
            let mut undo = self.undo.borrow_mut();
            undo.push(before);
            if undo.len() > UNDO_STEPS {
                undo.remove(0);
            }
            self.redo.borrow_mut().clear();
        }
        self.scene_stale.set(true);
        self.mark_dirty();
    }

    fn history(self: &Rc<Self>, back: bool) {
        self.finish_text();
        let (from, to) = if back {
            (&self.undo, &self.redo)
        } else {
            (&self.redo, &self.undo)
        };
        let Some(state) = from.borrow_mut().pop() else {
            return;
        };
        to.borrow_mut().push(self.items.replace(state));
        self.selected.borrow_mut().clear();
        self.scene_stale.set(true);
        self.mark_dirty();
        self.sync();
    }

    fn reorder(self: &Rc<Self>, front: bool) {
        let picked = self.selected.borrow().clone();
        self.begin_change();
        let all = self.items.borrow().clone();
        let (chosen, rest): (Vec<_>, Vec<_>) = all
            .into_iter()
            .enumerate()
            .partition(|(i, _)| picked.contains(i));
        let count = chosen.len();
        let strip = |v: Vec<(usize, Rc<Item>)>| v.into_iter().map(|(_, r)| r).collect::<Vec<_>>();
        let (chosen, rest) = (strip(chosen), strip(rest));
        let total = chosen.len() + rest.len();
        *self.selected.borrow_mut() = if front {
            (total - count..total).collect()
        } else {
            (0..count).collect()
        };
        *self.items.borrow_mut() = if front {
            [rest, chosen].concat()
        } else {
            [chosen, rest].concat()
        };
        self.commit_change();
        self.sync();
    }

    fn remove_selected(self: &Rc<Self>) {
        let mut picked = self.selected.take();
        if picked.is_empty() {
            return;
        }
        self.begin_change();
        picked.sort_unstable();
        let mut items = self.items.borrow_mut();
        for &i in picked.iter().rev() {
            items.remove(i);
        }
        drop(items);
        self.commit_change();
        self.sync();
    }

    fn copy(&self) {
        let items = self.items.borrow();
        let mut picked = self.selected.borrow().clone();
        picked.sort_unstable();
        *self.clip.borrow_mut() = picked.iter().map(|&i| items[i].clone()).collect();
    }

    fn paste(self: &Rc<Self>) {
        let clip = self.clip.borrow().clone();
        if clip.is_empty() {
            return;
        }
        self.begin_change();
        let shift = 24.0 / self.view.get().zoom;
        // New identities: links stay within what was copied.
        let ids: HashMap<u64, u64> = clip.iter().map(|i| (i.id, self.issue())).collect();
        let mut items = self.items.borrow_mut();
        let first = items.len();
        items.extend(clip.iter().map(|i| {
            let mut copy = i.moved(shift, shift);
            copy.id = ids[&i.id];
            copy.from = i.from.and_then(|f| ids.get(&f).copied());
            copy.to = i.to.and_then(|t| ids.get(&t).copied());
            Rc::new(copy)
        }));
        *self.selected.borrow_mut() = (first..items.len()).collect();
        drop(items);
        self.tool.set(Tool::Select);
        self.commit_change();
        self.sync();
    }

    fn issue(&self) -> u64 {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        id
    }

    /// Puts the ends of every link on the bubbles they are tied to; only links
    /// touching `moved` if given.
    fn reroute(&self, moved: Option<&[u64]>) {
        let mut items = self.items.borrow_mut();
        let shapes: HashMap<u64, Rc<Item>> = items
            .iter()
            .filter(|i| i.kind.closed() && i.id != 0)
            .map(|i| (i.id, i.clone()))
            .collect();
        for slot in items.iter_mut() {
            if !is_link(slot.kind) || (slot.from.is_none() && slot.to.is_none()) {
                continue;
            }
            let touches =
                |end: Option<u64>| end.is_some_and(|e| moved.is_none_or(|m| m.contains(&e)));
            if !touches(slot.from) && !touches(slot.to) {
                continue;
            }
            let lookup = |end: Option<u64>| end.and_then(|e| shapes.get(&e)).map(|s| &**s);
            let gap = LINK_GAP + slot.style.width / 2.0;
            let pts = routed(
                slot.pts[0],
                slot.pts[1],
                lookup(slot.from),
                lookup(slot.to),
                gap,
            );
            if pts != slot.pts {
                *slot = Rc::new(slot.with(slot.style, pts, slot.text.clone()));
            }
        }
    }

    /// The topmost bubble at `p`, other than `except`.
    fn bubble_at(&self, p: [f64; 2], except: Option<u64>) -> Option<usize> {
        let tol = 4.0 / self.view.get().zoom;
        self.items
            .borrow()
            .iter()
            .rposition(|i| i.kind.closed() && Some(i.id) != except && i.encloses(p, tol))
    }

    /// What a double click at `p` would write in.
    fn writable_at(&self, p: [f64; 2]) -> Option<usize> {
        let tol = 4.0 / self.view.get().zoom;
        self.items.borrow().iter().rposition(|i| {
            (i.kind == Kind::Text && i.bbox().grow(tol).contains(p)) || i.encloses(p, tol)
        })
    }
    fn write_in(self: &Rc<Self>, i: usize) {
        let (kind, at) = {
            let items = self.items.borrow();
            (items[i].kind, items[i].pts[0])
        };
        if kind == Kind::Text {
            self.open_text(at, Some(i));
        } else {
            self.open_bubble(i);
        }
    }

    fn world(&self, x: f64, y: f64) -> [f64; 2] {
        let v = self.view.get();
        [v.x + x / v.zoom, v.y + y / v.zoom]
    }
    fn on_screen(&self, p: [f64; 2]) -> (f64, f64) {
        let v = self.view.get();
        ((p[0] - v.x) * v.zoom, (p[1] - v.y) * v.zoom)
    }

    fn sync_view(&self) {
        self.zoom_label
            .set_label(&format!("{:.0}%", self.view.get().zoom * 100.0));
        self.area.queue_draw();
    }
    fn set_view(self: &Rc<Self>, view: View) {
        self.finish_text();
        self.view.set(view);
        self.sync_view();
    }
    fn size(&self) -> (f64, f64) {
        (f64::from(self.area.width()), f64::from(self.area.height()))
    }
    fn zoom_center(self: &Rc<Self>, factor: f64) {
        let v = self.view.get();
        let (w, h) = self.size();
        self.set_view(v.zoomed(v.zoom * factor, w / 2.0, h / 2.0));
    }
    fn fit(self: &Rc<Self>) {
        let (w, h) = self.size();
        let view = View::showing(&self.items.borrow(), w, h, 80.0, 1.0);
        self.set_view(view);
    }

    fn top_at(&self, p: [f64; 2]) -> Option<usize> {
        let tol = 6.0 / self.view.get().zoom;
        self.items
            .borrow()
            .iter()
            .rposition(|i| i.hit(p, tol) || i.encloses(p, tol))
    }

    fn selection_area(&self) -> Option<Area> {
        let items = self.items.borrow();
        self.selected
            .borrow()
            .iter()
            .map(|&i| items[i].bbox())
            .reduce(Area::union)
    }

    /// The selection's grips, each with where it sits on screen.
    fn grips(&self) -> Vec<((f64, f64), Grab)> {
        let items = self.items.borrow();
        let picked = self.selected.borrow();
        if let [i] = picked[..]
            && matches!(items[i].kind, Kind::Line | Kind::Arrow)
        {
            return (0..2)
                .map(|which| {
                    (
                        self.on_screen(items[i].pts[which]),
                        Grab::End { index: i, which },
                    )
                })
                .collect();
        }
        drop((items, picked));
        let Some(b) = self.selection_area() else {
            return Vec::new();
        };
        const OUT: f64 = 5.0;
        [
            ([b.x0, b.y0], [b.x1, b.y1], (-OUT, -OUT)),
            ([b.x1, b.y0], [b.x0, b.y1], (OUT, -OUT)),
            ([b.x1, b.y1], [b.x0, b.y0], (OUT, OUT)),
            ([b.x0, b.y1], [b.x1, b.y0], (-OUT, OUT)),
        ]
        .into_iter()
        .map(|(grip, anchor, (ox, oy))| {
            let (x, y) = self.on_screen(grip);
            ((x + ox, y + oy), Grab::Corner { anchor, grip })
        })
        .collect()
    }
    fn grip_at(&self, x: f64, y: f64) -> Option<Grab> {
        self.grips()
            .into_iter()
            .find(|((gx, gy), _)| (gx - x).abs() <= GRIP && (gy - y).abs() <= GRIP)
            .map(|(_, grab)| grab)
    }

    fn editor_style(&self, style: Style) {
        self.editor_css.load_from_data(&format!(
            "textview {{ font-family: Sans; font-size: {:.2}px; color: #{:06x}; caret-color: #{:06x}; }}",
            style.size * self.view.get().zoom,
            shown(style.stroke, self.dark.get()),
            shown(style.stroke, self.dark.get())
        ));
    }

    fn open_text(self: &Rc<Self>, at: [f64; 2], index: Option<usize>) {
        let (style, text) = match index {
            Some(i) => {
                let items = self.items.borrow();
                (items[i].style, items[i].text.clone())
            }
            None => (self.style.get(), String::new()),
        };
        let (x, y) = self.on_screen(at);
        self.editor.set_size_request(-1, -1);
        self.editor.set_wrap_mode(gtk::WrapMode::None);
        self.editor.set_justification(gtk::Justification::Left);
        self.editor.set_margin_start(x.max(0.0) as i32);
        self.editor.set_margin_top(y.max(0.0) as i32);
        self.editor_style(style);
        *self.editing.borrow_mut() = Some(Editing {
            at,
            index,
            style,
            bubble: None,
        });
        self.editor.buffer().set_text(&text);
        self.show_editor();
    }

    /// Writing inside bubble `i`: its text is the editor's until the end, and
    /// the bubble follows what gets typed.
    fn open_bubble(self: &Rc<Self>, i: usize) {
        let original = self.items.borrow()[i].clone();
        self.begin_change();
        self.items.borrow_mut()[i] =
            Rc::new(original.with(original.style, original.pts.clone(), String::new()));
        self.editor.set_wrap_mode(gtk::WrapMode::WordChar);
        self.editor.set_justification(gtk::Justification::Center);
        self.editor_style(original.style);
        *self.editing.borrow_mut() = Some(Editing {
            at: original.pts[0],
            index: Some(i),
            style: original.style,
            bubble: Some(original.clone()),
        });
        self.editor.buffer().set_text(&original.text);
        self.bubble_typed();
        self.show_editor();
    }

    fn show_editor(self: &Rc<Self>) {
        self.selected.borrow_mut().clear();
        self.editor.set_visible(true);
        self.editor.grab_focus();
        self.scene_stale.set(true);
        self.sync();
    }

    fn bubble_typed(self: &Rc<Self>) {
        let editing = self.editing.borrow();
        let Some(Editing {
            index: Some(i),
            bubble: Some(original),
            style,
            ..
        }) = &*editing
        else {
            return;
        };
        let buffer = self.editor.buffer();
        let (start, end) = buffer.bounds();
        let text = buffer.text(&start, &end, false);
        let pts = fit_label(&self.pc, original.kind, style.size, &original.pts, &text);
        let area = Area::of(&pts);
        let room = inner(original.kind, area);
        let sample = if text.is_empty() { " " } else { text.as_str() };
        let height = f64::from(
            label_layout(&self.pc, sample, style.size, room.width())
                .size()
                .1,
        ) / f64::from(pango::SCALE);
        let zoom = self.view.get().zoom;
        let (x, y) = self.on_screen([room.x0, (area.y0 + area.y1 - height) / 2.0]);
        // GTK measures a wrapping text view from its previous layout, and a box too low
        // scrolls and cuts the text. A spare line costs nothing: it is transparent, and
        // covers the view wrapping a little earlier than the bubble's own text.
        self.editor.set_size_request(
            (room.width() * zoom).ceil() as i32,
            ((height + style.size * 1.5) * zoom).ceil() as i32,
        );
        self.editor.set_margin_start(x.max(0.0) as i32);
        self.editor.set_margin_top(y.max(0.0) as i32);
        self.items.borrow_mut()[*i] = Rc::new(original.with(*style, pts, String::new()));
        self.reroute(Some(&[original.id]));
        self.scene_stale.set(true);
        self.area.queue_draw();
    }

    fn finish_text(self: &Rc<Self>) {
        let Some(edit) = self.editing.borrow_mut().take() else {
            return;
        };
        let buffer = self.editor.buffer();
        let (start, end) = buffer.bounds();
        let text = buffer.text(&start, &end, false).trim_end().to_owned();
        self.editor.set_visible(false);
        self.area.grab_focus();
        self.scene_stale.set(true);
        if let (Some(i), Some(original)) = (edit.index, &edit.bubble) {
            let same = text == original.text;
            let pts = fit_label(
                &self.pc,
                original.kind,
                edit.style.size,
                &original.pts,
                &text,
            );
            self.items.borrow_mut()[i] = if same {
                original.clone()
            } else {
                Rc::new(original.with(edit.style, pts, text))
            };
            self.reroute(Some(&[original.id]));
            if same {
                self.pending.borrow_mut().take();
            } else {
                self.commit_change();
            }
            self.selected.borrow_mut().clear();
            self.selected.borrow_mut().push(i);
        } else {
            self.finish_plain_text(&edit, text);
        }
        if self.tool.get() == Tool::Shape(Kind::Text) {
            self.tool.set(Tool::Select);
        }
        self.sync();
    }

    fn finish_plain_text(self: &Rc<Self>, edit: &Editing, text: String) {
        let unchanged = edit
            .index
            .is_some_and(|i| self.items.borrow()[i].text == text);
        if unchanged || (text.is_empty() && edit.index.is_none()) {
            return;
        }
        self.begin_change();
        let mut items = self.items.borrow_mut();
        if text.is_empty() {
            if let Some(i) = edit.index {
                items.remove(i);
            }
        } else {
            let (w, h) = text_size(&self.pc, &text, edit.style.size);
            let pts = vec![edit.at, [edit.at[0] + w, edit.at[1] + h]];
            let mut item = Item::new(Kind::Text, edit.style, pts, text);
            item.id = self.issue();
            match edit.index {
                Some(i) => items[i] = Rc::new(item),
                None => items.push(Rc::new(item)),
            }
        }
        drop(items);
        self.selected.borrow_mut().clear();
        self.commit_change();
    }

    fn press(self: &Rc<Self>, x: f64, y: f64, button: u32, shift: bool, time: u32) {
        self.finish_text();
        let (last, lx, ly) = self.last_press.get();
        let double = time.wrapping_sub(last) < 400 && (lx - x).abs() < 6.0 && (ly - y).abs() < 6.0;
        self.last_press.set((if double { 0 } else { time }, x, y));
        let p = self.world(x, y);
        self.start.set((x, y));
        self.changed.set(false);
        let tool = self.tool.get();
        if button == 2 || (button == 1 && (self.space.get() || tool == Tool::Hand)) {
            *self.drag.borrow_mut() = Drag::Pan(self.view.get());
            self.area.set_cursor_from_name(Some("grabbing"));
            return;
        }
        if button != 1 {
            return;
        }
        let drag = match tool {
            Tool::Hand => return,
            Tool::Select => {
                if double {
                    match self.writable_at(p) {
                        Some(i) => self.write_in(i),
                        None if self.top_at(p).is_none() => self.open_text(p, None),
                        None => {}
                    }
                    return;
                }
                self.select_press(x, y, p, shift)
            }
            Tool::Eraser => {
                self.selected.borrow_mut().clear();
                self.begin_change();
                self.erase_along(p, p);
                Drag::Erase(p)
            }
            Tool::Shape(Kind::Text) => {
                match self.writable_at(p) {
                    Some(i) => self.write_in(i),
                    None => self.open_text(p, None),
                }
                return;
            }
            Tool::Shape(kind) => {
                let pts = if kind == Kind::Pen {
                    vec![p]
                } else {
                    vec![p, p]
                };
                let mut item = Item::new(kind, self.style.get(), pts, String::new());
                if is_link(kind)
                    && let Some(i) = self.bubble_at(p, None)
                {
                    item.from = Some(self.items.borrow()[i].id);
                }
                *self.live.borrow_mut() = Some(item);
                Drag::Create
            }
        };
        *self.drag.borrow_mut() = drag;
        self.area.queue_draw();
        self.sync();
    }

    fn select_press(self: &Rc<Self>, x: f64, y: f64, p: [f64; 2], shift: bool) -> Drag {
        // A link moved on its own lets go of what does not move with it.
        let originals = |this: &Self| -> Vec<(usize, Rc<Item>)> {
            let items = this.items.borrow();
            let picked = this.selected.borrow();
            let ids: Vec<u64> = picked.iter().map(|&i| items[i].id).collect();
            this.linked.replace(
                (0..items.len())
                    .filter(|i| {
                        !picked.contains(i)
                            && is_link(items[*i].kind)
                            && [items[*i].from, items[*i].to]
                                .iter()
                                .flatten()
                                .any(|e| ids.contains(e))
                    })
                    .collect(),
            );
            picked
                .iter()
                .map(|&i| {
                    let it = &items[i];
                    let kept = |end: Option<u64>| end.filter(|e| ids.contains(e));
                    if !is_link(it.kind) || (kept(it.from) == it.from && kept(it.to) == it.to) {
                        return (i, it.clone());
                    }
                    let mut free = it.with(it.style, it.pts.clone(), String::new());
                    free.from = kept(it.from);
                    free.to = kept(it.to);
                    (i, Rc::new(free))
                })
                .collect()
        };
        if let Some(grab) = self.grip_at(x, y) {
            self.begin_change();
            self.scene_stale.set(true);
            return match grab {
                Grab::End { index, which } => Drag::End { index, which },
                Grab::Corner { anchor, grip } => Drag::Resize {
                    press: p,
                    anchor,
                    grip,
                    was: originals(self),
                },
            };
        }
        let in_group = self.selected.borrow().len() > 1
            && self.selection_area().is_some_and(|b| b.contains(p));
        if in_group && !shift {
            self.begin_change();
            self.scene_stale.set(true);
            return Drag::Move(p, originals(self));
        }
        match self.top_at(p) {
            Some(i) => {
                {
                    let mut picked = self.selected.borrow_mut();
                    if shift {
                        match picked.iter().position(|&s| s == i) {
                            Some(at) => {
                                picked.remove(at);
                            }
                            None => picked.push(i),
                        }
                    } else if !picked.contains(&i) {
                        *picked = vec![i];
                    }
                }
                self.begin_change();
                self.scene_stale.set(true);
                Drag::Move(p, originals(self))
            }
            None => {
                if !shift {
                    self.selected.borrow_mut().clear();
                }
                Drag::Band(p, p)
            }
        }
    }

    fn touched_at(&self, p: [f64; 2], tol: f64) -> Option<usize> {
        self.items.borrow().iter().rposition(|i| i.hit(p, tol))
    }

    fn erase_along(self: &Rc<Self>, from: [f64; 2], to: [f64; 2]) {
        let zoom = self.view.get().zoom;
        let steps = (((to[0] - from[0]).hypot(to[1] - from[1]) * zoom) / 4.0)
            .ceil()
            .max(1.0);
        for n in 0..=steps as usize {
            let t = n as f64 / steps;
            let p = [
                from[0] + (to[0] - from[0]) * t,
                from[1] + (to[1] - from[1]) * t,
            ];
            // The eraser takes what it touches: the inside of an empty bubble is not it.
            let tol = self.eraser.get() / 2.0 / zoom;
            while let Some(i) = self.touched_at(p, tol) {
                self.items.borrow_mut().remove(i);
                self.changed.set(true);
                self.scene_stale.set(true);
            }
        }
        self.area.queue_draw();
    }

    fn update(self: &Rc<Self>, ox: f64, oy: f64, shift: bool) {
        let (sx, sy) = self.start.get();
        let (x, y) = (sx + ox, sy + oy);
        self.pointer.set((x, y));
        let p = self.world(x, y);
        let zoom = self.view.get().zoom;
        let mut drag = self.drag.borrow_mut();
        match &mut *drag {
            Drag::Idle => return,
            Drag::Pan(from) => {
                let view = View {
                    x: from.x - ox / from.zoom,
                    y: from.y - oy / from.zoom,
                    zoom: from.zoom,
                };
                drop(drag);
                self.view.set(view);
                self.sync_view();
                return;
            }
            Drag::Create => {
                if ox.abs() + oy.abs() >= 4.0 {
                    self.changed.set(true);
                }
                let mut live = self.live.borrow_mut();
                if let Some(item) = live.as_mut() {
                    if item.kind == Kind::Pen {
                        let last = item.pts[item.pts.len() - 1];
                        if (last[0] - p[0]).hypot(last[1] - p[1]) * zoom >= 1.5 {
                            item.pts.push(p);
                        }
                    } else if is_link(item.kind) {
                        let aim = if shift {
                            snapped(item.kind, item.pts[0], p)
                        } else {
                            p
                        };
                        let target = self.bubble_at(p, item.from);
                        self.hint.set(target);
                        let items = self.items.borrow();
                        let from = item
                            .from
                            .and_then(|id| items.iter().find(|i| i.id == id))
                            .cloned();
                        let to = target.map(|i| items[i].clone());
                        item.to = to.as_ref().map(|t| t.id);
                        let gap = LINK_GAP + item.style.width / 2.0;
                        item.pts = routed(item.pts[0], aim, from.as_deref(), to.as_deref(), gap);
                    } else {
                        item.pts[1] = if shift {
                            snapped(item.kind, item.pts[0], p)
                        } else {
                            p
                        };
                    }
                }
            }
            Drag::Band(_, now) => *now = p,
            Drag::Move(press, was) => {
                let (dx, dy) = (p[0] - press[0], p[1] - press[1]);
                if (ox.abs() + oy.abs()) >= 2.0 || self.changed.get() {
                    self.changed.set(true);
                    let mut items = self.items.borrow_mut();
                    for (i, item) in was.iter() {
                        items[*i] = Rc::new(item.moved(dx, dy));
                    }
                    drop(items);
                    let ids: Vec<u64> = was.iter().map(|(_, it)| it.id).collect();
                    self.reroute(Some(&ids));
                }
            }
            Drag::Resize {
                press,
                anchor,
                grip,
                was,
            } => {
                let target = [grip[0] + p[0] - press[0], grip[1] + p[1] - press[1]];
                let ratio = |i: usize| {
                    if (grip[i] - anchor[i]).abs() < 1e-6 {
                        1.0
                    } else {
                        (target[i] - anchor[i]) / (grip[i] - anchor[i])
                    }
                };
                let (mut rx, mut ry) = (ratio(0), ratio(1));
                if shift || was.iter().any(|(_, it)| it.kind == Kind::Text) {
                    let s = rx.abs().max(ry.abs());
                    rx = s.copysign(rx);
                    ry = s.copysign(ry);
                }
                self.changed.set(true);
                let mut items = self.items.borrow_mut();
                for (i, item) in was.iter() {
                    items[*i] = Rc::new(item.scaled(*anchor, rx, ry).fitted(&self.pc));
                }
                drop(items);
                let ids: Vec<u64> = was.iter().map(|(_, it)| it.id).collect();
                self.reroute(Some(&ids));
            }
            Drag::End { index, which } => {
                let (index, which) = (*index, *which);
                let current = self.items.borrow()[index].clone();
                let other_end = if which == 0 { current.to } else { current.from };
                let aim = if shift {
                    snapped(Kind::Line, current.pts[1 - which], p)
                } else {
                    p
                };
                let target = self.bubble_at(p, other_end);
                self.hint.set(target);
                let mut pts = current.pts.clone();
                pts[which] = aim;
                let items = self.items.borrow();
                let find =
                    |end: Option<u64>| end.and_then(|e| items.iter().find(|i| i.id == e)).cloned();
                let tied = target.map(|i| items[i].clone());
                let (from, to) = if which == 0 {
                    (tied.clone(), find(current.to))
                } else {
                    (find(current.from), tied.clone())
                };
                drop(items);
                let gap = LINK_GAP + current.style.width / 2.0;
                let mut item = current.with(
                    current.style,
                    routed(pts[0], pts[1], from.as_deref(), to.as_deref(), gap),
                    current.text.clone(),
                );
                item.from = from.map(|s| s.id);
                item.to = to.map(|s| s.id);
                self.items.borrow_mut()[index] = Rc::new(item);
                self.changed.set(true);
            }
            Drag::Erase(last) => {
                let from = *last;
                *last = p;
                drop(drag);
                self.erase_along(from, p);
                return;
            }
        }
        drop(drag);
        self.area.queue_draw();
    }

    fn release(self: &Rc<Self>) {
        let drag = self.drag.replace(Drag::Idle);
        self.hint.set(None);
        self.linked.borrow_mut().clear();
        match drag {
            Drag::Idle => return,
            Drag::Pan(_) => {}
            Drag::Create => {
                let live = self.live.borrow_mut().take();
                if let Some(mut item) = live {
                    if item.kind != Kind::Pen && !self.changed.get() {
                        return self.sync();
                    }
                    item.id = self.issue();
                    let pen = item.kind == Kind::Pen;
                    self.begin_change();
                    let mut items = self.items.borrow_mut();
                    items.push(Rc::new(item));
                    let last = items.len() - 1;
                    drop(items);
                    if !pen {
                        self.tool.set(Tool::Select);
                        *self.selected.borrow_mut() = vec![last];
                    }
                    self.commit_change();
                }
            }
            Drag::Band(a, b) => {
                let band = Area::of(&[a, b]);
                let hits: Vec<usize> = self
                    .items
                    .borrow()
                    .iter()
                    .enumerate()
                    .filter(|(_, it)| it.bbox().inside(band))
                    .map(|(i, _)| i)
                    .collect();
                let mut picked = self.selected.borrow_mut();
                for i in hits {
                    if !picked.contains(&i) {
                        picked.push(i);
                    }
                }
            }
            Drag::Move(..) | Drag::Resize { .. } | Drag::End { .. } | Drag::Erase(_) => {
                self.scene_stale.set(true);
                if self.changed.get() {
                    self.commit_change();
                } else {
                    self.pending.borrow_mut().take();
                }
            }
        }
        self.sync();
    }

    fn hover(&self, x: f64, y: f64) {
        self.pointer.set((x, y));
        self.inside.set(true);
        if matches!(self.tool.get(), Tool::Shape(Kind::Pen) | Tool::Eraser) {
            self.area.queue_draw();
        }
        if self.tool.get() != Tool::Select
            || self.space.get()
            || !matches!(*self.drag.borrow(), Drag::Idle)
        {
            return;
        }
        let name = match self.grip_at(x, y) {
            Some(Grab::Corner { anchor, grip }) => {
                if (grip[0] - anchor[0]) * (grip[1] - anchor[1]) > 0.0 {
                    "nwse-resize"
                } else {
                    "nesw-resize"
                }
            }
            Some(Grab::End { .. }) => "move",
            None if self.top_at(self.world(x, y)).is_some() => "move",
            None => "default",
        };
        self.area.set_cursor_from_name(Some(name));
    }

    fn hidden(&self) -> Vec<usize> {
        let mut out = match *self.drag.borrow() {
            Drag::Move(..) | Drag::Resize { .. } => {
                [&self.selected.borrow()[..], &self.linked.borrow()[..]].concat()
            }
            Drag::End { index, .. } => vec![index],
            _ => Vec::new(),
        };
        if let Some(Editing {
            index: Some(i),
            bubble: None,
            ..
        }) = *self.editing.borrow()
        {
            out.push(i);
        }
        out.sort_unstable();
        out
    }

    fn draw(self: &Rc<Self>, cr: &cairo::Context, width: i32, height: i32) {
        let scale = self
            .area
            .native()
            .and_then(|n| n.surface())
            .map_or(1.0, |s| s.property::<f64>("scale"));
        let size = (width, height);
        let hidden = self.hidden();
        let (wide, tall) = (
            (f64::from(width) * scale).ceil(),
            (f64::from(height) * scale).ceil(),
        );
        let mut scene = self.scene.borrow_mut();
        if !scene
            .as_ref()
            .is_some_and(|s| s.size == size && s.scale == scale)
        {
            *scene = cairo::ImageSurface::create(cairo::Format::Rgb24, wide as i32, tall as i32)
                .ok()
                .map(|surface| {
                    surface.set_device_scale(scale, scale);
                    Scene {
                        surface,
                        size,
                        scale,
                        view: HOME,
                    }
                });
            self.scene_stale.set(true);
        }
        let Some(picture) = scene.as_mut() else {
            return;
        };
        let mut view = self.view.get();
        let unit = view.zoom * scale;
        let (dx, dy) = (
            ((picture.view.x - view.x) * unit).round(),
            ((picture.view.y - view.y) * unit).round(),
        );
        let stale = self.scene_stale.replace(false);
        if stale || picture.view.zoom != view.zoom || dx.abs() >= wide || dy.abs() >= tall {
            self.repaint(
                &picture.surface,
                view,
                scale,
                (0.0, 0.0, wide, tall),
                &hidden,
            );
        } else {
            // Only panned: the pixels slide, and only the strips that came into view
            // get painted, with the view on a whole pixel so that they join.
            view.x = picture.view.x - dx / unit;
            view.y = picture.view.y - dy / unit;
            if dx != 0.0 || dy != 0.0 {
                self.slide(&picture.surface, scale, dx, dy);
                if dx != 0.0 {
                    let strip = if dx > 0.0 {
                        (0.0, 0.0, dx, tall)
                    } else {
                        (wide + dx, 0.0, wide, tall)
                    };
                    self.repaint(&picture.surface, view, scale, strip, &hidden);
                }
                if dy != 0.0 {
                    let strip = if dy > 0.0 {
                        (0.0, 0.0, wide, dy)
                    } else {
                        (0.0, tall + dy, wide, tall)
                    };
                    self.repaint(&picture.surface, view, scale, strip, &hidden);
                }
            }
        }
        picture.view = view;
        picture.surface.flush();
        let _ = cr.set_source_surface(&picture.surface, 0.0, 0.0);
        cr.source().set_filter(cairo::Filter::Nearest);
        let _ = cr.paint();
        drop(scene);

        let _ = cr.save();
        cr.scale(view.zoom, view.zoom);
        cr.translate(-view.x, -view.y);
        let items = self.items.borrow();
        for &i in &hidden {
            if self.editing.borrow().as_ref().and_then(|e| e.index) != Some(i) {
                items[i].paint(cr, view.zoom, &self.pc, self.dark.get());
            }
        }
        drop(items);
        if let Some(item) = &*self.live.borrow() {
            item.paint(cr, view.zoom, &self.pc, self.dark.get());
        }
        let _ = cr.restore();
        self.draw_selection(cr);
        self.draw_brush(cr);
    }

    /// The pen's or the eraser's footprint, under the pointer.
    fn draw_brush(&self, cr: &cairo::Context) {
        let tool = self.tool.get();
        let panning = matches!(*self.drag.borrow(), Drag::Pan(_));
        if !self.inside.get() || self.space.get() || panning {
            return;
        }
        let (x, y) = self.pointer.get();
        let dark = self.dark.get();
        let ring = if dark { 1.0 } else { 0.0 };
        cr.set_dash(&[], 0.0);
        cr.set_line_width(1.0);
        let tau = std::f64::consts::TAU;
        match tool {
            Tool::Shape(Kind::Pen) => {
                let style = self.style.get();
                let radius = (style.width * self.view.get().zoom / 2.0).max(1.5);
                cr.arc(x, y, radius, 0.0, tau);
                ink(cr, style.stroke, dark);
                let _ = cr.fill();
                // Just outside the ink, so that the pointer shows on any colour.
                cr.arc(x, y, radius + 1.5, 0.0, tau);
                cr.set_source_rgba(ring, ring, ring, 0.8);
                let _ = cr.stroke();
            }
            Tool::Eraser => {
                cr.arc(x, y, self.eraser.get() / 2.0, 0.0, tau);
                cr.set_source_rgba(ring, ring, ring, 0.08);
                let _ = cr.fill_preserve();
                cr.set_source_rgba(ring, ring, ring, 0.8);
                let _ = cr.stroke();
            }
            _ => {}
        }
    }

    /// Paints the device-pixel rectangle `(x0, y0, x1, y1)` of the scene.
    fn repaint(
        &self,
        surface: &cairo::ImageSurface,
        view: View,
        scale: f64,
        (x0, y0, x1, y1): (f64, f64, f64, f64),
        hidden: &[usize],
    ) {
        let Ok(cr) = cairo::Context::new(surface) else {
            return;
        };
        let region = Area {
            x0: x0 / scale,
            y0: y0 / scale,
            x1: x1 / scale,
            y1: y1 / scale,
        };
        // A soft edge would show where two strips meet.
        cr.set_antialias(cairo::Antialias::None);
        cr.rectangle(region.x0, region.y0, region.width(), region.height());
        cr.clip();
        cr.set_antialias(cairo::Antialias::Default);
        let (r, g, b) = self.canvas.get();
        cr.set_source_rgb(r, g, b);
        let _ = cr.paint();
        render(
            &cr,
            &self.items.borrow(),
            view,
            region,
            hidden,
            &self.pc,
            self.dark.get(),
        );
    }

    /// Moves the scene's pixels by whole device pixels.
    fn slide(&self, surface: &cairo::ImageSurface, scale: f64, dx: f64, dy: f64) {
        let Ok(cr) = cairo::Context::new(surface) else {
            return;
        };
        // Through a group, as source and destination overlap.
        cr.set_operator(cairo::Operator::Source);
        cr.push_group();
        cr.set_operator(cairo::Operator::Source);
        let _ = cr.set_source_surface(surface, dx / scale, dy / scale);
        cr.source().set_filter(cairo::Filter::Nearest);
        let _ = cr.paint();
        let _ = cr.pop_group_to_source();
        let _ = cr.paint();
    }

    fn draw_selection(&self, cr: &cairo::Context) {
        let accent = self.area.color();
        let line = |cr: &cairo::Context, alpha: f32| {
            cr.set_source_rgba(
                accent.red().into(),
                accent.green().into(),
                accent.blue().into(),
                alpha.into(),
            );
        };
        cr.set_dash(&[], 0.0);
        cr.set_line_width(1.0);
        if let Drag::Band(a, b) = *self.drag.borrow() {
            let (ax, ay) = self.on_screen(a);
            let (bx, by) = self.on_screen(b);
            cr.rectangle(ax.min(bx), ay.min(by), (ax - bx).abs(), (ay - by).abs());
            line(cr, 0.12);
            let _ = cr.fill_preserve();
            line(cr, 1.0);
            let _ = cr.stroke();
        }
        if let Some(i) = self.hint.get()
            && let Some(item) = self.items.borrow().get(i)
        {
            let b = item.bbox();
            let (x0, y0) = self.on_screen([b.x0, b.y0]);
            let (x1, y1) = self.on_screen([b.x1, b.y1]);
            cr.rectangle(x0 - 7.0, y0 - 7.0, x1 - x0 + 14.0, y1 - y0 + 14.0);
            line(cr, 0.16);
            let _ = cr.fill_preserve();
            line(cr, 0.8);
            let _ = cr.stroke();
        }
        if self.live.borrow().is_some() {
            return;
        }
        let grips = self.grips();
        if grips.len() == 4
            && let Some(b) = self.selection_area()
        {
            let (x0, y0) = self.on_screen([b.x0, b.y0]);
            let (x1, y1) = self.on_screen([b.x1, b.y1]);
            cr.rectangle(x0 - 5.0, y0 - 5.0, x1 - x0 + 10.0, y1 - y0 + 10.0);
            line(cr, 1.0);
            let _ = cr.stroke();
        }
        for ((x, y), _) in grips {
            cr.rectangle(x - 4.0, y - 4.0, 8.0, 8.0);
            let (r, g, b) = self.canvas.get();
            cr.set_source_rgb(r, g, b);
            let _ = cr.fill_preserve();
            line(cr, 1.0);
            let _ = cr.stroke();
        }
    }

    fn wire_canvas(self: &Rc<Self>) {
        let this = self.clone();
        self.area
            .set_draw_func(move |_, cr, w, h| this.draw(cr, w, h));

        let drag = gtk::GestureDrag::new();
        drag.set_button(0);
        let this = self.clone();
        drag.connect_drag_begin(move |g, x, y| {
            this.area.grab_focus();
            let shift = g
                .current_event_state()
                .contains(gdk::ModifierType::SHIFT_MASK);
            this.press(x, y, g.current_button(), shift, g.current_event_time());
        });
        let this = self.clone();
        drag.connect_drag_update(move |g, x, y| {
            let shift = g
                .current_event_state()
                .contains(gdk::ModifierType::SHIFT_MASK);
            this.update(x, y, shift);
        });
        let this = self.clone();
        drag.connect_drag_end(move |_, _, _| this.release());
        self.area.add_controller(drag);

        let motion = gtk::EventControllerMotion::new();
        let this = self.clone();
        motion.connect_motion(move |_, x, y| this.hover(x, y));
        let this = self.clone();
        motion.connect_leave(move |_| {
            this.inside.set(false);
            this.area.queue_draw();
        });
        self.area.add_controller(motion);

        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        let this = self.clone();
        scroll.connect_scroll(move |c, dx, dy| {
            let v = this.view.get();
            let wheel = c.unit() == gdk::ScrollUnit::Wheel;
            if c.current_event_state()
                .contains(gdk::ModifierType::CONTROL_MASK)
            {
                let factor = if wheel {
                    1.15_f64.powf(-dy)
                } else {
                    (-dy * 0.01).exp()
                };
                let (x, y) = this.pointer.get();
                this.set_view(v.zoomed(v.zoom * factor, x, y));
            } else {
                let step = if wheel { 60.0 } else { 1.0 };
                this.set_view(View {
                    x: v.x + dx * step / v.zoom,
                    y: v.y + dy * step / v.zoom,
                    zoom: v.zoom,
                });
            }
            glib::Propagation::Stop
        });
        self.area.add_controller(scroll);

        let pinch = gtk::GestureZoom::new();
        let this = self.clone();
        pinch.connect_begin(move |_, _| this.pinch_from.set(this.view.get().zoom));
        let this = self.clone();
        pinch.connect_scale_changed(move |g, scale| {
            let (x, y) = g.bounding_box_center().unwrap_or(this.pointer.get());
            let v = this.view.get();
            this.set_view(v.zoomed(this.pinch_from.get() * scale, x, y));
        });
        self.area.add_controller(pinch);
    }

    fn wire_keys(self: &Rc<Self>) {
        let this = self.clone();
        self.editor
            .buffer()
            .connect_changed(move |_| this.bubble_typed());
        // Bubbling: what the text boxes type never gets here.
        let keys = gtk::EventControllerKey::new();
        let this = self.clone();
        keys.connect_key_pressed(move |_, key, _, state| {
            if this.key(key, state) {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        let this = self.clone();
        keys.connect_key_released(move |_, key, _, _| {
            if key == gdk::Key::space {
                this.space.set(false);
                this.area.set_cursor_from_name(Some(this.tool_cursor()));
            }
        });
        self.window.add_controller(keys);
    }

    fn key(self: &Rc<Self>, key: gdk::Key, state: gdk::ModifierType) -> bool {
        use gdk::Key as K;
        let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
        let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
        if key == K::Escape {
            if self.editing.borrow().is_some() {
                self.finish_text();
            } else if !self.selected.borrow().is_empty() {
                self.selected.borrow_mut().clear();
                self.sync();
            } else {
                self.set_tool(Tool::Select);
            }
            return true;
        }
        let letter = key.to_lower().to_unicode().unwrap_or('\0');
        if ctrl {
            match letter {
                'z' => self.history(!shift),
                'y' => self.history(false),
                's' => {
                    self.finish_text();
                    self.save(true);
                }
                'a' => {
                    self.tool.set(Tool::Select);
                    *self.selected.borrow_mut() = (0..self.items.borrow().len()).collect();
                    self.sync();
                }
                'c' => self.copy(),
                'v' => self.paste(),
                'd' => {
                    self.copy();
                    self.paste();
                }
                '=' | '+' => self.zoom_center(1.25),
                '-' => self.zoom_center(0.8),
                // 0 is à on an AZERTY keyboard.
                '0' | 'à' => {
                    let v = self.view.get();
                    let (w, h) = self.size();
                    self.set_view(v.zoomed(1.0, w / 2.0, h / 2.0));
                }
                _ => return false,
            }
            return true;
        }
        if self.editing.borrow().is_some() {
            return false;
        }
        if key == K::space {
            self.space.set(true);
            self.area.set_cursor_from_name(Some("grab"));
            return true;
        }
        if key == K::Return || key == K::KP_Enter {
            if let [i] = self.selected.borrow()[..]
                && {
                    let kind = self.items.borrow()[i].kind;
                    kind.closed() || kind == Kind::Text
                }
            {
                self.write_in(i);
                return true;
            }
            return false;
        }
        if key == K::Delete || key == K::BackSpace {
            self.remove_selected();
            return true;
        }
        if shift && (letter == '1' || letter == '&') {
            self.fit();
            return true;
        }
        match TOOLS.iter().find(|(_, _, k)| *k == letter) {
            Some((tool, ..)) => {
                self.set_tool(*tool);
                true
            }
            None => false,
        }
    }
}

/// `to`, squared up for shapes, or on the nearest 45° for lines.
fn snapped(kind: Kind, from: [f64; 2], to: [f64; 2]) -> [f64; 2] {
    let (dx, dy) = (to[0] - from[0], to[1] - from[1]);
    if kind.closed() {
        let side = dx.abs().max(dy.abs());
        return [from[0] + side.copysign(dx), from[1] + side.copysign(dy)];
    }
    let step = std::f64::consts::FRAC_PI_4;
    let angle = (dy.atan2(dx) / step).round() * step;
    let len = dx.hypot(dy);
    [from[0] + len * angle.cos(), from[1] + len * angle.sin()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(kind: Kind, pts: Vec<[f64; 2]>, fill: Option<u32>) -> Rc<Item> {
        let style = Style {
            fill,
            ..DEFAULT_STYLE
        };
        Rc::new(Item::new(kind, style, pts, String::new()))
    }

    #[test]
    fn a_board_survives_the_file() {
        let items = vec![
            item(Kind::Pen, vec![[1.234567, 2.0], [3.0, 4.0]], None),
            Rc::new(Item::new(
                Kind::Text,
                DEFAULT_STYLE,
                vec![[0.0, 0.0], [9.0, 9.0]],
                "line one\nline \"two\"".into(),
            )),
        ];
        let head = Head {
            name: "Plan".into(),
            view: [1.0, 2.0, 0.5],
        };
        let (back, read) = decode(&encode(&head, &items).unwrap()).unwrap();
        assert_eq!(back.name, "Plan");
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].pts[0], [1.23, 2.0]);
        assert_eq!(read[1].text, "line one\nline \"two\"");
        assert!(decode("{\"name\":\"x\",\"view\":[0,0,1]}\nnot json\n").is_err());
    }

    #[test]
    fn zooming_keeps_the_point_under_the_pointer() {
        let v = View {
            x: 40.0,
            y: -10.0,
            zoom: 2.0,
        };
        let z = v.zoomed(0.5, 300.0, 120.0);
        let world = |v: View| (v.x + 300.0 / v.zoom, v.y + 120.0 / v.zoom);
        assert!((world(v).0 - world(z).0).abs() < 1e-9);
        assert!((world(v).1 - world(z).1).abs() < 1e-9);
        assert_eq!(v.zoomed(1e9, 0.0, 0.0).zoom, MAX_ZOOM);
    }

    #[test]
    fn hits_follow_what_is_painted() {
        let hollow = item(Kind::Rect, vec![[0.0, 0.0], [100.0, 100.0]], None);
        assert!(hollow.hit([0.0, 50.0], 2.0));
        assert!(!hollow.hit([50.0, 50.0], 2.0));
        let solid = item(Kind::Rect, vec![[0.0, 0.0], [100.0, 100.0]], Some(1));
        assert!(solid.hit([50.0, 50.0], 2.0));
        let round = item(Kind::Ellipse, vec![[0.0, 0.0], [100.0, 50.0]], None);
        assert!(round.hit([100.0, 25.0], 2.0));
        assert!(!round.hit([2.0, 2.0], 2.0));
        let gem = item(Kind::Diamond, vec![[0.0, 0.0], [100.0, 100.0]], None);
        assert!(gem.hit([25.0, 25.0], 2.0));
        assert!(!gem.hit([2.0, 2.0], 2.0));
        let stroke = item(
            Kind::Pen,
            vec![[0.0, 0.0], [100.0, 0.0], [100.0, 100.0]],
            None,
        );
        assert!(stroke.hit([50.0, 1.0], 2.0));
        assert!(!stroke.hit([50.0, 50.0], 2.0));
    }

    #[test]
    fn scaling_flips_around_the_anchor() {
        let box_ = item(Kind::Rect, vec![[10.0, 10.0], [20.0, 30.0]], None);
        let s = box_.scaled([10.0, 10.0], -2.0, 1.0);
        assert_eq!(
            s.bbox(),
            Area {
                x0: -10.0,
                y0: 10.0,
                x1: 10.0,
                y1: 30.0
            }
        );
    }

    fn pango() -> pango::Context {
        use pango::prelude::FontMapExt;
        pangocairo::FontMap::default().create_context()
    }

    #[test]
    fn text_stays_inside_its_bubble() {
        let pc = pango();
        let wide = [[0.0, 0.0], [300.0, 40.0]];
        let words = "a rather long sentence that cannot possibly fit in a bubble this flat, \
                     and awordsolongthatitcouldneverfitonasinglelineofthisbubblewhatsoever";
        for kind in [Kind::Rect, Kind::Ellipse, Kind::Diamond] {
            let a = Area::of(&fit_label(&pc, kind, 22.0, &wide, words));
            assert_eq!((a.x0, a.x1), (0.0, 300.0), "only the height grows");
            assert!(a.height() > 40.0);
            assert!((a.y0 + a.y1 - 40.0).abs() < 1e-6, "around its center");
            let room = inner(kind, a);
            let (w, h) = label_layout(&pc, words, 22.0, room.width()).size();
            let unit = f64::from(pango::SCALE);
            assert!(f64::from(w) / unit <= room.width() + 0.5);
            assert!(f64::from(h) / unit <= room.height() + 0.5);
        }
        let narrow = [[0.0, 0.0], [20.0, 40.0]];
        assert!(Area::of(&fit_label(&pc, Kind::Rect, 22.0, &narrow, "abc")).width() > 20.0);
        assert_eq!(
            Area::of(&fit_label(&pc, Kind::Rect, 22.0, &wide, "")),
            Area::of(&wide)
        );
        let roomy = [[0.0, 0.0], [300.0, 80.0]];
        assert_eq!(
            Area::of(&fit_label(&pc, Kind::Rect, 22.0, &roomy, "ok")),
            Area::of(&roomy),
            "a bubble with room left does not move"
        );
    }

    #[test]
    fn links_stop_at_the_edges_they_face() {
        let a = item(Kind::Rect, vec![[0.0, 0.0], [100.0, 100.0]], None);
        let b = item(Kind::Ellipse, vec![[300.0, 0.0], [400.0, 100.0]], None);
        let pts = routed([0.0, 0.0], [0.0, 0.0], Some(&a), Some(&b), 5.0);
        assert_eq!(pts, vec![[105.0, 50.0], [295.0, 50.0]]);
        let free = routed([0.0, 0.0], [50.0, 300.0], Some(&a), None, 0.0);
        assert_eq!(free, vec![[50.0, 100.0], [50.0, 300.0]]);
        let gem = item(Kind::Diamond, vec![[0.0, 0.0], [100.0, 100.0]], None);
        assert_eq!(border(&gem, [50.0, -300.0], 0.0), [50.0, 0.0]);
    }

    #[test]
    fn links_are_kept_by_the_file() {
        let mut link = Item::new(
            Kind::Arrow,
            DEFAULT_STYLE,
            vec![[0.0, 0.0], [1.0, 1.0]],
            String::new(),
        );
        link.id = 3;
        link.from = Some(1);
        link.to = Some(2);
        let head = Head {
            name: "x".into(),
            view: [0.0, 0.0, 1.0],
        };
        let (_, read) = decode(&encode(&head, &[Rc::new(link)]).unwrap()).unwrap();
        assert_eq!(
            (read[0].id, read[0].from, read[0].to),
            (3, Some(1), Some(2))
        );
    }

    #[test]
    fn dark_boards_turn_lightness_over() {
        let lightness = |c: u32| {
            let (r, g, b) = ((c >> 16) & 255, (c >> 8) & 255, c & 255);
            (r.max(g).max(b) + r.min(g).min(b)) as f64 / 510.0
        };
        assert_eq!(shown(STROKES[0], false), STROKES[0]);
        assert!(
            lightness(shown(STROKES[0], true)) > 0.8,
            "black ink turns light"
        );
        let fill = shown(FILLS[1].unwrap(), true);
        assert!(lightness(fill) < 0.3, "pale fills turn deep");
        assert!((fill >> 16) & 255 > fill & 255, "and keep their hue");
        let blue = shown(STROKES[3], true);
        assert!(blue & 255 > (blue >> 16) & 255);
    }

    #[test]
    fn theme_tokens_are_read_from_the_sheet() {
        let css = "@define-color nexus_bg #141416; @define-color nexus_fg #ffffff;";
        let bg = token(css, "nexus_bg").unwrap();
        assert!((bg.red() - 20.0 / 255.0).abs() < 1e-3);
        assert!(token(css, "nexus_nope").is_none());
    }

    #[test]
    fn snapping_squares_shapes_and_rounds_angles() {
        assert_eq!(snapped(Kind::Rect, [0.0, 0.0], [10.0, -3.0]), [10.0, -10.0]);
        let p = snapped(Kind::Line, [0.0, 0.0], [10.0, 1.0]);
        assert!(p[1].abs() < 1e-9 && p[0] > 0.0);
    }
}
