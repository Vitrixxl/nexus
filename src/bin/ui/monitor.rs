//! The control center's Monitor page. Performance: every resource in a list
//! with its trend, the selected one in detail beside it. Processes: a sortable,
//! searchable table, flat or as a tree, with End behind a confirmation.
//! A thread samples once a second only while the page shows, and reads every
//! process only while the table shows; hidden widgets are neither relabelled
//! nor redrawn.
use super::super::{hbox, vbox};
use gtk::{cairo, gio, glib, prelude::*};
use nexus_control::{
    monitor::{self, Proc, Sample, Sampler},
    power,
};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// Seconds of history the graphs show.
const HISTORY: usize = 120;
/// Samples before a graph draws its series: two make a line.
const TREND: usize = 2;
/// Process rows built once and refilled; the rest are a search away.
const ROWS: usize = 150;
/// GPUs given a section; a third is unusual enough to leave out.
const GPUS: usize = 2;

fn text(content: &str, class: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(content));
    l.set_xalign(0.);
    for c in class.split_whitespace() {
        l.add_css_class(c);
    }
    l
}
/// Binary units, as btop and free show them.
fn size(bytes: u64) -> String {
    let b = bytes as f64;
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.0} KiB", b / 1024.),
        1_048_576..1_073_741_824 => format!("{:.1} MiB", b / 1_048_576.),
        _ => format!("{:.1} GiB", b / 1_073_741_824.),
    }
}
fn rate(bytes: u64) -> String {
    format!("{}/s", size(bytes))
}
fn duration(seconds: u64) -> String {
    let (d, h, m) = (seconds / 86400, seconds / 3600 % 24, seconds / 60 % 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else {
        format!("{h}h {m:02}m")
    }
}
fn pct(v: f32) -> String {
    format!("{:.0}%", v * 100.)
}
fn or_dash(v: Option<String>) -> String {
    v.unwrap_or_else(|| "—".into())
}
/// The graph's top: the smallest 1, 2 or 5 step at or above `v`, in binary
/// units for bytes, so the scale reads "2 MiB/s" rather than "1.9 MiB/s".
fn nice(v: f32, bytes: bool) -> (f32, String) {
    let (unit, name) = if !bytes {
        (1., "")
    } else {
        [
            (1_073_741_824., " GiB/s"),
            (1_048_576., " MiB/s"),
            (1024., " KiB/s"),
            (1., " B/s"),
        ]
        .into_iter()
        .find(|(u, _)| v >= *u)
        .unwrap_or((1024., " KiB/s"))
    };
    let x = (v / unit).max(1.);
    let p = 10f32.powf(x.log10().floor());
    let m = [1., 2., 5., 10.]
        .into_iter()
        .find(|m| m * p >= x)
        .unwrap_or(10.)
        * p;
    // 1000 KiB/s reads better as the 1 MiB/s just above it.
    if bytes && m >= 1000. && unit < 1_073_741_824. {
        return nice(unit * 1024., true);
    }
    (m * unit, format!("{m}{name}"))
}

/// A scrolling history: one series rising from the bottom, or two mirrored
/// around the middle (in above, out below), against `max`. The newest sample
/// sits at the right edge and each step is a second, so the width is always
/// the same two minutes.
struct History {
    up: VecDeque<f32>,
    down: VecDeque<f32>,
    max: f32,
}
type Series = Rc<RefCell<History>>;
fn series(max: f32) -> Series {
    Rc::new(RefCell::new(History {
        up: VecDeque::with_capacity(HISTORY),
        down: VecDeque::new(),
        max,
    }))
}
fn push(series: &mut VecDeque<f32>, value: f32) {
    if series.len() == HISTORY {
        series.pop_front();
    }
    series.push_back(value);
}
fn rgb(area: &gtk::DrawingArea) -> (f64, f64, f64) {
    let c = area.color();
    (
        f64::from(c.red()),
        f64::from(c.green()),
        f64::from(c.blue()),
    )
}
/// Draws `series`; `big` adds quarter gridlines.
fn plot(series: &Series, mirrored: bool, height: i32, big: bool) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.add_css_class("monitor-graph");
    area.set_content_height(height);
    area.set_hexpand(true);
    let h = series.clone();
    area.set_draw_func(move |area, cr, w, height| {
        let history = h.borrow();
        let (w, height) = (f64::from(w), f64::from(height));
        let (r, g, b) = rgb(area);
        if big {
            cr.set_source_rgba(r, g, b, 0.08);
            for i in 1..4 {
                cr.rectangle(0., (height * f64::from(i) / 4.).round() - 0.5, w, 1.);
            }
            let _ = cr.fill();
        }
        let max = f64::from(history.max.max(f32::EPSILON));
        let base = if mirrored { height / 2. } else { height };
        let span = if mirrored {
            height / 2. - 1.
        } else {
            height - 1.
        };
        let step = w / (HISTORY - 1) as f64;
        cr.set_line_join(cairo::LineJoin::Round);
        for (series, sign) in [(&history.up, -1.), (&history.down, 1.)] {
            if series.len() < TREND || (!mirrored && sign > 0.) {
                continue;
            }
            let x0 = w - (series.len() - 1) as f64 * step;
            let y = |v: f32| base + sign * (f64::from(v) / max).clamp(0., 1.) * span;
            cr.move_to(x0, y(series[0]));
            for (i, v) in series.iter().enumerate().skip(1) {
                cr.line_to(x0 + i as f64 * step, y(*v));
            }
            let Ok(line) = cr.copy_path() else { continue };
            cr.line_to(w, base);
            cr.line_to(x0, base);
            cr.close_path();
            let primary = sign < 0.;
            let fill = cairo::LinearGradient::new(0., base + sign * span, 0., base);
            fill.add_color_stop_rgba(0., r, g, b, if primary { 0.32 } else { 0.2 });
            fill.add_color_stop_rgba(1., r, g, b, 0.03);
            let _ = cr.set_source(&fill);
            let _ = cr.fill();
            cr.append_path(&line);
            cr.set_source_rgba(r, g, b, if primary { 1. } else { 0.6 });
            cr.set_line_width(if big { 1.5 } else { 1.25 });
            let _ = cr.stroke();
        }
        cr.set_source_rgba(r, g, b, 0.18);
        cr.rectangle(0., if mirrored { base - 0.5 } else { base - 1. }, w, 1.);
        let _ = cr.fill();
    });
    area
}
/// A graph with its caption, and its scale at the top right.
fn chart(
    series: &Series,
    mirrored: bool,
    caption: &str,
) -> (gtk::Box, gtk::DrawingArea, gtk::Label) {
    let b = vbox(4);
    let head = hbox(8);
    let caption = text(caption, "caption muted");
    caption.set_hexpand(true);
    let scale = text("", "caption muted monitor-number");
    head.append(&caption);
    head.append(&scale);
    b.append(&head);
    let area = plot(series, mirrored, 120, true);
    // Taller where the pane has room to spare.
    area.set_vexpand(true);
    b.append(&area);
    (b, area, scale)
}
/// Name over value, three to a row; `swatch` marks a part of a bar above.
fn stats(names: &[(&str, &str)]) -> (gtk::Grid, Vec<gtk::Label>) {
    let grid = gtk::Grid::new();
    grid.set_column_spacing(16);
    grid.set_row_spacing(10);
    grid.set_column_homogeneous(true);
    let values = names
        .iter()
        .enumerate()
        .map(|(i, (name, swatch))| {
            let cell = vbox(1);
            let head = hbox(6);
            if !swatch.is_empty() {
                let s = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                s.add_css_class("monitor-swatch");
                s.add_css_class(swatch);
                s.set_valign(gtk::Align::Center);
                head.append(&s);
            }
            head.append(&text(name, "caption muted"));
            cell.append(&head);
            let value = text("—", "monitor-stat");
            value.set_ellipsize(gtk::pango::EllipsizeMode::End);
            cell.append(&value);
            grid.attach(&cell, i as i32 % 3, i as i32 / 3, 1, 1);
            value
        })
        .collect();
    (grid, values)
}
fn set(labels: &[gtk::Label], values: [String; N]) {
    for (l, v) in labels.iter().zip(values) {
        l.set_text(&v);
    }
}
const N: usize = 6;

/// Bars side by side, one per value in 0..1: the cores.
fn bars(values: &Rc<RefCell<Vec<f32>>>) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.add_css_class("monitor-graph");
    area.set_content_height(40);
    area.set_hexpand(true);
    let v = values.clone();
    area.set_draw_func(move |area, cr, w, h| {
        let values = v.borrow();
        if values.is_empty() {
            return;
        }
        let (r, g, b) = rgb(area);
        let (w, h) = (f64::from(w), f64::from(h));
        let gap = 3.;
        let n = values.len() as f64;
        let bar = ((w - gap * (n - 1.)) / n).max(1.);
        cr.set_source_rgba(r, g, b, 0.08);
        for i in 0..values.len() {
            cr.rectangle(i as f64 * (bar + gap), 0., bar, h);
        }
        let _ = cr.fill();
        cr.set_source_rgba(r, g, b, 0.9);
        for (i, value) in values.iter().enumerate() {
            let filled = (h * f64::from(value.clamp(0., 1.))).round();
            cr.rectangle(i as f64 * (bar + gap), h - filled, bar, filled);
        }
        let _ = cr.fill();
    });
    area
}
/// Used, then cache, then free memory, as one bar.
fn composition(parts: &Rc<Cell<(f32, f32)>>) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.add_css_class("monitor-graph");
    area.set_content_height(8);
    area.set_hexpand(true);
    let p = parts.clone();
    area.set_draw_func(move |area, cr, w, h| {
        let (used, cache) = p.get();
        let (r, g, b) = rgb(area);
        let (w, h) = (f64::from(w), f64::from(h));
        let used_w = (w * f64::from(used)).round();
        let cache_w = (w * f64::from(cache)).round().min(w - used_w);
        for (x, width, alpha) in [
            (0., used_w, 1.),
            (used_w, cache_w, 0.4),
            (used_w + cache_w, w - used_w - cache_w, 0.12),
        ] {
            cr.set_source_rgba(r, g, b, alpha);
            cr.rectangle(x, 0., width, h);
            let _ = cr.fill();
        }
    });
    area
}

/// Rows of name and value, a detail line and an optional bar below; grown on
/// demand, extras hidden.
struct Lines {
    root: gtk::Box,
    rows: RefCell<Vec<Line>>,
}
/// The row, its name, value and detail, and its bar.
type Line = (
    gtk::Box,
    gtk::Label,
    gtk::Label,
    gtk::Label,
    gtk::ProgressBar,
);
impl Lines {
    fn new() -> Self {
        let root = vbox(10);
        Self {
            root,
            rows: RefCell::default(),
        }
    }
    fn set(&self, i: usize, name: &str, value: &str, detail: &str, bar: Option<(f64, bool)>) {
        let mut rows = self.rows.borrow_mut();
        if i == rows.len() {
            let row = vbox(3);
            let head = hbox(8);
            let n = text("", "monitor-line-name");
            n.set_hexpand(true);
            n.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            let v = text("", "monitor-number");
            v.set_xalign(1.);
            head.append(&n);
            head.append(&v);
            row.append(&head);
            let b = gtk::ProgressBar::new();
            b.add_css_class("monitor-bar");
            row.append(&b);
            let d = text("", "caption muted monitor-number");
            row.append(&d);
            self.root.append(&row);
            rows.push((row, n, v, d, b));
        }
        let (row, n, v, d, b) = &rows[i];
        row.set_visible(true);
        n.set_text(name);
        v.set_text(value);
        d.set_text(detail);
        d.set_visible(!detail.is_empty());
        b.set_visible(bar.is_some());
        if let Some((fraction, hot)) = bar {
            b.set_fraction(fraction.clamp(0., 1.));
            if hot {
                b.add_css_class("hot");
            } else {
                b.remove_css_class("hot");
            }
        }
    }
    fn finish(&self, n: usize) {
        for (row, ..) in self.rows.borrow().iter().skip(n) {
            row.set_visible(false);
        }
    }
}
fn section(title: &str) -> gtk::Label {
    let l = text(title, "monitor-section");
    l.set_margin_top(6);
    l
}
/// A detail pane: title and subtitle, then `body`, scrolling if it must.
fn pane(title: &str) -> (gtk::ScrolledWindow, gtk::Box, gtk::Label) {
    let b = vbox(14);
    b.add_css_class("monitor-pane");
    let head = hbox(8);
    let t = text(title, "monitor-title");
    t.set_hexpand(true);
    let subtitle = text("", "caption muted");
    subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
    subtitle.set_valign(gtk::Align::Baseline);
    t.set_valign(gtk::Align::Baseline);
    head.append(&t);
    head.append(&subtitle);
    b.append(&head);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(false)
        .vexpand(true)
        .hexpand(true)
        .child(&b)
        .build();
    (scroll, b, subtitle)
}

struct Nav {
    row: gtk::ListBoxRow,
    value: gtk::Label,
    spark: gtk::DrawingArea,
}
fn nav(list: &gtk::ListBox, name: &str, series: &Series, mirrored: bool) -> Nav {
    let b = vbox(4);
    let head = hbox(8);
    let n = text(name, "monitor-nav-name");
    n.set_hexpand(true);
    let value = text("—", "monitor-nav-value");
    value.set_xalign(1.);
    value.set_ellipsize(gtk::pango::EllipsizeMode::Start);
    head.append(&n);
    head.append(&value);
    b.append(&head);
    let spark = plot(series, mirrored, 26, false);
    b.append(&spark);
    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&b));
    list.append(&row);
    Nav { row, value, spark }
}

struct GpuView {
    root: gtk::Box,
    title: gtk::Label,
    note: gtk::Label,
    series: Series,
    graph: gtk::DrawingArea,
    stats: Vec<gtk::Label>,
}

#[derive(Clone, Copy, PartialEq)]
enum Sort {
    Pid,
    Name,
    User,
    Cpu,
    Memory,
    Threads,
}
struct Row {
    row: gtk::ListBoxRow,
    cells: [gtk::Label; 6],
    shows: Cell<i32>,
}
/// Name, PID, user, CPU, memory, threads.
fn columns() -> [gtk::Label; 6] {
    let cell = |chars: i32, xalign: f32| {
        let l = text("", "");
        l.set_width_chars(chars);
        l.set_max_width_chars(chars);
        l.set_xalign(xalign);
        l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        l
    };
    let name = cell(0, 0.);
    name.set_hexpand(true);
    [
        name,
        cell(7, 1.),
        cell(9, 0.),
        cell(6, 1.),
        cell(9, 1.),
        cell(4, 1.),
    ]
}
const COLUMNS: [(Sort, &str); 6] = [
    (Sort::Name, "Name"),
    (Sort::Pid, "PID"),
    (Sort::User, "User"),
    (Sort::Cpu, "CPU %"),
    (Sort::Memory, "Memory"),
    (Sort::Threads, "Threads"),
];
/// Users by uid, read once.
fn users() -> HashMap<u32, String> {
    std::fs::read_to_string("/etc/passwd")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut f = l.split(':');
            let name = f.next()?;
            Some((f.nth(1)?.parse().ok()?, name.to_string()))
        })
        .collect()
}

#[derive(Default)]
struct Parts {
    navs: HashMap<&'static str, Nav>,
    graphs: HashMap<&'static str, Vec<gtk::DrawingArea>>,
    scales: HashMap<&'static str, gtk::Label>,
    subtitles: HashMap<&'static str, gtk::Label>,
    stats: HashMap<&'static str, Vec<gtk::Label>>,
}
struct Page {
    tabs: gtk::Stack,
    detail: gtk::Stack,
    want_procs: Arc<AtomicBool>,
    sample: RefCell<Sample>,
    profile: RefCell<Option<String>>,
    // Performance.
    navs: HashMap<&'static str, Nav>,
    cpu: Series,
    memory: Series,
    network: Series,
    disk: Series,
    battery: Series,
    heat: Series,
    graphs: HashMap<&'static str, Vec<gtk::DrawingArea>>,
    scales: HashMap<&'static str, gtk::Label>,
    subtitles: HashMap<&'static str, gtk::Label>,
    stats: HashMap<&'static str, Vec<gtk::Label>>,
    cores: Rc<RefCell<Vec<f32>>>,
    parts: Rc<Cell<(f32, f32)>>,
    gpus: Vec<GpuView>,
    devices: Lines,
    mounts: Lines,
    links: Lines,
    sensors: Lines,
    // Processes.
    users: HashMap<u32, String>,
    sort: Cell<(Sort, bool)>,
    tree: Cell<bool>,
    filter: RefCell<String>,
    selected: Cell<Option<i32>>,
    rows: Vec<Row>,
    list: gtk::ListBox,
    headers: Vec<(Sort, gtk::Button, &'static str)>,
    count: gtk::Label,
    footer: gtk::Stack,
    info: gtk::Label,
    end: gtk::Button,
    question: gtk::Label,
    consequence: gtk::Label,
    #[cfg(feature = "lab")]
    search: gtk::SearchEntry,
    #[cfg(feature = "lab")]
    tab_buttons: [gtk::ToggleButton; 3],
    /// The process the confirmation names, fixed when it was asked.
    asking: RefCell<Option<(i32, String)>>,
    /// The info line holds what End did.
    told: Cell<bool>,
    empty: gtk::Label,
}

impl Page {
    /// Every sample feeds the histories, shown or not, so a graph opened later
    /// already has its past.
    fn record(&self, s: &Sample) {
        push(&mut self.cpu.borrow_mut().up, s.cpu);
        let used = s.memory_used as f32 / s.memory_total.max(1) as f32;
        push(&mut self.memory.borrow_mut().up, used);
        for (series, a, b) in [
            (&self.network, s.received, s.sent),
            (&self.disk, s.read, s.written),
        ] {
            let mut h = series.borrow_mut();
            push(&mut h.up, a as f32);
            push(&mut h.down, b as f32);
            let top = h.up.iter().chain(&h.down).fold(1024f32, |m, v| m.max(*v));
            h.max = nice(top, true).0;
        }
        if let Some(b) = &s.battery {
            let mut h = self.battery.borrow_mut();
            push(&mut h.up, b.watts);
            h.max = nice(h.up.iter().fold(5f32, |m, v| m.max(*v)), false).0;
        }
        if let Some(t) = s.temperature {
            push(&mut self.heat.borrow_mut().up, t);
        }
        for (view, gpu) in self.gpus.iter().zip(&s.gpus) {
            push(&mut view.series.borrow_mut().up, gpu.busy.unwrap_or(0.));
        }
    }
    /// Labels and redraws what is on screen.
    fn show(&self) {
        let s = self.sample.borrow();
        match self.tabs.visible_child_name().as_deref() {
            Some("processes") => self.procs(&s),
            _ => {
                self.overview(&s);
                let pane = self.detail.visible_child_name();
                let pane = pane.as_deref().unwrap_or("cpu");
                self.pane(pane, &s);
                for area in self.graphs.get(pane).into_iter().flatten() {
                    area.queue_draw();
                }
            }
        }
    }
    fn overview(&self, s: &Sample) {
        let nav = |name: &str, value: String| {
            let n = &self.navs[name];
            n.value.set_text(&value);
            n.spark.queue_draw();
        };
        nav(
            "cpu",
            match s.temperature {
                Some(t) => format!("{} · {t:.0}°C", pct(s.cpu)),
                None => pct(s.cpu),
            },
        );
        nav(
            "memory",
            format!(
                "{} · {}",
                size(s.memory_used),
                pct(s.memory_used as f32 / s.memory_total.max(1) as f32)
            ),
        );
        let gpu = s.gpus.first();
        self.navs["gpu"].row.set_visible(gpu.is_some());
        if let Some(g) = gpu {
            nav(
                "gpu",
                if g.asleep {
                    "Asleep".into()
                } else {
                    or_dash(g.busy.map(pct))
                },
            );
        }
        nav(
            "disk",
            format!("R {} · W {}", size(s.read), size(s.written)),
        );
        nav(
            "network",
            format!("↓ {} · ↑ {}", size(s.received), size(s.sent)),
        );
        self.navs["battery"].row.set_visible(s.battery.is_some());
        if let Some(b) = &s.battery {
            nav("battery", format!("{:.0}% · {:.1} W", b.percent, b.watts));
        }
        let hottest = s.sensors.iter().map(|t| t.celsius).fold(f32::MIN, f32::max);
        self.navs["sensors"].row.set_visible(!s.sensors.is_empty());
        nav("sensors", format!("{hottest:.0}°C"));
    }
    fn pane(&self, pane: &str, s: &Sample) {
        let stats = |values| set(&self.stats[pane], values);
        let scale = |text: &str| self.scales[pane].set_text(text);
        match pane {
            "cpu" => {
                *self.cores.borrow_mut() = s.cores.clone();
                stats([
                    pct(s.cpu),
                    format!("{:.2} GHz", s.clock as f32 / 1000.),
                    or_dash(s.temperature.map(|t| format!("{t:.0}°C"))),
                    format!("{:.2}  {:.2}  {:.2}", s.load[0], s.load[1], s.load[2]),
                    format!("{} · {}", s.processes, s.threads),
                    duration(s.uptime),
                ]);
            }
            "memory" => {
                let total = s.memory_total.max(1) as f32;
                // Used, cache and free add up to the total: the cache counted is
                // what the kernel would hand back, the rest of what is available.
                let cache = s.memory_available.saturating_sub(s.memory_free);
                self.parts
                    .set((s.memory_used as f32 / total, cache as f32 / total));
                stats([
                    size(s.memory_used),
                    size(cache),
                    size(s.memory_free),
                    size(s.memory_available),
                    if s.swap_total == 0 {
                        "Off".into()
                    } else {
                        format!("{} / {}", size(s.swap_used), size(s.swap_total))
                    },
                    match s.zram {
                        Some((stored, packed)) if stored > 0 => {
                            format!("{} in {}", size(stored), size(packed))
                        }
                        _ => "Off".into(),
                    },
                ]);
                self.subtitles["memory"].set_text(&format!(
                    "{} used of {}",
                    pct(s.memory_used as f32 / total),
                    size(s.memory_total)
                ));
            }
            "gpu" => {
                for (i, view) in self.gpus.iter().enumerate() {
                    let Some(g) = s.gpus.get(i) else {
                        view.root.set_visible(false);
                        continue;
                    };
                    view.root.set_visible(true);
                    view.title.set_text(&g.name);
                    view.note.set_visible(g.asleep);
                    view.graph.queue_draw();
                    set(
                        &view.stats,
                        [
                            if g.asleep {
                                "Asleep".into()
                            } else {
                                or_dash(g.busy.map(pct))
                            },
                            or_dash(g.clock.map(|(c, m)| format!("{c} / {m} MHz"))),
                            or_dash(g.memory.map(|(u, t)| format!("{} / {}", size(u), size(t)))),
                            or_dash(g.temperature.map(|t| format!("{t:.0}°C"))),
                            or_dash(g.watts.map(|w| format!("{w:.1} W"))),
                            String::new(),
                        ],
                    );
                }
            }
            "disk" => {
                scale(&nice(self.disk.borrow().max, true).1);
                stats([
                    rate(s.read),
                    rate(s.written),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                ]);
                for (i, d) in s.devices.iter().enumerate() {
                    self.devices.set(
                        i,
                        &d.name,
                        &format!("R {} · W {}", rate(d.read), rate(d.written)),
                        "",
                        None,
                    );
                }
                self.devices.finish(s.devices.len());
                for (i, m) in s.mounts.iter().enumerate() {
                    let used = m.used as f64 / m.total.max(1) as f64;
                    self.mounts.set(
                        i,
                        &m.path,
                        &format!("{} / {}", size(m.used), size(m.total)),
                        &format!("{} free", size(m.total - m.used)),
                        Some((used, used > 0.9)),
                    );
                }
                self.mounts.finish(s.mounts.len());
            }
            "network" => {
                scale(&nice(self.network.borrow().max, true).1);
                let (rx, tx) = s
                    .links
                    .iter()
                    .fold((0, 0), |(r, t), l| (r + l.total_received, t + l.total_sent));
                stats([
                    rate(s.received),
                    rate(s.sent),
                    size(rx),
                    size(tx),
                    String::new(),
                    String::new(),
                ]);
                for (i, l) in s.links.iter().enumerate() {
                    self.links.set(
                        i,
                        &l.name,
                        &format!("↓ {}  ↑ {}", rate(l.received), rate(l.sent)),
                        &format!(
                            "{} received · {} sent since boot",
                            size(l.total_received),
                            size(l.total_sent)
                        ),
                        None,
                    );
                }
                self.links.finish(s.links.len());
            }
            "battery" => {
                let Some(b) = &s.battery else { return };
                scale(&format!("{} W", self.battery.borrow().max));
                stats([
                    format!("{:.0}%", b.percent),
                    b.status.clone(),
                    format!("{:.1} W", b.watts),
                    or_dash(b.seconds.map(duration)),
                    or_dash(b.health.map(pct)),
                    or_dash(self.profile.borrow().clone()),
                ]);
                self.subtitles["battery"].set_text(&match b.cycles {
                    Some(c) => format!("{c} charge cycles"),
                    None => String::new(),
                });
            }
            "sensors" => {
                let mut i = 0;
                for t in &s.sensors {
                    self.sensors.set(
                        i,
                        &t.name,
                        &format!("{:.0}°C", t.celsius),
                        "",
                        Some((f64::from(t.celsius) / 100., t.celsius >= 90.)),
                    );
                    i += 1;
                }
                for g in &s.gpus {
                    if let Some(t) = g.temperature {
                        self.sensors.set(
                            i,
                            &g.name,
                            &format!("{t:.0}°C"),
                            "",
                            Some((f64::from(t) / 100., t >= 90.)),
                        );
                        i += 1;
                    }
                }
                self.sensors.finish(i);
            }
            _ => {}
        }
    }

    fn procs(&self, sample: &Sample) {
        let filter = self.filter.borrow().to_lowercase();
        let user = |p: &Proc| self.users.get(&p.uid).map_or("", String::as_str);
        let (sort, descending) = self.sort.get();
        let order = |a: &&Proc, b: &&Proc| {
            let order = match sort {
                Sort::Pid => a.pid.cmp(&b.pid),
                Sort::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                Sort::User => user(a).cmp(user(b)),
                Sort::Cpu => a.cpu.total_cmp(&b.cpu),
                Sort::Memory => a.memory.cmp(&b.memory),
                Sort::Threads => a.threads.cmp(&b.threads),
            };
            let order = if descending { order.reverse() } else { order };
            order.then(a.pid.cmp(&b.pid))
        };
        // (process, depth)
        let mut shown: Vec<(&Proc, usize)>;
        if self.tree.get() && filter.is_empty() {
            let pids: std::collections::HashSet<i32> = sample.procs.iter().map(|p| p.pid).collect();
            let mut children: HashMap<i32, Vec<&Proc>> = HashMap::new();
            for p in &sample.procs {
                let parent = if pids.contains(&p.ppid) { p.ppid } else { 0 };
                children.entry(parent).or_default().push(p);
            }
            for c in children.values_mut() {
                c.sort_unstable_by(order);
            }
            shown = Vec::with_capacity(sample.procs.len());
            let mut stack: Vec<(&Proc, usize)> = children
                .get(&0)
                .into_iter()
                .flatten()
                .rev()
                .map(|p| (*p, 0))
                .collect();
            while let Some((p, depth)) = stack.pop() {
                shown.push((p, depth));
                if let Some(c) = children.get(&p.pid) {
                    stack.extend(c.iter().rev().map(|c| (*c, depth + 1)));
                }
            }
        } else {
            shown = sample
                .procs
                .iter()
                .filter(|p| {
                    filter.is_empty()
                        || p.name.to_lowercase().contains(&filter)
                        || p.pid.to_string().starts_with(&filter)
                        || user(p).starts_with(&filter)
                })
                .map(|p| (p, 0))
                .collect();
            shown.sort_unstable_by(|a, b| order(&a.0, &b.0));
        }
        if shown.is_empty() {
            self.empty.set_text(&if filter.is_empty() {
                "Reading processes…".to_string()
            } else {
                format!("No process matches “{filter}”")
            });
        }
        self.count
            .set_text(&match (shown.len(), filter.is_empty()) {
                (n, true) if n > ROWS && self.tree.get() => {
                    format!("{n} processes · first {ROWS} shown")
                }
                (n, true) if n > ROWS => format!("{n} processes · top {ROWS} shown"),
                (n, true) => format!("{n} processes"),
                (1, false) => "1 match".into(),
                (n, false) => format!("{n} matches"),
            });
        let selected = self.selected.get();
        let mut still = None;
        for (i, row) in self.rows.iter().enumerate() {
            let Some((p, depth)) = shown.get(i) else {
                row.row.set_visible(false);
                continue;
            };
            row.row.set_visible(true);
            row.shows.set(p.pid);
            let [name, pid, user_, cpu, memory, threads] = &row.cells;
            name.set_margin_start(14 * (*depth).min(8) as i32);
            name.set_text(&p.name);
            pid.set_text(&p.pid.to_string());
            user_.set_text(user(p));
            cpu.set_text(&format!("{:.1}", p.cpu * 100.));
            memory.set_text(&size(p.memory));
            threads.set_text(&p.threads.to_string());
            if Some(p.pid) == selected {
                still = Some(&row.row);
            }
        }
        // Selection follows the process, wherever sorting moves it.
        match still {
            Some(row) => self.list.select_row(Some(row)),
            None => self.list.unselect_all(),
        }
        for (s, button, title) in &self.headers {
            let arrow = match (*s == sort, descending) {
                (false, _) => "",
                (true, true) => " ↓",
                (true, false) => " ↑",
            };
            button.set_label(&format!("{title}{arrow}"));
            if *s == sort {
                button.add_css_class("active");
            } else {
                button.remove_css_class("active");
            }
        }
    }
    fn select(&self, pid: Option<i32>) {
        self.selected.set(pid);
        self.end.set_sensitive(pid.is_some());
        if pid.is_some() {
            self.told.set(false);
        }
        if !self.told.get() {
            self.info.remove_css_class("error");
        }
        let s = self.sample.borrow();
        let Some(p) = pid.and_then(|pid| s.procs.iter().find(|p| p.pid == pid)) else {
            // The outcome of End stays until another process is picked.
            if self.told.get() {
                return;
            }
            self.info
                .set_text("Select a process to see its command and end it");
            self.info.set_tooltip_text(None);
            self.footer.set_visible_child_name("info");
            return;
        };
        let command = std::fs::read(format!("/proc/{}/cmdline", p.pid))
            .map(|c| {
                String::from_utf8_lossy(&c)
                    .replace('\0', " ")
                    .trim()
                    .to_string()
            })
            .unwrap_or_default();
        self.info.set_text(&if command.is_empty() {
            format!("{} · PID {}", p.name, p.pid)
        } else {
            format!("PID {} · {command}", p.pid)
        });
        self.info.set_tooltip_text(Some(&command));
    }
    /// Asks before ending: the process is fixed now, not when the answer comes.
    fn ask(&self) {
        let s = self.sample.borrow();
        let Some(p) = self
            .selected
            .get()
            .and_then(|pid| s.procs.iter().find(|p| p.pid == pid))
        else {
            return;
        };
        self.question.set_text(&format!("End “{}”?", p.name));
        self.consequence
            .set_text(&format!("PID {} · unsaved work in it may be lost", p.pid));
        *self.asking.borrow_mut() = Some((p.pid, p.name.clone()));
        self.footer.set_visible_child_name("confirm");
    }
    fn cancel(&self) {
        self.asking.borrow_mut().take();
        self.footer.set_visible_child_name("info");
    }
    fn end(&self, force: bool) {
        let Some((pid, name)) = self.asking.borrow_mut().take() else {
            return;
        };
        self.footer.set_visible_child_name("info");
        // The PID could have gone to another process since the question.
        let same = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .is_ok_and(|s| s.contains(&format!("({name})")));
        // SAFETY: kill only sends a signal.
        let sent = same
            && unsafe { libc::kill(pid, if force { libc::SIGKILL } else { libc::SIGTERM }) } == 0;
        let error = std::io::Error::last_os_error().raw_os_error();
        self.info.set_text(&match (sent, error) {
            (true, _) if force => format!("Forced “{name}” to quit"),
            (true, _) => format!("Asked “{name}” to end"),
            (false, _) if !same => format!("“{name}” has already ended"),
            (false, Some(libc::EPERM)) => {
                format!("“{name}” belongs to another user: it cannot be ended from here")
            }
            (false, _) => format!("“{name}” could not be ended"),
        });
        if !sent {
            self.info.add_css_class("error");
        }
        self.told.set(true);
    }
}

pub fn page() -> gtk::Box {
    let root = vbox(14);
    root.add_css_class("monitor");
    root.add_css_class("monitor-page");

    // Performance: the list, then the selected resource.
    let cpu = series(1.);
    let memory = series(1.);
    let network = series(1024.);
    let disk = series(1024.);
    let battery = series(5.);
    let heat = series(100.);
    let menu = gtk::ListBox::new();
    menu.add_css_class("monitor-nav");
    menu.set_selection_mode(gtk::SelectionMode::Browse);
    let detail = gtk::Stack::new();
    detail.set_hexpand(true);
    let mut parts = Parts::default();
    let add = |parts: &mut Parts,
               key: &'static str,
               name: &str,
               series: &Series,
               mirrored: bool,
               caption: &str,
               stat_names: &[(&str, &str)]|
     -> gtk::Box {
        parts.navs.insert(key, nav(&menu, name, series, mirrored));
        let (scroll, b, subtitle) = pane(name);
        parts.subtitles.insert(key, subtitle);
        if !caption.is_empty() {
            let (c, area, scale) = chart(series, mirrored, caption);
            b.append(&c);
            parts.graphs.entry(key).or_default().push(area);
            parts.scales.insert(key, scale);
        }
        if !stat_names.is_empty() {
            let (grid, values) = stats(stat_names);
            b.append(&grid);
            parts.stats.insert(key, values);
        }
        detail.add_named(&scroll, Some(key));
        b
    };
    let cpu_pane = add(
        &mut parts,
        "cpu",
        "CPU",
        &cpu,
        false,
        "Utilization · 2 minutes",
        &[
            ("Utilization", ""),
            ("Speed", ""),
            ("Temperature", ""),
            ("Load 1 · 5 · 15 min", ""),
            ("Processes · Threads", ""),
            ("Up time", ""),
        ],
    );
    let cores = Rc::new(RefCell::new(Vec::new()));
    cpu_pane.append(&section("Cores"));
    let core_bars = bars(&cores);
    cpu_pane.append(&core_bars);
    parts.graphs.entry("cpu").or_default().push(core_bars);
    parts.subtitles["cpu"].set_text(&monitor::cpu_model());

    let memory_pane = add(
        &mut parts,
        "memory",
        "Memory",
        &memory,
        false,
        "In use · 2 minutes",
        &[],
    );
    let split = Rc::new(Cell::new((0., 0.)));
    let bar = composition(&split);
    memory_pane.append(&bar);
    parts.graphs.entry("memory").or_default().push(bar);
    let (grid, values) = stats(&[
        ("Used", "used"),
        ("Cache", "cache"),
        ("Free", "free"),
        ("Available", ""),
        ("Swap", ""),
        ("Compressed", ""),
    ]);
    memory_pane.append(&grid);
    parts.stats.insert("memory", values);

    let gpu_series = series(1.);
    let gpu_pane = add(&mut parts, "gpu", "GPU", &gpu_series, false, "", &[]);
    let gpus: Vec<GpuView> = (0..GPUS)
        .map(|i| {
            let root = vbox(10);
            let title = text("", "monitor-section");
            root.append(&title);
            let note = text(
                "Asleep to save power: not read, so as not to wake it.",
                "caption muted",
            );
            note.set_wrap(true);
            root.append(&note);
            let series = if i == 0 {
                gpu_series.clone()
            } else {
                series(1.)
            };
            let (c, graph, scale) = chart(&series, false, "Utilization · 2 minutes");
            scale.set_text("100%");
            root.append(&c);
            let (grid, stats) = stats(&[
                ("Utilization", ""),
                ("Clock", ""),
                ("Memory", ""),
                ("Temperature", ""),
                ("Power", ""),
            ]);
            root.append(&grid);
            gpu_pane.append(&root);
            GpuView {
                root,
                title,
                note,
                series,
                graph,
                stats,
            }
        })
        .collect();

    let disk_pane = add(
        &mut parts,
        "disk",
        "Disk",
        &disk,
        true,
        "Read above, write below · 2 minutes",
        &[("Read", ""), ("Write", "")],
    );
    let devices = Lines::new();
    disk_pane.append(&section("Drives"));
    disk_pane.append(&devices.root);
    let mounts = Lines::new();
    disk_pane.append(&section("Volumes"));
    disk_pane.append(&mounts.root);

    let network_pane = add(
        &mut parts,
        "network",
        "Network",
        &network,
        true,
        "Received above, sent below · 2 minutes",
        &[
            ("Download", ""),
            ("Upload", ""),
            ("Received since boot", ""),
            ("Sent since boot", ""),
        ],
    );
    let links = Lines::new();
    network_pane.append(&section("Interfaces"));
    network_pane.append(&links.root);

    add(
        &mut parts,
        "battery",
        "Battery",
        &battery,
        false,
        "Power draw · 2 minutes",
        &[
            ("Charge", ""),
            ("State", ""),
            ("Power", ""),
            ("Time left", ""),
            ("Health", ""),
            ("Power mode", ""),
        ],
    );
    let sensors_pane = add(
        &mut parts,
        "sensors",
        "Temperatures",
        &heat,
        false,
        "CPU package · 2 minutes",
        &[],
    );
    parts.scales["sensors"].set_text("100°C");
    parts.scales["cpu"].set_text("100%");
    parts.scales["memory"].set_text("100%");
    let sensors = Lines::new();
    sensors_pane.append(&sensors.root);

    let performance = hbox(12);
    let menu_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&menu)
        .build();
    menu_scroll.add_css_class("monitor-nav-scroll");
    menu_scroll.set_size_request(200, -1);
    performance.append(&menu_scroll);
    let detail_card = vbox(0);
    detail_card.add_css_class("card");
    detail_card.add_css_class("monitor-detail");
    detail_card.append(&detail);
    performance.append(&detail_card);

    // Processes.
    let procs = vbox(10);
    procs.add_css_class("card");
    procs.set_vexpand(true);
    let head = hbox(10);
    let search = gtk::SearchEntry::new();
    search.add_css_class("monitor-search");
    search.set_placeholder_text(Some("Filter by name, PID or user"));
    search.set_hexpand(true);
    head.append(&search);
    let tree = gtk::ToggleButton::with_label("Tree");
    tree.add_css_class("monitor-tree");
    tree.set_tooltip_text(Some("Show each process under the one that started it"));
    head.append(&tree);
    procs.append(&head);
    let header = hbox(12);
    header.add_css_class("monitor-columns");
    let mut headers = vec![];
    for (label, (sort, title)) in columns().iter().zip(COLUMNS) {
        let button = gtk::Button::with_label(title);
        button.set_tooltip_text(Some("Sort by this column"));
        button.set_hexpand(label.hexpands());
        if let Some(child) = button.child().and_downcast::<gtk::Label>() {
            child.set_width_chars(label.width_chars());
            child.set_xalign(label.xalign());
        }
        header.append(&button);
        headers.push((sort, button, title));
    }
    // In line with the rows' padding, and room on the right for the scrollbar,
    // which would cover the last column.
    header.set_margin_start(8);
    header.set_margin_end(14 + 8);
    procs.append(&header);
    let list = gtk::ListBox::new();
    list.add_css_class("monitor-list");
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.set_margin_end(14);
    let empty = text("", "muted");
    empty.set_xalign(0.5);
    empty.set_margin_top(24);
    list.set_placeholder(Some(&empty));
    let rows: Vec<Row> = (0..ROWS)
        .map(|_| {
            let cells = columns();
            let line = hbox(12);
            for l in &cells {
                line.append(l);
            }
            let row = gtk::ListBoxRow::new();
            row.set_child(Some(&line));
            row.set_visible(false);
            list.append(&row);
            Row {
                row,
                cells,
                shows: Cell::new(0),
            }
        })
        .collect();
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();
    procs.append(&scroll);
    let footer = gtk::Stack::new();
    footer.add_css_class("monitor-footer");
    footer.set_transition_type(gtk::StackTransitionType::Crossfade);
    footer.set_transition_duration(120);
    let info_row = hbox(10);
    let info = text("", "caption muted");
    info.set_hexpand(true);
    info.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let count = text("", "caption muted monitor-number");
    let end = gtk::Button::with_label("End…");
    end.add_css_class("monitor-end");
    end.set_sensitive(false);
    end.set_tooltip_text(Some("End the selected process (Delete)"));
    info_row.append(&info);
    info_row.append(&count);
    info_row.append(&end);
    footer.add_named(&info_row, Some("info"));
    let confirm_row = hbox(8);
    let asked = vbox(0);
    asked.set_hexpand(true);
    asked.set_valign(gtk::Align::Center);
    let question = text("", "monitor-question");
    question.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    let consequence = text("", "caption muted");
    consequence.set_ellipsize(gtk::pango::EllipsizeMode::End);
    asked.append(&question);
    asked.append(&consequence);
    let cancel = gtk::Button::with_label("Cancel");
    cancel.add_css_class("flat");
    cancel.add_css_class("monitor-end");
    let force = gtk::Button::with_label("Force quit");
    force.add_css_class("monitor-end");
    force.set_tooltip_text(Some("Stop it at once, without letting it save or clean up"));
    let confirm = gtk::Button::with_label("End process");
    confirm.add_css_class("monitor-end");
    confirm.add_css_class("destructive-action");
    confirm_row.append(&asked);
    confirm_row.append(&cancel);
    confirm_row.append(&force);
    confirm_row.append(&confirm);
    footer.add_named(&confirm_row, Some("confirm"));
    procs.append(&footer);

    let tabs = gtk::Stack::new();
    tabs.set_transition_type(gtk::StackTransitionType::Crossfade);
    tabs.set_transition_duration(150);
    tabs.set_vexpand(true);
    tabs.add_named(&performance, Some("performance"));
    tabs.add_named(&procs, Some("processes"));
    let switch = hbox(0);
    switch.add_css_class("segmented");
    switch.set_halign(gtk::Align::Start);
    let perf_button = gtk::ToggleButton::with_label("Performance");
    let procs_button = gtk::ToggleButton::with_label("Processes");
    procs_button.set_group(Some(&perf_button));
    perf_button.set_active(true);
    switch.append(&perf_button);
    switch.append(&procs_button);
    root.append(&switch);
    root.append(&tabs);

    let want_procs = Arc::new(AtomicBool::new(false));
    let page = Rc::new(Page {
        tabs: tabs.clone(),
        detail: detail.clone(),
        want_procs: want_procs.clone(),
        sample: RefCell::default(),
        profile: RefCell::default(),
        navs: parts.navs,
        cpu,
        memory,
        network,
        disk,
        battery,
        heat,
        graphs: parts.graphs,
        scales: parts.scales,
        subtitles: parts.subtitles,
        stats: parts.stats,
        cores,
        parts: split,
        gpus,
        devices,
        mounts,
        links,
        sensors,
        users: users(),
        sort: Cell::new((Sort::Cpu, true)),
        tree: Cell::new(false),
        filter: RefCell::default(),
        selected: Cell::new(None),
        rows,
        list: list.clone(),
        headers,
        count,
        footer,
        info,
        end: end.clone(),
        question,
        consequence,
        #[cfg(feature = "lab")]
        search: search.clone(),
        #[cfg(feature = "lab")]
        tab_buttons: [perf_button.clone(), procs_button.clone(), tree.clone()],
        asking: RefCell::default(),
        told: Cell::new(false),
        empty: empty.clone(),
    });
    page.select(None);

    for (button, name) in [(&perf_button, "performance"), (&procs_button, "processes")] {
        let p = Rc::downgrade(&page);
        button.connect_toggled(move |b| {
            let Some(p) = p.upgrade() else { return };
            if b.is_active() {
                p.tabs.set_visible_child_name(name);
                p.want_procs.store(name == "processes", Ordering::Relaxed);
                p.show();
            }
        });
    }
    if let Some(first) = menu.row_at_index(0) {
        menu.select_row(Some(&first));
    }
    let p = Rc::downgrade(&page);
    menu.connect_row_selected(move |_, row| {
        let (Some(p), Some(row)) = (p.upgrade(), row) else {
            return;
        };
        if let Some((key, _)) = p.navs.iter().find(|(_, n)| &n.row == row) {
            p.detail.set_visible_child_name(key);
            p.show();
        }
    });
    for (sort, button, _) in &page.headers {
        let (p, sort) = (Rc::downgrade(&page), *sort);
        button.connect_clicked(move |_| {
            let Some(p) = p.upgrade() else { return };
            let (current, descending) = p.sort.get();
            // Numbers start largest first, words from A.
            p.sort.set(if current == sort {
                (sort, !descending)
            } else {
                (sort, !matches!(sort, Sort::Name | Sort::Pid | Sort::User))
            });
            p.show();
        });
    }
    let p = Rc::downgrade(&page);
    tree.connect_toggled(move |t| {
        if let Some(p) = p.upgrade() {
            p.tree.set(t.is_active());
            p.show();
        }
    });
    let p = Rc::downgrade(&page);
    search.connect_search_changed(move |search| {
        if let Some(p) = p.upgrade() {
            *p.filter.borrow_mut() = search.text().trim().to_string();
            p.show();
        }
    });
    let p = Rc::downgrade(&page);
    list.connect_row_selected(move |_, row| {
        let Some(p) = p.upgrade() else { return };
        let pid = row.and_then(|row| p.rows.iter().find(|r| &r.row == row).map(|r| r.shows.get()));
        if pid != p.selected.get() {
            p.cancel();
            p.select(pid);
        }
    });
    for (button, action) in [(&end, 0), (&cancel, 1), (&force, 2), (&confirm, 3)] {
        let p = Rc::downgrade(&page);
        button.connect_clicked(move |_| {
            let Some(p) = p.upgrade() else { return };
            match action {
                0 => p.ask(),
                1 => p.cancel(),
                n => p.end(n == 2),
            }
        });
    }
    let key = gtk::EventControllerKey::new();
    let (p, search2) = (Rc::downgrade(&page), search.clone());
    key.connect_key_pressed(move |_, key, _, _| {
        let Some(p) = p.upgrade() else {
            return glib::Propagation::Proceed;
        };
        match key {
            // Otherwise Escape reaches the window, which closes.
            gtk::gdk::Key::Escape if p.asking.borrow().is_some() => p.cancel(),
            gtk::gdk::Key::Escape if !search2.text().is_empty() => search2.set_text(""),
            gtk::gdk::Key::Delete
                if !search2.has_focus()
                    && p.tabs.visible_child_name().as_deref() == Some("processes") =>
            {
                p.ask()
            }
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    root.add_controller(key);

    // The sample travels to the page and back, so its buffers are reused.
    let (back_tx, back_rx) = async_channel::bounded::<Sample>(1);
    let p = page.clone();
    let update: Rc<dyn Fn(Sample)> = Rc::new(move |sample: Sample| {
        p.record(&sample);
        let old = p.sample.replace(sample);
        let _ = back_tx.try_send(old);
        p.show();
        #[cfg(feature = "lab")]
        lab(&p);
    });
    // The page is mapped while it is the one shown in an open window: sample
    // then, and stop the thread as soon as it is not.
    let running = Rc::new(RefCell::new(None::<Arc<AtomicBool>>));
    let (r, p) = (running.clone(), Rc::downgrade(&page));
    root.connect_map(move |_| {
        if r.borrow().is_some() {
            return;
        }
        let flag = Arc::new(AtomicBool::new(true));
        *r.borrow_mut() = Some(flag.clone());
        let (tx, rx) = async_channel::bounded::<Sample>(1);
        let (want, back) = (want_procs.clone(), back_rx.clone());
        std::thread::spawn(move || {
            let mut sampler = Sampler::default();
            let mut s = Sample::default();
            sampler.sample(&mut s, want.load(Ordering::Relaxed));
            // A first reading soon, then once a second.
            let mut wait = Duration::from_millis(300);
            loop {
                std::thread::sleep(wait);
                wait = Duration::from_secs(1);
                if !flag.load(Ordering::Relaxed) {
                    break;
                }
                sampler.sample(&mut s, want.load(Ordering::Relaxed));
                if tx.send_blocking(s).is_err() {
                    break;
                }
                s = back.try_recv().unwrap_or_default();
            }
        });
        let update = update.clone();
        glib::spawn_future_local(async move {
            while let Ok(sample) = rx.recv().await {
                update(sample);
            }
        });
        // The power mode, read once a visit: it is only shown, never changed here.
        let p = p.clone();
        glib::spawn_future_local(async move {
            let Ok(bus) = gio::bus_get_future(gio::BusType::System).await else {
                return;
            };
            if let (Some(p), Ok(profiles)) = (p.upgrade(), power::profiles(&bus).await) {
                let name = power::MODES
                    .iter()
                    .find(|(id, ..)| *id == profiles.active)
                    .map_or(profiles.active.clone(), |(_, name, _)| name.to_string());
                *p.profile.borrow_mut() = Some(name);
            }
        });
    });
    root.connect_unmap(move |_| {
        if let Some(flag) = running.borrow_mut().take() {
            flag.store(false, Ordering::Relaxed);
        }
    });
    root
}

/// Lab only: steers the page from the file `NEXUS_MONITOR_LAB` names, one
/// command per change: `tab processes`, `pane gpu`, `filter text`, `tree`,
/// `select N` (the Nth row), `ask`, `end`, `force`.
#[cfg(feature = "lab")]
fn lab(p: &Page) {
    thread_local!(static LAST: RefCell<String> = RefCell::default());
    let Ok(path) = std::env::var("NEXUS_MONITOR_LAB") else {
        return;
    };
    let command = std::fs::read_to_string(path).unwrap_or_default();
    if LAST.with(|l| l.replace(command.clone())) == command {
        return;
    }
    let (verb, arg) = command
        .trim()
        .split_once(' ')
        .unwrap_or((command.trim(), ""));
    match verb {
        "tab" => p.tab_buttons[usize::from(arg == "processes")].set_active(true),
        "pane" => {
            if let Some(n) = p.navs.get(arg)
                && let Some(list) = n.row.parent().and_downcast::<gtk::ListBox>()
            {
                list.select_row(Some(&n.row));
            }
        }
        "filter" => p.search.set_text(arg),
        "tree" => p.tab_buttons[2].set_active(!p.tree.get()),
        "select" => {
            if let Some(r) = p.rows.get(arg.parse::<usize>().unwrap_or(0)) {
                p.list.select_row(Some(&r.row));
            }
        }
        "ask" => p.ask(),
        "end" => p.end(false),
        "force" => p.end(true),
        _ => {}
    }
    p.show();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sizes_read_naturally() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(43_008), "42 KiB");
        assert_eq!(size(3_565_158), "3.4 MiB");
        assert_eq!(size(16_535_624_704), "15.4 GiB");
        assert_eq!(duration(3 * 3600 + 7 * 60), "3h 07m");
        assert_eq!(duration(2 * 86400 + 5 * 3600), "2d 5h");
    }
    #[test]
    fn scales_land_on_round_numbers() {
        assert_eq!(nice(900., true), (1024., "1 KiB/s".into()));
        assert_eq!(
            nice(1.5 * 1_048_576., true),
            (2. * 1_048_576., "2 MiB/s".into())
        );
        assert_eq!(nice(300. * 1024., true), (500. * 1024., "500 KiB/s".into()));
        assert_eq!(nice(12.3, false), (20., "20".into()));
    }
}
