//! The control center's Phone page: the pairing QR code until a phone
//! connects, then its files and messages. Everything goes through the daemon,
//! which holds the connection (see `nexus_control::phone`).
use super::super::{button, caption, clear, hbox, label, page_box, scrolled, vbox};
use gtk::{glib, prelude::*};
use nexus_control::{Request, phone::PhoneState, request};
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc};

/// Asks the daemon for `op`, then hands the phone's answer to `done`.
fn call(op: &str, args: Value, done: impl FnOnce(Result<Value, String>) + 'static) {
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
fn row(icon: &str, title: &str, detail: &str) -> gtk::Box {
    let (title, detail) = (one_line(title), one_line(detail));
    let r = hbox(12);
    r.set_margin_top(8);
    r.set_margin_bottom(8);
    r.set_margin_start(10);
    r.set_margin_end(10);
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(18);
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
fn list() -> gtk::ListBox {
    let l = gtk::ListBox::new();
    l.add_css_class("card");
    l.set_selection_mode(gtk::SelectionMode::None);
    l.set_activate_on_single_click(true);
    l
}
fn clear_list(l: &gtk::ListBox) {
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
    let card = vbox(8);
    card.add_css_class("card");
    card.set_hexpand(true);
    card.append(&label(title, "eyebrow"));
    let value = label("—", "monitor-value");
    card.append(&value);
    let bar = gtk::ProgressBar::new();
    bar.add_css_class("monitor-bar");
    card.append(&bar);
    let detail = caption("");
    detail.set_wrap(false);
    detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
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
fn uptime(seconds: u64) -> String {
    let (d, h, m) = (seconds / 86400, seconds / 3600 % 24, seconds / 60 % 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else {
        format!("{h}h {m:02}m")
    }
}

pub struct Page {
    pub widget: gtk::Box,
    stack: gtk::Stack,
    qr: gtk::Picture,
    url: gtk::Label,
    error: gtk::Label,
    name: gtk::Label,
    summary: gtk::Label,
    status: gtk::Label,
    battery: Stat,
    storage: Stat,
    memory: Stat,
    cpu: Stat,
    state: RefCell<PhoneState>,
    threads: gtk::ListBox,
    thread_ids: RefCell<Vec<(i64, String, String)>>,
    messages: gtk::Box,
    messages_scroll: gtk::ScrolledWindow,
    to: gtk::Entry,
    with: gtk::Label,
    current: RefCell<Option<(i64, String)>>,
}
impl Page {
    pub fn new() -> Rc<Self> {
        let widget = page_box();
        let stack = gtk::Stack::new();
        stack.set_vexpand(true);
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        widget.append(&stack);

        // Not paired, or the phone is away.
        let pair = vbox(14);
        pair.append(&label("Phone", "title"));
        pair.append(&label(
            "Scan this code with your phone's camera, Tailscale on. The page it opens installs the app the first time, then pairs it.",
            "muted",
        ));
        let qr = gtk::Picture::new();
        qr.set_can_shrink(false);
        qr.set_halign(gtk::Align::Center);
        qr.set_margin_top(10);
        pair.append(&qr);
        let url = caption("");
        url.set_selectable(true);
        url.set_halign(gtk::Align::Center);
        pair.append(&url);
        let error = label("", "error");
        error.add_css_class("banner");
        error.set_visible(false);
        pair.append(&error);
        stack.add_named(&pair, Some("pair"));

        // Connected: what the phone reports of itself.
        let linked = vbox(16);
        linked.add_css_class("monitor");
        let head = hbox(12);
        let titles = vbox(4);
        titles.set_hexpand(true);
        let name = label("", "title");
        titles.append(&name);
        let summary = label("", "muted");
        titles.append(&summary);
        head.append(&titles);
        let disconnect = button("Disconnect");
        disconnect.set_tooltip_text(Some("Tap this desktop in the phone app to reconnect"));
        head.append(&disconnect);
        linked.append(&head);
        let grid = gtk::Grid::new();
        grid.set_row_spacing(14);
        grid.set_column_spacing(14);
        grid.set_column_homogeneous(true);
        let battery = stat(&grid, "BATTERY", 0, 0);
        let storage = stat(&grid, "STORAGE", 1, 0);
        let memory = stat(&grid, "MEMORY", 0, 1);
        let cpu = stat(&grid, "CPU", 1, 1);
        linked.append(&grid);
        let actions = hbox(10);
        let files = button("Open files");
        files.add_css_class("suggested-action");
        actions.append(&files);
        let open_messages = button("Messages");
        actions.append(&open_messages);
        linked.append(&actions);
        let status = caption("");
        status.set_visible(false);
        linked.append(&status);
        stack.add_named(&linked, Some("linked"));

        // Messages, a page further.
        let messages_page = vbox(12);
        let top = hbox(10);
        let back = gtk::Button::from_icon_name("go-previous-symbolic");
        back.set_tooltip_text(Some("Back"));
        top.append(&back);
        let heading = label("Messages", "title");
        heading.set_hexpand(true);
        top.append(&heading);
        let new = button("New message");
        top.append(&new);
        messages_page.append(&top);
        let panes = hbox(12);
        panes.set_vexpand(true);
        let threads = list();
        let threads_scroll = scrolled(&threads);
        threads_scroll.set_width_request(250);
        threads_scroll.set_hexpand(false);
        panes.append(&threads_scroll);
        let conversation = vbox(8);
        conversation.set_hexpand(true);
        let with = label("", "row-title");
        conversation.append(&with);
        let to = gtk::Entry::new();
        to.set_placeholder_text(Some("Phone number"));
        to.set_input_purpose(gtk::InputPurpose::Phone);
        to.set_visible(false);
        conversation.append(&to);
        let messages = vbox(6);
        messages.set_valign(gtk::Align::End);
        let messages_scroll = scrolled(&messages);
        conversation.append(&messages_scroll);
        let compose = hbox(8);
        let text = gtk::Entry::new();
        text.set_hexpand(true);
        text.set_placeholder_text(Some("Message"));
        compose.append(&text);
        let send = button("Send");
        send.add_css_class("suggested-action");
        compose.append(&send);
        conversation.append(&compose);
        panes.append(&conversation);
        messages_page.append(&panes);
        stack.add_named(&messages_page, Some("messages"));

        let page = Rc::new(Self {
            widget,
            stack,
            qr,
            url,
            error,
            name,
            summary,
            status,
            battery,
            storage,
            memory,
            cpu,
            state: RefCell::default(),
            threads,
            thread_ids: RefCell::default(),
            messages,
            messages_scroll,
            to,
            with,
            current: RefCell::default(),
        });

        let p = page.clone();
        disconnect.connect_clicked(move |_| {
            let p = p.clone();
            call("disconnect", Value::Null, move |r| p.report(r.map(|_| ())));
        });
        let p = page.clone();
        files.connect_clicked(move |_| {
            let exe = std::env::current_exe()
                .map(|e| e.with_file_name("nexus-files"))
                .unwrap_or_else(|_| "nexus-files".into());
            p.report(
                std::process::Command::new(exe)
                    .arg("--phone")
                    .spawn()
                    .map(|_| ())
                    .map_err(|e| format!("Could not open the file manager: {e}")),
            );
        });
        let p = page.clone();
        open_messages.connect_clicked(move |_| {
            p.stack.set_visible_child_name("messages");
            p.load_threads();
        });
        let p = page.clone();
        back.connect_clicked(move |_| p.stack.set_visible_child_name("linked"));
        let p = page.clone();
        page.threads.connect_row_activated(move |_, row| {
            let Some((thread, address, name)) =
                p.thread_ids.borrow().get(row.index() as usize).cloned()
            else {
                return;
            };
            p.to.set_visible(false);
            p.with
                .set_text(if name.is_empty() { &address } else { &name });
            *p.current.borrow_mut() = Some((thread, address));
            p.load_messages();
        });
        let p = page.clone();
        new.connect_clicked(move |_| {
            *p.current.borrow_mut() = None;
            p.with.set_text("New message");
            clear(&p.messages);
            p.to.set_text("");
            p.to.set_visible(true);
            p.to.grab_focus();
        });
        let p = page.clone();
        let entry = text.clone();
        let sent = move || {
            let body = entry.text().to_string();
            let to = match &*p.current.borrow() {
                Some((_, address)) => address.clone(),
                None => p.to.text().trim().to_owned(),
            };
            if body.trim().is_empty() || to.is_empty() {
                return;
            }
            entry.set_text("");
            let p = p.clone();
            call("sms_send", json!({ "to": to, "body": body }), move |r| {
                let ok = r.is_ok();
                p.report(r.map(|_| ()));
                if ok {
                    // The phone writes the message to its store as it sends it.
                    glib::timeout_add_local_once(
                        std::time::Duration::from_millis(800),
                        move || {
                            p.load_threads();
                            if p.current.borrow().is_some() {
                                p.load_messages();
                            }
                        },
                    );
                }
            });
        };
        let s = sent.clone();
        send.connect_clicked(move |_| s());
        text.connect_activate(move |_| sent());
        page
    }

    pub fn update(self: &Rc<Self>, s: &PhoneState) {
        if *self.state.borrow() == *s {
            return;
        }
        let was = self.state.replace(s.clone());
        if was.pair_url != s.pair_url {
            let texture = s.pair_url.as_deref().and_then(qr_texture);
            self.qr.set_paintable(texture.as_ref());
            self.url.set_text(s.pair_url.as_deref().unwrap_or(""));
        }
        let error = s.error.as_deref().unwrap_or("");
        self.error.set_text(error);
        self.error.set_visible(!error.is_empty());
        let Some(name) = &s.name else {
            self.stack.set_visible_child_name("pair");
            return;
        };
        if was.name.is_none() {
            self.stack.set_visible_child_name("linked");
        }
        let i = s.info.clone().unwrap_or_default();
        self.name.set_text(i["model"].as_str().unwrap_or(name));
        let mut summary = vec![];
        if let Some(v) = i["android"].as_str() {
            summary.push(format!("Android {v}"));
        }
        if let Some(n) = i["network"].as_str() {
            summary.push(n.to_owned());
        }
        if let Some(u) = i["uptime"].as_u64() {
            summary.push(format!("up {}", uptime(u)));
        }
        self.summary.set_text(&summary.join(" · "));
        let b = &i["battery"];
        let level = b["level"].as_f64().unwrap_or(0.);
        let charging = if b["charging"] == true {
            "Charging"
        } else {
            "On battery"
        };
        self.battery.set(
            &format!("{level:.0}%"),
            level / 100.,
            &format!("{charging} · {:.1} °C", b["temp"].as_f64().unwrap_or(0.)),
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
    fn say(&self, text: &str) {
        self.status.set_text(text);
        self.status.set_visible(!text.is_empty());
    }
    fn report(&self, r: Result<(), String>) {
        match r {
            Ok(()) => self.say(""),
            Err(e) => self.say(&e),
        }
    }
    fn load_threads(self: &Rc<Self>) {
        let p = self.clone();
        call("sms_threads", Value::Null, move |r| {
            let data = match r {
                Ok(d) => d,
                Err(e) => return p.say(&e),
            };
            clear_list(&p.threads);
            let mut ids = p.thread_ids.borrow_mut();
            ids.clear();
            for t in data["threads"].as_array().into_iter().flatten() {
                let address = t["address"].as_str().unwrap_or_default().to_owned();
                let name = t["name"].as_str().unwrap_or_default().to_owned();
                let title = if name.is_empty() { &address } else { &name };
                p.threads.append(&row(
                    "mail-unread-symbolic",
                    title,
                    t["snippet"].as_str().unwrap_or_default(),
                ));
                ids.push((t["thread"].as_i64().unwrap_or_default(), address, name));
            }
        });
    }
    fn load_messages(self: &Rc<Self>) {
        let Some((thread, _)) = self.current.borrow().clone() else {
            return;
        };
        let p = self.clone();
        call("sms_list", json!({ "thread": thread }), move |r| {
            let data = match r {
                Ok(d) => d,
                Err(e) => return p.say(&e),
            };
            clear(&p.messages);
            for m in data["messages"].as_array().into_iter().flatten() {
                let bubble = label(m["body"].as_str().unwrap_or_default(), "");
                bubble.add_css_class("card");
                bubble.set_selectable(true);
                bubble.set_max_width_chars(48);
                let out = m["out"] == true;
                bubble.set_halign(if out {
                    gtk::Align::End
                } else {
                    gtk::Align::Start
                });
                p.messages.append(&bubble);
            }
            // Newest at the bottom, in view.
            let adj = p.messages_scroll.vadjustment();
            glib::idle_add_local_once(move || adj.set_value(adj.upper()));
        });
    }
}
