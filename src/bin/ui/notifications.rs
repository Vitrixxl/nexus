use super::super::{caption, hbox, label, vbox};
use gtk::{gio, glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use nexus_control::notifications::{Image, Notification, Server, Urgency};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

/// Popups on screen at once; older ones remain in the history.
const MAX_POPUPS: usize = 4;
const SLIDE: u32 = 180;

fn picture(image: &Image, size: i32) -> Option<gtk::Image> {
    let widget = match image {
        Image::Pixels(p) => {
            let texture = gtk::gdk::MemoryTexture::new(
                p.width,
                p.height,
                if p.has_alpha {
                    gtk::gdk::MemoryFormat::R8g8b8a8
                } else {
                    gtk::gdk::MemoryFormat::R8g8b8
                },
                &p.data,
                p.rowstride as usize,
            );
            gtk::Image::from_paintable(Some(&texture))
        }
        Image::File(path) if std::path::Path::new(path).is_file() => gtk::Image::from_file(path),
        Image::Named(name)
            if gtk::IconTheme::for_display(&gtk::gdk::Display::default()?).has_icon(name) =>
        {
            gtk::Image::from_icon_name(name)
        }
        _ => return None,
    };
    widget.set_pixel_size(size);
    Some(widget)
}
/// The sender's icon: the one it named, else that of its desktop entry or of an
/// application with its name.
fn app_icon(n: &Notification) -> gtk::Image {
    n.app_icon
        .as_ref()
        .and_then(|i| picture(i, 16))
        .or_else(|| {
            let entry = n.desktop_entry.as_ref()?;
            let icon = gio::DesktopAppInfo::new(&format!("{entry}.desktop"))?.icon()?;
            let image = gtk::Image::from_gicon(&icon);
            image.set_pixel_size(16);
            Some(image)
        })
        .or_else(|| picture(&Image::Named(n.app_name.to_lowercase()), 16))
        .unwrap_or_else(|| gtk::Image::from_icon_name("preferences-system-notifications-symbolic"))
}
/// "now", minutes ago, then the time of day, then the date.
fn age(time: i64) -> String {
    let (Ok(now), Ok(then)) = (
        glib::DateTime::now_local(),
        glib::DateTime::from_unix_local(time),
    ) else {
        return String::new();
    };
    let seconds = now.to_unix() - time;
    let format = |f: &str| then.format(f).map(|s| s.to_string()).unwrap_or_default();
    match seconds {
        ..60 => "now".into(),
        60..3600 => format!("{} min", seconds / 60),
        _ if then.ymd() == now.ymd() => format("%H:%M"),
        _ => format("%d %b"),
    }
}
/// One notification. `lines` bounds the body; `clicked` handles a click on a
/// notification without a default action.
fn card(n: &Notification, server: &Rc<Server>, lines: i32, clicked: Rc<dyn Fn()>) -> gtk::Box {
    let card = vbox(8);
    card.add_css_class("notification");
    if n.urgency == Urgency::Critical {
        card.add_css_class("critical");
    }
    let head = hbox(8);
    head.append(&app_icon(n));
    let app = label(
        if n.app_name.is_empty() {
            "Notification"
        } else {
            &n.app_name
        },
        "app-name",
    );
    app.set_wrap(false);
    app.set_ellipsize(gtk::pango::EllipsizeMode::End);
    head.append(&app);
    let time = caption(&age(n.time));
    time.set_wrap(false);
    time.set_hexpand(true);
    head.append(&time);
    let close = gtk::Button::from_icon_name("window-close-symbolic");
    close.add_css_class("flat");
    close.add_css_class("notification-close");
    close.set_tooltip_text(Some("Dismiss"));
    let (s, id) = (server.clone(), n.id);
    close.connect_clicked(move |_| s.dismiss(id));
    head.append(&close);
    card.append(&head);
    let main = hbox(12);
    if let Some(image) = n.image.as_ref().and_then(|i| picture(i, 48)) {
        let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
        frame.add_css_class("notification-image");
        frame.set_overflow(gtk::Overflow::Hidden);
        frame.set_valign(gtk::Align::Start);
        frame.append(&image);
        main.append(&frame);
    }
    let text = vbox(2);
    text.set_hexpand(true);
    if !n.summary.is_empty() {
        let summary = label(&n.summary, "summary");
        summary.set_lines(2);
        summary.set_ellipsize(gtk::pango::EllipsizeMode::End);
        text.append(&summary);
    }
    if !n.body.is_empty() {
        let body = label("", "body");
        body.set_markup(&n.body);
        body.set_lines(lines);
        body.set_ellipsize(gtk::pango::EllipsizeMode::End);
        body.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        text.append(&body);
    }
    main.append(&text);
    card.append(&main);
    if let Some(value) = n.value {
        let bar = gtk::ProgressBar::new();
        bar.set_fraction(f64::from(value) / 100.);
        card.append(&bar);
    }
    let actions: Vec<_> = n.actions.iter().filter(|(k, _)| k != "default").collect();
    if !actions.is_empty() {
        let row = hbox(6);
        row.add_css_class("actions");
        row.set_homogeneous(true);
        for (key, text) in actions {
            let b = gtk::Button::with_label(if text.is_empty() { key } else { text });
            let (s, key) = (server.clone(), key.clone());
            b.connect_clicked(move |_| s.invoke(id, &key));
            row.append(&b);
        }
        card.append(&row);
    }
    let default = n.default_action();
    if default {
        card.set_cursor_from_name(Some("pointer"));
    }
    let click = gtk::GestureClick::new();
    let s = server.clone();
    click.connect_released(move |gesture, _, _, _| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        if default {
            s.invoke(id, "default");
        } else {
            clicked();
        }
    });
    card.add_controller(click);
    card
}

struct Popup {
    id: u32,
    revealer: gtk::Revealer,
    timeout: Cell<Option<Duration>>,
    timer: RefCell<Option<glib::SourceId>>,
}
/// Notifications as they arrive, stacked in the top-right corner below the bar.
pub struct Popups {
    window: gtk::Window,
    list: gtk::Box,
    server: Rc<Server>,
    shown: RefCell<Vec<Rc<Popup>>>,
    /// Do not disturb: only critical notifications pop up.
    quiet: Cell<bool>,
}
impl Popups {
    pub fn new(app: &gtk::Application, server: Rc<Server>) -> Rc<Self> {
        let window = gtk::Window::builder()
            .application(app)
            .title("Nexus notifications")
            .build();
        window.add_css_class("nexus");
        window.add_css_class("notification-popups");
        window.init_layer_shell();
        window.set_namespace(Some("nexus-notifications"));
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::None);
        // From the very top of the screen, over the bar and everything else,
        // rather than pushed down below the bar's exclusive zone.
        window.set_exclusive_zone(-1);
        window.set_anchor(Edge::Top, true);
        window.set_anchor(Edge::Right, true);
        window.set_margin(Edge::Top, 6);
        window.set_margin(Edge::Right, 6);
        let list = vbox(0);
        list.set_width_request(400);
        window.set_child(Some(&list));
        Rc::new(Self {
            window,
            list,
            server,
            shown: RefCell::default(),
            quiet: Cell::new(false),
        })
    }
    pub fn set_quiet(&self, quiet: bool) {
        self.quiet.set(quiet);
    }
    pub fn show(self: &Rc<Self>, n: &Notification) {
        if self.quiet.get() && n.urgency != Urgency::Critical {
            return;
        }
        let weak = Rc::downgrade(self);
        let id = n.id;
        let content = card(
            n,
            &self.server,
            4,
            Rc::new(move || {
                if let Some(s) = weak.upgrade() {
                    s.hide(id, false);
                }
            }),
        );
        let existing = self.shown.borrow().iter().find(|p| p.id == id).cloned();
        let popup = match existing {
            Some(popup) => {
                popup.revealer.set_child(Some(&content));
                popup
            }
            None => self.add(id, &content),
        };
        popup.timeout.set(n.timeout);
        self.start(&popup);
        let excess: Vec<_> = self
            .shown
            .borrow()
            .iter()
            .skip(MAX_POPUPS)
            .map(|p| p.id)
            .collect();
        for id in excess {
            self.hide(id, false);
        }
    }
    fn add(self: &Rc<Self>, id: u32, content: &gtk::Box) -> Rc<Popup> {
        let revealer = gtk::Revealer::new();
        revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
        revealer.set_transition_duration(SLIDE);
        revealer.set_child(Some(content));
        let popup = Rc::new(Popup {
            id,
            revealer: revealer.clone(),
            timeout: Cell::new(None),
            timer: RefCell::default(),
        });
        // Reading a popup holds it on screen.
        let hover = gtk::EventControllerMotion::new();
        let (weak, p) = (Rc::downgrade(self), Rc::downgrade(&popup));
        hover.connect_enter(move |_, _, _| {
            if let Some(p) = p.upgrade() {
                Self::stop(&p);
            }
        });
        let p = Rc::downgrade(&popup);
        hover.connect_leave(move |_| {
            if let (Some(s), Some(p)) = (weak.upgrade(), p.upgrade())
                && s.shown.borrow().iter().any(|o| Rc::ptr_eq(o, &p))
            {
                s.start(&p);
            }
        });
        revealer.add_controller(hover);
        let weak = Rc::downgrade(self);
        revealer.connect_child_revealed_notify(move |r| {
            if let Some(s) = weak.upgrade()
                && !r.reveals_child()
                && !r.is_child_revealed()
            {
                s.list.remove(r);
                if s.list.first_child().is_none() {
                    s.window.set_visible(false);
                }
            }
        });
        self.list.prepend(&revealer);
        self.shown.borrow_mut().insert(0, popup.clone());
        self.window.set_visible(true);
        glib::idle_add_local_once(move || revealer.set_reveal_child(true));
        popup
    }
    fn stop(popup: &Popup) {
        if let Some(timer) = popup.timer.take() {
            timer.remove();
        }
    }
    fn start(self: &Rc<Self>, popup: &Rc<Popup>) {
        Self::stop(popup);
        let Some(timeout) = popup.timeout.get() else {
            return;
        };
        let (weak, p) = (Rc::downgrade(self), Rc::downgrade(popup));
        let timer = glib::timeout_add_local_once(timeout, move || {
            if let (Some(s), Some(p)) = (weak.upgrade(), p.upgrade()) {
                // The source is finishing; it must not be removed again.
                p.timer.take();
                s.hide(p.id, true);
            }
        });
        *popup.timer.borrow_mut() = Some(timer);
    }
    /// Slides the popup away. A popup that `expired` lets the server drop a
    /// transient notification.
    pub fn hide(&self, id: u32, expired: bool) {
        let popup = {
            let mut shown = self.shown.borrow_mut();
            let Some(i) = shown.iter().position(|p| p.id == id) else {
                return;
            };
            shown.remove(i)
        };
        Self::stop(&popup);
        popup.revealer.set_reveal_child(false);
        if expired {
            self.server.expire(id);
        }
    }
    pub fn clear(&self) {
        let ids: Vec<_> = self.shown.borrow().iter().map(|p| p.id).collect();
        for id in ids {
            self.hide(id, false);
        }
    }
}

/// History of the notifications still live, dropped from the bar.
pub struct Center {
    pub widget: gtk::Box,
    /// Do not disturb.
    pub quiet: gtk::Switch,
    list: gtk::Box,
    empty: gtk::Label,
    status: gtk::Label,
    count: gtk::Label,
    clear: gtk::Button,
    server: Rc<Server>,
}
impl Center {
    pub fn new(server: Rc<Server>) -> Rc<Self> {
        let widget = vbox(12);
        widget.add_css_class("notification-center");
        widget.set_width_request(460);
        let head = hbox(10);
        let title = label("Notifications", "center-title");
        title.set_hexpand(true);
        title.set_valign(gtk::Align::Center);
        head.append(&title);
        let quiet_label = caption("Do not disturb");
        quiet_label.set_valign(gtk::Align::Center);
        head.append(&quiet_label);
        let quiet = gtk::Switch::new();
        quiet.set_valign(gtk::Align::Center);
        head.append(&quiet);
        widget.append(&head);
        let status = label("", "banner");
        status.add_css_class("muted");
        status.set_visible(false);
        widget.append(&status);
        let list = vbox(8);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(520)
            .child(&list)
            .build();
        widget.append(&scroll);
        let empty = label("No notifications", "empty-state");
        empty.set_xalign(0.5);
        widget.append(&empty);
        let foot = hbox(10);
        let count = caption("");
        count.set_hexpand(true);
        count.set_valign(gtk::Align::Center);
        foot.append(&count);
        let clear = gtk::Button::with_label("Clear all");
        clear.add_css_class("flat");
        let s = server.clone();
        clear.connect_clicked(move |_| s.clear());
        foot.append(&clear);
        widget.append(&foot);
        Rc::new(Self {
            widget,
            quiet,
            list,
            empty,
            status,
            count,
            clear,
            server,
        })
    }
    pub fn refresh(&self) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let all = self.server.list();
        for n in &all {
            self.list.append(&card(n, &self.server, 8, Rc::new(|| {})));
        }
        self.list.set_visible(!all.is_empty());
        self.empty.set_visible(all.is_empty());
        self.clear.set_sensitive(!all.is_empty());
        self.count.set_text(&match all.len() {
            0 => String::new(),
            1 => "1 notification".into(),
            n => format!("{n} notifications"),
        });
        let status = self.server.status();
        self.status.set_text(status.as_deref().unwrap_or_default());
        self.status.set_visible(status.is_some());
    }
}
