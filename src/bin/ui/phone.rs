//! The control center's Phone page. One header says where the link stands:
//! starting, broken (the problem and its way out), unpaired (the pairing code),
//! away (how to bring it back) or connected (battery, storage, memory, CPU,
//! then the way to its files, messages and notifications). Everything goes
//! through the daemon, which holds the connection (see `nexus_control::phone`).
use super::super::{
    button, caption, group, hbox, label, page_box, row as setting, scrolled, spinner, vbox,
};
use gtk::{gdk, glib, prelude::*};
use nexus_control::{
    Request,
    phone::{NotesSync, PhoneNotification, PhoneState},
    request,
};
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc};

/// Asks the daemon for `op`, then hands the phone's answer to `done`.
pub(super) fn call(op: &str, args: Value, done: impl FnOnce(Result<Value, String>) + 'static) {
    let req = Request::Phone {
        op: op.into(),
        args,
    };
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let r = request(&req).map_err(|e| e.to_string()).and_then(|r| {
            if r.ok {
                Ok(r.data.unwrap_or_default())
            } else {
                Err(r.error.unwrap_or_else(|| "The phone request failed".into()))
            }
        });
        let _ = tx.send_blocking(r);
    });
    glib::spawn_future_local(async move {
        if let Ok(r) = rx.recv().await {
            done(r);
        }
    });
}
/// Dark modules on a white quiet zone, 6 px each, as phone cameras read best.
fn qr_texture(text: &str) -> Option<gtk::gdk::MemoryTexture> {
    const SCALE: usize = 6;
    const QUIET: usize = 4;
    let code = qrcode::QrCode::new(text).ok()?;
    let n = code.width();
    let side = (n + 2 * QUIET) * SCALE;
    let mut pixels = vec![255u8; side * side * 3];
    for (i, color) in code.to_colors().iter().enumerate() {
        if *color == qrcode::Color::Dark {
            let (x, y) = ((i % n + QUIET) * SCALE, (i / n + QUIET) * SCALE);
            for row in y..y + SCALE {
                pixels[(row * side + x) * 3..(row * side + x + SCALE) * 3].fill(0);
            }
        }
    }
    Some(gtk::gdk::MemoryTexture::new(
        side as i32,
        side as i32,
        gtk::gdk::MemoryFormat::R8g8b8,
        &glib::Bytes::from_owned(pixels),
        side * 3,
    ))
}
fn size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.0} KB", bytes as f64 / 1024.),
        1_048_576..1_073_741_824 => format!("{:.1} MB", bytes as f64 / 1_048_576.),
        _ => format!("{:.1} GB", bytes as f64 / 1_073_741_824.),
    }
}
/// Newlines and runs of spaces as single spaces, so a row stays on one line.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
pub(super) fn row(icon: &str, title: &str, detail: &str) -> gtk::Box {
    let (title, detail) = (one_line(title), one_line(detail));
    let r = hbox(12);
    r.set_margin_top(8);
    r.set_margin_bottom(8);
    r.set_margin_start(10);
    r.set_margin_end(10);
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(18);
    image.add_css_class("row-icon");
    r.append(&image);
    let text = vbox(2);
    text.set_hexpand(true);
    let name = label(&title, "row-title");
    name.set_wrap(false);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    text.append(&name);
    if !detail.is_empty() {
        let d = caption(&detail);
        d.set_wrap(false);
        d.set_ellipsize(gtk::pango::EllipsizeMode::End);
        text.append(&d);
    }
    r.append(&text);
    r
}
pub(super) fn list() -> gtk::ListBox {
    let l = gtk::ListBox::new();
    l.add_css_class("group");
    l.add_css_class("phone-list");
    l.set_selection_mode(gtk::SelectionMode::None);
    l.set_activate_on_single_click(true);
    l
}
pub(super) fn clear_list(l: &gtk::ListBox) {
    while let Some(child) = l.first_child() {
        l.remove(&child);
    }
}

/// One figure of the phone: a title, a value, a bar and a detail line.
struct Stat {
    value: gtk::Label,
    bar: gtk::ProgressBar,
    detail: gtk::Label,
}
fn stat(grid: &gtk::Grid, title: &str, column: i32, row: i32) -> Stat {
    let card = vbox(6);
    card.add_css_class("card");
    card.add_css_class("phone-stat");
    card.set_hexpand(true);
    card.append(&caption(title));
    let value = label("—", "monitor-value");
    card.append(&value);
    let bar = gtk::ProgressBar::new();
    bar.add_css_class("monitor-bar");
    card.append(&bar);
    let detail = caption("");
    detail.set_wrap(false);
    detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
    // Its natural width would widen the window.
    detail.set_max_width_chars(12);
    detail.set_hexpand(true);
    card.append(&detail);
    grid.attach(&card, column, row, 1, 1);
    Stat { value, bar, detail }
}
impl Stat {
    fn set(&self, value: &str, fraction: f64, detail: &str) {
        self.value.set_text(value);
        self.bar.set_fraction(fraction.clamp(0., 1.));
        self.detail.set_text(detail);
    }
}
/// How long ago `ms`, milliseconds since the epoch, was: "now", "5 min", "3 h"…
pub(super) fn ago(ms: i64) -> String {
    let now = glib::real_time() / 1000;
    match (now - ms).max(0) / 60_000 {
        0 => "now".into(),
        m @ 1..60 => format!("{m} min"),
        m @ 60..1440 => format!("{} h", m / 60),
        m => format!("{} d", m / 1440),
    }
}
/// The whole of a notification in a popup at (`x`, `y`) on `r`, scrolling
/// when long.
fn reveal(r: &gtk::Widget, full: &str, text: &str, time: &str, x: f64, y: f64) {
    let body = vbox(8);
    body.set_margin_top(6);
    body.set_margin_bottom(6);
    body.set_margin_start(6);
    body.set_margin_end(6);
    // Wrapped text would otherwise shrink to a letter a line.
    body.set_width_request(380);
    let mut labels = Vec::new();
    for (t, class) in [(full, "row-title"), (text, "")] {
        let l = label(t, class);
        l.set_xalign(0.);
        l.set_wrap(true);
        l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        l.set_max_width_chars(48);
        l.set_selectable(true);
        body.append(&l);
        labels.push(l);
    }
    let when = caption(time);
    when.set_xalign(0.);
    body.append(&when);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(420)
        .child(&body)
        .build();
    let pop = gtk::Popover::builder().child(&scroll).build();
    pop.set_parent(r);
    pop.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    pop.connect_closed(|p| {
        let p = p.clone();
        glib::idle_add_local_once(move || p.unparent());
    });
    pop.popup();
    // Focus selects the whole title; leave the text unselected.
    glib::idle_add_local_once(move || labels.iter().for_each(|l| l.select_region(0, 0)));
}
fn uptime(seconds: u64) -> String {
    let (d, h, m) = (seconds / 86400, seconds / 3600 % 24, seconds / 60 % 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else {
        format!("{h}h {m:02}m")
    }
}

/// Where the link stands, from the daemon's state.
#[derive(Clone, Copy, PartialEq, Debug)]
enum View {
    /// Asking Tailscale for the address, binding the port.
    Starting,
    /// It cannot listen: the problem and its way out.
    Problem,
    /// No phone has connected yet: the pairing code.
    Unpaired,
    /// The phone that connected last is not here.
    Away,
    Linked,
}
fn view(s: &PhoneState) -> View {
    match s {
        PhoneState { name: Some(_), .. } => View::Linked,
        PhoneState { error: Some(_), .. } => View::Problem,
        PhoneState { pair_url: None, .. } => View::Starting,
        PhoneState { last: Some(_), .. } => View::Away,
        _ => View::Unpaired,
    }
}
/// "Problem. What to do." as its two parts.
fn split_problem(text: &str) -> (&str, &str) {
    match text.split_once(". ") {
        Some((problem, fix)) => (problem, fix),
        None => (text.trim_end_matches('.'), ""),
    }
}
/// An icon, a title and a line under it, on a card.
fn notice(icon: &str, class: &str) -> (gtk::Box, gtk::Label, gtk::Label) {
    let card = hbox(12);
    card.add_css_class("card");
    card.add_css_class("phone-notice");
    card.add_css_class(class);
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(18);
    image.set_valign(gtk::Align::Start);
    image.add_css_class("notice-icon");
    card.append(&image);
    let text = vbox(2);
    text.set_hexpand(true);
    let title = label("", "row-title");
    text.append(&title);
    let detail = label("", "muted");
    text.append(&detail);
    card.append(&text);
    (card, title, detail)
}
fn host() -> String {
    glib::host_name().to_string()
}

pub struct Page {
    pub widget: gtk::Box,
    avatar: gtk::Image,
    name: gtk::Label,
    dot: gtk::Box,
    status: gtk::Label,
    busy: gtk::Spinner,
    disconnect: gtk::Button,
    mirror: gtk::Button,
    pair_other: gtk::Button,
    problem: gtk::Box,
    problem_title: gtk::Label,
    problem_detail: gtk::Label,
    away: gtk::Box,
    away_detail: gtk::Label,
    pair: gtk::Box,
    qr: gtk::Picture,
    copy: gtk::Button,
    linked: gtk::Box,
    say: gtk::Label,
    /// Nexus Notes kept in step with the phone: on or off, and how it went.
    notes_sync: gtk::Switch,
    notes_line: gtk::Label,
    battery: Stat,
    storage: Stat,
    memory: Stat,
    cpu: Stat,
    state: RefCell<PhoneState>,
    shown: std::cell::Cell<Option<View>>,
    /// Closes the control center, once the files or messages open.
    leave: RefCell<Option<Box<dyn Fn()>>>,
    notifications: gtk::ListBox,
    /// The notifications' group, there while the phone has some.
    notes: gtk::Box,
    threads: gtk::ListBox,
    /// The recent conversations' group.
    talks: gtk::Box,
}
impl Page {
    pub fn new() -> Rc<Self> {
        let widget = vbox(0);
        let body = page_box();
        body.add_css_class("phone-page");
        widget.append(&scrolled(&body));

        // The header: whose link, and where it stands.
        let head = hbox(14);
        let avatar = gtk::Image::from_icon_name("phone-symbolic");
        avatar.set_pixel_size(22);
        avatar.add_css_class("phone-avatar");
        avatar.set_valign(gtk::Align::Center);
        head.append(&avatar);
        let titles = vbox(3);
        titles.set_hexpand(true);
        titles.set_valign(gtk::Align::Center);
        let name = label("Phone", "title");
        name.set_wrap(false);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        titles.append(&name);
        let line = hbox(7);
        let dot = vbox(0);
        dot.add_css_class("status-dot");
        dot.set_valign(gtk::Align::Center);
        line.append(&dot);
        let busy = spinner();
        busy.set_visible(false);
        line.append(&busy);
        let status = label("", "muted");
        status.set_wrap(false);
        status.set_ellipsize(gtk::pango::EllipsizeMode::End);
        status.set_max_width_chars(40);
        line.append(&status);
        titles.append(&line);
        head.append(&titles);
        let disconnect = button("Disconnect");
        disconnect.set_tooltip_text(Some("Tap this computer in the phone app to reconnect"));
        let pair_other = button("Pair a new phone");
        pair_other.add_css_class("flat");
        head.append(&pair_other);
        let mirror = button("Screen");
        mirror.set_tooltip_text(Some("See and use the phone's screen"));
        head.append(&mirror);
        head.append(&disconnect);
        body.append(&head);

        let (problem, problem_title, problem_detail) = notice("dialog-warning-symbolic", "problem");
        problem.set_margin_top(20);
        body.append(&problem);
        let (away, away_title, away_detail) = notice("view-refresh-symbolic", "away");
        away.set_margin_top(20);
        away_title.set_text("Reconnect from the phone");
        body.append(&away);

        // Pairing: the code to scan, the steps, and a link for those who can't.
        let pair = hbox(28);
        pair.add_css_class("card");
        pair.add_css_class("phone-pair");
        pair.set_margin_top(20);
        let frame = vbox(0);
        frame.add_css_class("qr-frame");
        frame.set_overflow(gtk::Overflow::Hidden);
        frame.set_valign(gtk::Align::Start);
        let qr = gtk::Picture::new();
        qr.set_can_shrink(false);
        qr.set_tooltip_text(Some("Scan with the phone's camera"));
        frame.append(&qr);
        pair.append(&frame);
        let steps = vbox(14);
        steps.set_hexpand(true);
        steps.set_valign(gtk::Align::Center);
        steps.append(&label("Pair your phone", "section-title"));
        for (n, text) in [
            "Turn on Tailscale on the phone, on the same tailnet as this computer.",
            "Scan this code with the phone's camera.",
            "Install Nexus if the page asks, then tap Pair and allow what the app lists.",
        ]
        .iter()
        .enumerate()
        {
            let step = hbox(10);
            let number = label(&(n + 1).to_string(), "step-number");
            number.set_xalign(0.5);
            number.set_valign(gtk::Align::Start);
            step.append(&number);
            step.append(&label(text, ""));
            steps.append(&step);
        }
        let manual = hbox(12);
        manual.add_css_class("pair-manual");
        let manual_text = caption(
            "Can't scan? Copy the pairing link, open it on the phone, or paste it in the app under Pair with a link.",
        );
        manual_text.set_hexpand(true);
        manual.append(&manual_text);
        let copy = button("Copy link");
        manual.append(&copy);
        steps.append(&manual);
        pair.append(&steps);
        body.append(&pair);

        // Connected: what the phone reports of itself, then what came to it.
        let linked = vbox(0);
        linked.add_css_class("monitor");
        let grid = gtk::Grid::new();
        grid.set_column_spacing(10);
        grid.set_column_homogeneous(true);
        grid.set_margin_top(22);
        let battery = stat(&grid, "Battery", 0, 0);
        let storage = stat(&grid, "Storage", 1, 0);
        let memory = stat(&grid, "Memory", 2, 0);
        let cpu = stat(&grid, "CPU", 3, 0);
        linked.append(&grid);
        let open = group(&linked, "On this computer");
        let mut opens = vec![];
        for (title, about) in [
            ("Files", "Browse the phone's storage, copy both ways"),
            ("Messages", "Read and send SMS"),
            ("Clipboard", "Shared both ways · history with Super+Shift+V"),
        ] {
            let r = setting(&open, title, about);
            let b = button("Open");
            r.append(&b);
            opens.push(b);
        }
        let r = setting(&open, "Notes", " ");
        let notes_line = r
            .first_child()
            .and_then(|t| t.last_child())
            .and_downcast::<gtk::Label>()
            .unwrap();
        let notes_sync = gtk::Switch::new();
        notes_sync.set_valign(gtk::Align::Center);
        notes_sync.set_tooltip_text(Some(
            "Keep Nexus Notes on the phone too, synced over this link only",
        ));
        r.append(&notes_sync);
        let say = label("", "error");
        say.add_css_class("banner");
        say.set_margin_top(12);
        say.set_visible(false);
        linked.append(&say);
        let section = |title: &str, list: &gtk::ListBox| {
            let b = vbox(0);
            b.set_visible(false);
            let rows = group(&b, title);
            rows.remove_css_class("group");
            rows.append(list);
            linked.append(&b);
            b
        };
        let notifications = list();
        let notes = section("Notifications", &notifications);
        let threads = list();
        let talks = section("Recent conversations", &threads);
        body.append(&linked);

        let page = Rc::new(Self {
            widget,
            avatar,
            name,
            dot,
            status,
            busy,
            disconnect,
            mirror,
            pair_other,
            problem,
            problem_title,
            problem_detail,
            away,
            away_detail,
            pair,
            qr,
            copy,
            linked,
            say,
            notes_sync,
            notes_line,
            battery,
            storage,
            memory,
            cpu,
            state: RefCell::default(),
            shown: Default::default(),
            leave: RefCell::default(),
            notifications,
            notes,
            threads,
            talks,
        });

        let screen = Rc::downgrade(&page);
        page.mirror.connect_clicked(move |_| {
            if let Some(p) = screen.upgrade() {
                p.report(spawn_sibling("nexus", &["phone", "mirror"]));
            }
        });
        let p = page.clone();
        page.disconnect.connect_clicked(move |_| {
            let p = p.clone();
            call("disconnect", Value::Null, move |r| p.report(r.map(|_| ())));
        });
        let p = Rc::downgrade(&page);
        page.pair_other.connect_clicked(move |_| {
            if let Some(p) = p.upgrade() {
                p.pair.set_visible(!p.pair.is_visible());
            }
        });
        let p = Rc::downgrade(&page);
        page.copy.connect_clicked(move |b| {
            let Some(p) = p.upgrade() else { return };
            if let Some(url) = &p.state.borrow().pair_url {
                b.clipboard().set_text(url);
                b.set_label("Copied");
                let b = b.downgrade();
                glib::timeout_add_seconds_local_once(2, move || {
                    if let Some(b) = b.upgrade() {
                        b.set_label("Copy link");
                    }
                });
            }
        });
        for (b, (bin, arg)) in opens.iter().zip([
            ("nexus-files", "--phone"),
            ("nexus", "messages"),
            ("nexus", "clipboard"),
        ]) {
            let p = page.clone();
            b.connect_clicked(move |_| p.open(bin, arg));
        }
        let p = Rc::downgrade(&page);
        page.notes_sync.connect_active_notify(move |sw| {
            let Some(p) = p.upgrade() else { return };
            // Not what the daemon said: the user's own change.
            if sw.is_active() != p.state.borrow().notes.on {
                let p2 = p.clone();
                call("notes_sync", json!({ "on": sw.is_active() }), move |r| {
                    p2.report(r.map(|_| ()))
                });
            }
        });
        let p = page.clone();
        page.threads
            .connect_row_activated(move |_, _| p.open("nexus", "messages"));
        // Conversations change unseen: look again each time the page shows.
        let p = Rc::downgrade(&page);
        page.widget.connect_map(move |_| {
            if let Some(p) = p.upgrade().filter(|p| p.state.borrow().name.is_some()) {
                p.load_threads();
            }
        });
        page.show(View::Starting, &PhoneState::default());
        page
    }

    /// The header and which blocks show, for `v`.
    fn show(&self, v: View, s: &PhoneState) {
        let (dot, status) = match v {
            View::Starting => ("", "Connecting to Tailscale…".to_owned()),
            View::Problem => ("bad", "Unavailable".to_owned()),
            View::Unpaired => ("off", "Not paired".to_owned()),
            View::Away if s.released => ("off", "Disconnected".to_owned()),
            View::Away => (
                "off",
                match s.seen.map(ago).as_deref() {
                    Some("now") => "Not connected · last seen just now".to_owned(),
                    Some(t) => format!("Not connected · last seen {t} ago"),
                    None => "Not connected".to_owned(),
                },
            ),
            View::Linked => ("on", self.summary(s)),
        };
        for c in ["on", "off", "bad"] {
            self.dot.remove_css_class(c);
        }
        if !dot.is_empty() {
            self.dot.add_css_class(dot);
        }
        self.dot.set_visible(v != View::Starting);
        self.busy.set_visible(v == View::Starting);
        self.status.set_text(&status);
        if v == View::Linked {
            self.avatar.add_css_class("on");
        } else {
            self.avatar.remove_css_class("on");
        }
        let name = s.name.as_ref().or(s.last.as_ref());
        let model = s.info.as_ref().and_then(|i| i["model"].as_str());
        self.name
            .set_text(model.or(name.map(String::as_str)).unwrap_or("Phone"));
        self.disconnect.set_visible(v == View::Linked);
        self.mirror.set_visible(v == View::Linked);
        self.pair_other.set_visible(v == View::Away);
        self.problem.set_visible(v == View::Problem);
        self.away.set_visible(v == View::Away);
        self.linked.set_visible(v == View::Linked);
        if self.shown.replace(Some(v)) != Some(v) {
            // Away, the code waits behind its button.
            self.pair.set_visible(v == View::Unpaired);
        }
        if v != View::Unpaired && v != View::Away {
            self.pair.set_visible(false);
        }
        if let Some(e) = &s.error {
            let (problem, fix) = split_problem(e);
            self.problem_title.set_text(problem);
            self.problem_detail.set_text(fix);
            self.problem_detail.set_visible(!fix.is_empty());
        }
        self.away_detail.set_text(&if s.released {
            format!("Open Nexus on the phone and tap {}.", host())
        } else {
            format!(
                "It comes back by itself once the phone is on Tailscale. If not, open Nexus on the phone and tap {}.",
                host()
            )
        });
    }
    /// "Connected · Wi-Fi · Android 15 · up 3h 02m".
    fn summary(&self, s: &PhoneState) -> String {
        let i = s.info.clone().unwrap_or_default();
        let mut parts = vec!["Connected".to_owned()];
        if let Some(n) = i["network"].as_str() {
            parts.push(n.to_owned());
        }
        if let Some(v) = i["android"].as_str() {
            parts.push(format!("Android {v}"));
        }
        if let Some(u) = i["uptime"].as_u64() {
            parts.push(format!("up {}", uptime(u)));
        }
        parts.join(" · ")
    }

    pub fn update(self: &Rc<Self>, s: &PhoneState) {
        if *self.state.borrow() == *s {
            return;
        }
        let was = self.state.replace(s.clone());
        if was.pair_url != s.pair_url {
            let texture = s.pair_url.as_deref().and_then(qr_texture);
            self.qr.set_paintable(texture.as_ref());
        }
        let v = view(s);
        self.show(v, s);
        if v != View::Linked {
            return;
        }
        if was.name.is_none() {
            self.say("");
            self.load_threads();
        }
        if was.notifications != s.notifications {
            self.show_notifications(&s.notifications);
        }
        if self.notes_sync.is_active() != s.notes.on {
            self.notes_sync.set_active(s.notes.on);
        }
        self.notes_line.set_text(&notes_line(&s.notes));
        self.notes_line.set_tooltip_text(
            Some(s.notes.conflicts.join("\n").as_str()).filter(|t| !t.is_empty()),
        );
        if s.notes.on && s.notes.error.is_some() {
            self.notes_line.add_css_class("error");
        } else {
            self.notes_line.remove_css_class("error");
        }
        let i = s.info.clone().unwrap_or_default();
        let b = &i["battery"];
        let level = b["level"].as_f64().unwrap_or(0.);
        if level <= 15. && b["charging"] != true {
            self.battery.bar.add_css_class("low");
        } else {
            self.battery.bar.remove_css_class("low");
        }
        let charging = if b["charging"] == true {
            "Charging"
        } else {
            "On battery"
        };
        self.battery.set(
            &format!("{level:.0}%"),
            level / 100.,
            &format!("{charging} · {:.0} °C", b["temp"].as_f64().unwrap_or(0.)),
        );
        let used = |o: &Value| {
            let total = o["total"].as_u64().unwrap_or(0);
            let free = o["free"].as_u64().unwrap_or(0);
            (total.saturating_sub(free), total)
        };
        let (u, t) = used(&i["storage"]);
        self.storage.set(
            &size(u),
            u as f64 / t.max(1) as f64,
            &format!("of {} · {} free", size(t), size(t - u)),
        );
        let (u, t) = used(&i["ram"]);
        self.memory.set(
            &size(u),
            u as f64 / t.max(1) as f64,
            &format!("of {}", size(t)),
        );
        let c = &i["cpu"];
        let (mhz, max) = (
            c["mhz"].as_u64().unwrap_or(0),
            c["max_mhz"].as_u64().unwrap_or(0),
        );
        let soc = c["soc"].as_str().unwrap_or("");
        let cores = format!("{} cores", c["cores"].as_u64().unwrap_or(0));
        // Some phones keep their CPU frequencies to themselves.
        let (value, detail) = if max > 0 {
            (format!("{mhz} MHz"), format!("{cores} · up to {max} MHz"))
        } else {
            (cores, String::new())
        };
        self.cpu.set(
            &value,
            mhz as f64 / max.max(1) as f64,
            &[detail.as_str(), soc]
                .iter()
                .filter(|s| !s.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(" · "),
        );
    }
    /// The latest conversations; a click opens Messages.
    fn load_threads(self: &Rc<Self>) {
        let p = self.clone();
        call("sms_threads", Value::Null, move |r| {
            let Ok(data) = r else { return };
            clear_list(&p.threads);
            let threads = data["threads"].as_array().cloned().unwrap_or_default();
            p.talks.set_visible(!threads.is_empty());
            for t in threads.iter().take(5) {
                let text = |k: &str| t[k].as_str().unwrap_or_default();
                let title = if text("name").is_empty() {
                    text("address")
                } else {
                    text("name")
                };
                let r = row("mail-unread-symbolic", title, text("snippet"));
                if let Some(date) = t["date"].as_i64().filter(|d| *d > 0) {
                    let when = caption(&ago(date));
                    when.set_wrap(false);
                    when.set_valign(gtk::Align::Center);
                    r.append(&when);
                }
                p.threads.append(&r);
            }
        });
    }
    /// One row per notification: its app's icon, title and text, and a
    /// button that dismisses it on the phone.
    fn show_notifications(self: &Rc<Self>, list: &[PhoneNotification]) {
        clear_list(&self.notifications);
        self.notes.set_visible(!list.is_empty());
        for n in list {
            let title = match (n.app.as_str(), n.title.as_str()) {
                (app, "") => app.to_owned(),
                (app, title) if app.is_empty() || app == title => title.to_owned(),
                (app, title) => format!("{title} · {app}"),
            };
            let r = row(
                if n.icon.is_empty() || n.icon.starts_with('/') {
                    "phone-symbolic"
                } else {
                    &n.icon
                },
                &title,
                &n.text,
            );
            if n.icon.starts_with('/')
                && let Some(image) = r.first_child().and_downcast::<gtk::Image>()
            {
                image.set_from_file(Some(&n.icon));
            }
            let when = caption(&ago(n.time));
            when.set_wrap(false);
            when.set_valign(gtk::Align::Center);
            r.append(&when);
            let dismiss = gtk::Button::from_icon_name("window-close-symbolic");
            dismiss.add_css_class("flat");
            dismiss.add_css_class("dismiss");
            dismiss.set_valign(gtk::Align::Center);
            dismiss.set_tooltip_text(Some("Dismiss on the phone"));
            let (p, key) = (self.clone(), n.key.clone());
            dismiss.connect_clicked(move |_| {
                let p = p.clone();
                call("dismiss", json!({ "key": key }), move |r| {
                    p.report(r.map(|_| ()))
                });
            });
            r.append(&dismiss);
            // Right click: the whole notification.
            let click = gtk::GestureClick::builder().button(3).build();
            let (full, text, time) = (title.clone(), n.text.clone(), ago(n.time));
            click.connect_pressed(move |g, _, x, y| {
                if let Some(r) = g.widget() {
                    reveal(&r, &full, &text, &time, x, y);
                }
            });
            r.add_controller(click);
            self.notifications.append(&r);
        }
    }
    pub fn on_leave(&self, f: impl Fn() + 'static) {
        self.leave.replace(Some(Box::new(f)));
    }
    /// Starts our sibling binary `bin`, then gets out of its way.
    fn open(&self, bin: &str, arg: &str) {
        match spawn_sibling(bin, &[arg]) {
            Ok(()) => {
                self.say("");
                if let Some(leave) = &*self.leave.borrow() {
                    leave();
                }
            }
            Err(e) => self.say(&e),
        }
    }
    fn say(&self, text: &str) {
        self.say.set_text(text);
        self.say.set_visible(!text.is_empty());
    }
    fn report(&self, r: Result<(), String>) {
        match r {
            Ok(()) => self.say(""),
            Err(e) => self.say(&e),
        }
    }
}
/// "Synced 2 min ago · 1 conflict: Plan (changed on phone …)".
fn notes_line(n: &NotesSync) -> String {
    if !n.on {
        return "Off · each device keeps its own notes".into();
    }
    if n.busy {
        return "Syncing…".into();
    }
    let mut parts = vec![match (&n.error, n.last.map(ago).as_deref()) {
        (Some(e), _) => e.clone(),
        (None, Some("now")) => "Synced just now".into(),
        (None, Some(t)) => format!("Synced {t} ago"),
        (None, None) => "Syncs with the phone over this link only".into(),
    }];
    match &n.conflicts[..] {
        [] => {}
        [one] => parts.push(format!(
            "Kept both versions: {}",
            one.rsplit('/')
                .next()
                .unwrap_or(one)
                .trim_end_matches(".md")
        )),
        many => parts.push(format!("{} pages kept in both versions", many.len())),
    }
    parts.join(" · ")
}
/// Starts our sibling binary `bin` with `arg`, not waiting for it.
pub(super) fn spawn_sibling(bin: &str, args: &[&str]) -> Result<(), String> {
    let exe = std::env::current_exe()
        .map(|e| e.with_file_name(bin))
        .unwrap_or_else(|_| bin.into());
    match std::process::Command::new(exe).args(args).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
            Ok(())
        }
        Err(e) => Err(format!("Could not open {bin}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn views() {
        let mut s = PhoneState::default();
        assert_eq!(view(&s), View::Starting);
        s.pair_url = Some("http://x".into());
        assert_eq!(view(&s), View::Unpaired);
        s.last = Some("Pixel".into());
        assert_eq!(view(&s), View::Away);
        s.error = Some("Tailscale is not connected. Run it.".into());
        assert_eq!(view(&s), View::Problem);
        s.name = Some("Pixel".into());
        assert_eq!(view(&s), View::Linked);
        assert_eq!(
            split_problem("A is off. Turn it on."),
            ("A is off", "Turn it on.")
        );
        assert_eq!(split_problem("Broken."), ("Broken", ""));
        let mut n = NotesSync::default();
        assert!(notes_line(&n).starts_with("Off"));
        n.on = true;
        n.last = Some(glib::real_time() / 1000);
        n.conflicts = vec!["A/Plan (changed on phone 2026-11-07 10.00).md".into()];
        assert_eq!(
            notes_line(&n),
            "Synced just now · Kept both versions: Plan (changed on phone 2026-11-07 10.00)"
        );
    }
}
