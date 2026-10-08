//! The control center's Monitor page: CPU, memory, network, disk and
//! processes, in the manner of btop. A thread samples once a second only
//! while the page shows; hidden, it costs nothing but its widgets.
use super::super::{hbox, vbox};
use gtk::{glib, prelude::*};
use nexus_control::monitor::{Proc, Sample, Sampler};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// Seconds of history the graphs show.
const HISTORY: usize = 120;
/// Process rows built once and refilled; the rest are a search away.
const ROWS: usize = 80;

fn text(content: &str, class: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(content));
    l.set_xalign(0.);
    if !class.is_empty() {
        l.add_css_class(class);
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
fn duration(seconds: u64) -> String {
    let (d, h, m) = (seconds / 86400, seconds / 3600 % 24, seconds / 60 % 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else {
        format!("{h}h {m:02}m")
    }
}

/// A scrolling history: one series rising from the bottom, or two mirrored
/// around the middle (in above, out below). Fixed to 0..1, or scaled to its
/// largest value.
#[derive(Default)]
struct History {
    up: VecDeque<f32>,
    down: VecDeque<f32>,
}
fn push(series: &mut VecDeque<f32>, value: f32) {
    if series.len() == HISTORY {
        series.pop_front();
    }
    series.push_back(value);
}
fn graph(class: &str, mirrored: bool, fixed: bool) -> (gtk::DrawingArea, Rc<RefCell<History>>) {
    let area = gtk::DrawingArea::new();
    area.add_css_class("monitor-graph");
    area.add_css_class(class);
    area.set_content_height(72);
    area.set_hexpand(true);
    // Graphs take what their card has to spare.
    area.set_vexpand(true);
    let history = Rc::new(RefCell::new(History::default()));
    let h = history.clone();
    area.set_draw_func(move |area, cr, w, height| {
        let history = h.borrow();
        let (w, height) = (f64::from(w), f64::from(height));
        let c = area.color();
        let (r, g, b) = (
            f64::from(c.red()),
            f64::from(c.green()),
            f64::from(c.blue()),
        );
        let max = if fixed {
            1.
        } else {
            history
                .up
                .iter()
                .chain(&history.down)
                .fold(1f32, |m, v| m.max(*v))
        };
        let step = w / (HISTORY - 1) as f64;
        let base = if mirrored { height / 2. } else { height };
        let span = if mirrored {
            height / 2. - 1.
        } else {
            height - 1.
        };
        for (series, sign) in [(&history.up, -1.), (&history.down, 1.)] {
            if series.is_empty() || (!mirrored && sign > 0.) {
                continue;
            }
            let x0 = w - (series.len() - 1) as f64 * step;
            let y = |v: f32| base + sign * f64::from(v / max) * span;
            cr.move_to(x0, base);
            for (i, v) in series.iter().enumerate() {
                cr.line_to(x0 + i as f64 * step, y(*v));
            }
            cr.line_to(w, base);
            cr.close_path();
            cr.set_source_rgba(r, g, b, if sign < 0. { 0.22 } else { 0.12 });
            let _ = cr.fill();
            for (i, v) in series.iter().enumerate() {
                cr.line_to(x0 + i as f64 * step, y(*v));
            }
            cr.set_source_rgba(r, g, b, if sign < 0. { 1. } else { 0.6 });
            cr.set_line_width(1.5);
            let _ = cr.stroke();
        }
        if mirrored {
            cr.set_source_rgba(r, g, b, 0.15);
            cr.rectangle(0., base - 0.5, w, 1.);
            let _ = cr.fill();
        }
    });
    (area, history)
}
/// One bar per core, side by side.
fn cores() -> (gtk::DrawingArea, Rc<RefCell<Vec<f32>>>) {
    let area = gtk::DrawingArea::new();
    area.add_css_class("monitor-graph");
    area.add_css_class("cpu");
    area.set_content_height(34);
    let values = Rc::new(RefCell::new(Vec::<f32>::new()));
    let v = values.clone();
    area.set_draw_func(move |area, cr, w, h| {
        let values = v.borrow();
        if values.is_empty() {
            return;
        }
        let c = area.color();
        let (r, g, b) = (
            f64::from(c.red()),
            f64::from(c.green()),
            f64::from(c.blue()),
        );
        let (w, h) = (f64::from(w), f64::from(h));
        let gap = 3.;
        let n = values.len() as f64;
        let bar = ((w - gap * (n - 1.)) / n).max(1.);
        for (i, value) in values.iter().enumerate() {
            let x = i as f64 * (bar + gap);
            cr.set_source_rgba(r, g, b, 0.14);
            cr.rectangle(x, 0., bar, h);
            let _ = cr.fill();
            let filled = h * f64::from(value.clamp(0., 1.));
            cr.set_source_rgba(r, g, b, 0.9);
            cr.rectangle(x, h - filled, bar, filled);
            let _ = cr.fill();
        }
    });
    (area, values)
}
/// A titled card: title, headline value, detail line, then `body`.
fn card(title: &str, body: &[&gtk::Widget]) -> (gtk::Box, gtk::Label, gtk::Label) {
    let card = vbox(8);
    card.add_css_class("card");
    card.set_hexpand(true);
    card.set_vexpand(true);
    card.append(&text(title, "eyebrow"));
    let value = text("—", "monitor-value");
    card.append(&value);
    for widget in body {
        card.append(*widget);
    }
    let detail = text("", "muted");
    detail.add_css_class("caption");
    detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
    card.append(&detail);
    (card, value, detail)
}

#[derive(Clone, Copy, PartialEq)]
enum Sort {
    Pid,
    Name,
    Cpu,
    Memory,
    Threads,
}
struct Row {
    row: gtk::ListBoxRow,
    pid: gtk::Label,
    name: gtk::Label,
    cpu: gtk::Label,
    memory: gtk::Label,
    threads: gtk::Label,
    shows: Cell<i32>,
}
fn columns() -> [gtk::Label; 5] {
    let cell = |chars: i32, xalign: f32| {
        let l = text("", "");
        l.set_width_chars(chars);
        l.set_xalign(xalign);
        l
    };
    let name = cell(0, 0.);
    name.set_hexpand(true);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    [cell(8, 1.), name, cell(7, 1.), cell(9, 1.), cell(7, 1.)]
}
struct State {
    sample: RefCell<Sample>,
    sort: Cell<(Sort, bool)>,
    filter: RefCell<String>,
    selected: Cell<Option<i32>>,
    rows: Vec<Row>,
    list: gtk::ListBox,
    headers: Vec<(Sort, gtk::Button, &'static str)>,
}
impl State {
    fn procs(&self) {
        let sample = self.sample.borrow();
        let filter = self.filter.borrow().to_lowercase();
        let mut procs: Vec<&Proc> = sample
            .procs
            .iter()
            .filter(|p| {
                filter.is_empty()
                    || p.name.to_lowercase().contains(&filter)
                    || p.pid.to_string().starts_with(&filter)
            })
            .collect();
        let (sort, descending) = self.sort.get();
        procs.sort_unstable_by(|a, b| {
            let order = match sort {
                Sort::Pid => a.pid.cmp(&b.pid),
                Sort::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                Sort::Cpu => a.cpu.total_cmp(&b.cpu),
                Sort::Memory => a.memory.cmp(&b.memory),
                Sort::Threads => a.threads.cmp(&b.threads),
            };
            let order = if descending { order.reverse() } else { order };
            order.then(a.pid.cmp(&b.pid))
        });
        let selected = self.selected.get();
        let mut still = None;
        for (i, row) in self.rows.iter().enumerate() {
            let Some(p) = procs.get(i) else {
                row.row.set_visible(false);
                continue;
            };
            row.row.set_visible(true);
            row.shows.set(p.pid);
            row.pid.set_text(&p.pid.to_string());
            row.name.set_text(&p.name);
            row.cpu.set_text(&format!("{:.1}%", p.cpu * 100.));
            row.memory.set_text(&size(p.memory));
            row.threads.set_text(&p.threads.to_string());
            if Some(p.pid) == selected {
                still = Some(&row.row);
            }
        }
        // Selection follows the process, wherever sorting moves it.
        match still {
            Some(row) => self.list.select_row(Some(row)),
            None => self.list.unselect_all(),
        }
        self.selected.set(selected.filter(|_| still.is_some()));
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
    fn end(&self, force: bool) {
        if let Some(pid) = self.selected.get() {
            // SAFETY: kill only sends a signal.
            unsafe {
                libc::kill(pid, if force { libc::SIGKILL } else { libc::SIGTERM });
            }
        }
    }
}

pub fn page() -> gtk::Box {
    let root = vbox(14);
    root.add_css_class("monitor");
    root.add_css_class("monitor-page");

    let (cpu_graph, cpu_history) = graph("cpu", false, true);
    let (core_bars, core_values) = cores();
    let (cpu_card, cpu_value, cpu_detail) =
        card("CPU", &[cpu_graph.upcast_ref(), core_bars.upcast_ref()]);
    let (memory_graph, memory_history) = graph("memory", false, true);
    let (memory_card, memory_value, memory_detail) = card("MEMORY", &[memory_graph.upcast_ref()]);
    let (net_graph, net_history) = graph("network", true, false);
    let (net_card, net_value, net_detail) = card("NETWORK", &[net_graph.upcast_ref()]);
    let disk_bar = gtk::ProgressBar::new();
    disk_bar.add_css_class("monitor-bar");
    let (disk_graph, disk_history) = graph("disk", true, false);
    let (disk_card, disk_value, disk_detail) =
        card("DISK", &[disk_bar.upcast_ref(), disk_graph.upcast_ref()]);
    let cards = gtk::Grid::new();
    cards.set_row_spacing(14);
    cards.set_column_spacing(14);
    cards.set_column_homogeneous(true);
    cards.set_row_homogeneous(true);
    for (i, c) in [&cpu_card, &memory_card, &net_card, &disk_card]
        .iter()
        .enumerate()
    {
        cards.attach(*c, i as i32 % 2, i as i32 / 2, 1, 1);
    }

    let procs = vbox(10);
    procs.add_css_class("card");
    procs.set_vexpand(true);
    let head = hbox(10);
    let search = gtk::SearchEntry::new();
    search.add_css_class("monitor-search");
    search.set_placeholder_text(Some("Filter by name or PID"));
    search.set_hexpand(true);
    head.append(&search);
    let end = gtk::Button::with_label("End process");
    end.add_css_class("monitor-end");
    end.add_css_class("destructive-action");
    end.set_sensitive(false);
    end.set_tooltip_text(Some("Delete; Shift+Delete forces it"));
    head.append(&end);
    procs.append(&head);
    let header = hbox(12);
    header.add_css_class("monitor-columns");
    let [h_pid, h_name, h_cpu, h_memory, h_threads] = columns();
    let mut headers = vec![];
    for (label, sort, title) in [
        (&h_pid, Sort::Pid, "PID"),
        (&h_name, Sort::Name, "Name"),
        (&h_cpu, Sort::Cpu, "CPU"),
        (&h_memory, Sort::Memory, "Memory"),
        (&h_threads, Sort::Threads, "Threads"),
    ] {
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
    // Room on the right for the scrollbar, which would cover the last column.
    header.set_margin_end(14);
    procs.append(&header);
    let list = gtk::ListBox::new();
    list.add_css_class("monitor-list");
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.set_margin_end(14);
    let rows: Vec<Row> = (0..ROWS)
        .map(|_| {
            let [pid, name, cpu, memory, threads] = columns();
            let line = hbox(12);
            for l in [&pid, &name, &cpu, &memory, &threads] {
                line.append(l);
            }
            let row = gtk::ListBoxRow::new();
            row.set_child(Some(&line));
            row.set_visible(false);
            list.append(&row);
            Row {
                row,
                pid,
                name,
                cpu,
                memory,
                threads,
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
    // Resources and processes each take the whole page, one at a time.
    let tabs = gtk::Stack::new();
    tabs.set_transition_type(gtk::StackTransitionType::Crossfade);
    tabs.set_transition_duration(150);
    tabs.set_vexpand(true);
    tabs.add_named(&cards, Some("resources"));
    tabs.add_named(&procs, Some("processes"));
    let switch = hbox(0);
    switch.add_css_class("segmented");
    switch.set_halign(gtk::Align::Start);
    let resources = gtk::ToggleButton::with_label("Resources");
    let processes = gtk::ToggleButton::with_label("Processes");
    processes.set_group(Some(&resources));
    resources.set_active(true);
    for (button, name) in [(&resources, "resources"), (&processes, "processes")] {
        let tabs = tabs.clone();
        button.connect_toggled(move |b| {
            if b.is_active() {
                tabs.set_visible_child_name(name);
            }
        });
        switch.append(button);
    }
    root.append(&switch);
    root.append(&tabs);

    let state = Rc::new(State {
        sample: RefCell::default(),
        sort: Cell::new((Sort::Cpu, true)),
        filter: RefCell::default(),
        selected: Cell::new(None),
        rows,
        list: list.clone(),
        headers,
    });
    for (sort, button, _) in &state.headers {
        let (s, sort) = (Rc::downgrade(&state), *sort);
        button.connect_clicked(move |_| {
            let Some(s) = s.upgrade() else { return };
            let (current, descending) = s.sort.get();
            // Numbers start largest first, names from A.
            s.sort.set(if current == sort {
                (sort, !descending)
            } else {
                (sort, !matches!(sort, Sort::Name | Sort::Pid))
            });
            s.procs();
        });
    }
    let s = Rc::downgrade(&state);
    search.connect_search_changed(move |search| {
        if let Some(s) = s.upgrade() {
            *s.filter.borrow_mut() = search.text().trim().to_string();
            s.procs();
        }
    });
    let (s, end2) = (Rc::downgrade(&state), end.clone());
    list.connect_row_selected(move |_, row| {
        let Some(s) = s.upgrade() else { return };
        let pid = row.and_then(|row| s.rows.iter().find(|r| &r.row == row).map(|r| r.shows.get()));
        s.selected.set(pid);
        end2.set_sensitive(pid.is_some());
    });
    let s = Rc::downgrade(&state);
    end.connect_clicked(move |_| {
        if let Some(s) = s.upgrade() {
            s.end(false);
        }
    });
    let key = gtk::EventControllerKey::new();
    let (s, search2) = (Rc::downgrade(&state), search.clone());
    key.connect_key_pressed(move |_, key, _, modifiers| {
        match key {
            // Otherwise Escape reaches the window, which closes.
            gtk::gdk::Key::Escape if !search2.text().is_empty() => search2.set_text(""),
            gtk::gdk::Key::Delete if !search2.has_focus() => {
                if let Some(s) = s.upgrade() {
                    s.end(modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK));
                }
            }
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    root.add_controller(key);

    // The handlers hold the state weakly; this keeps it for the page's life.
    let s = state;
    let update: Rc<dyn Fn(Sample)> = Rc::new(move |sample: Sample| {
        let pct = |v: f32| format!("{:.0}%", v * 100.);
        cpu_value.set_text(&match sample.temperature {
            Some(t) => format!("{} · {t:.0}°C", pct(sample.cpu)),
            None => pct(sample.cpu),
        });
        cpu_detail.set_text(&format!(
            "{} cores · load {:.2} {:.2} {:.2} · up {}",
            sample.cores.len(),
            sample.load[0],
            sample.load[1],
            sample.load[2],
            duration(sample.uptime)
        ));
        push(&mut cpu_history.borrow_mut().up, sample.cpu);
        *core_values.borrow_mut() = sample.cores.clone();
        let used = sample.memory_used as f32 / sample.memory_total.max(1) as f32;
        memory_value.set_text(&format!(
            "{} / {}",
            size(sample.memory_used),
            size(sample.memory_total)
        ));
        memory_detail.set_text(&format!(
            "{} · cache {} · swap {} / {}",
            pct(used),
            size(sample.memory_cached),
            size(sample.swap_used),
            size(sample.swap_total)
        ));
        push(&mut memory_history.borrow_mut().up, used);
        net_value.set_text(&format!(
            "↓ {}/s  ↑ {}/s",
            size(sample.received),
            size(sample.sent)
        ));
        net_detail.set_text("Received above, sent below");
        {
            let mut h = net_history.borrow_mut();
            push(&mut h.up, sample.received as f32);
            push(&mut h.down, sample.sent as f32);
        }
        disk_value.set_text(&format!(
            "{} / {}",
            size(sample.disk_used),
            size(sample.disk_total)
        ));
        disk_bar.set_fraction(sample.disk_used as f64 / sample.disk_total.max(1) as f64);
        disk_detail.set_text(&format!(
            "read {}/s · write {}/s",
            size(sample.read),
            size(sample.written)
        ));
        {
            let mut h = disk_history.borrow_mut();
            push(&mut h.up, sample.read as f32);
            push(&mut h.down, sample.written as f32);
        }
        for area in [
            &cpu_graph,
            &core_bars,
            &memory_graph,
            &net_graph,
            &disk_graph,
        ] {
            area.queue_draw();
        }
        *s.sample.borrow_mut() = sample;
        s.procs();
    });
    // The page is mapped while it is the one shown in an open window: sample
    // then, and stop the thread as soon as it is not.
    let running = Rc::new(RefCell::new(None::<Arc<AtomicBool>>));
    let r = running.clone();
    root.connect_map(move |_| {
        if r.borrow().is_some() {
            return;
        }
        let flag = Arc::new(AtomicBool::new(true));
        *r.borrow_mut() = Some(flag.clone());
        let (tx, rx) = async_channel::bounded::<Sample>(1);
        std::thread::spawn(move || {
            let mut sampler = Sampler::default();
            sampler.sample();
            // A first reading soon, then once a second.
            let mut wait = Duration::from_millis(300);
            loop {
                std::thread::sleep(wait);
                wait = Duration::from_secs(1);
                if !flag.load(Ordering::Relaxed) || tx.send_blocking(sampler.sample()).is_err() {
                    break;
                }
            }
        });
        let update = update.clone();
        glib::spawn_future_local(async move {
            while let Ok(sample) = rx.recv().await {
                update(sample);
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
}
