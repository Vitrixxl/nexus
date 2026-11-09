use super::{
    super::{caption, hbox, label, vbox},
    shell::{BAR, GAP},
    slide::{self, Pose, Slide},
};
use gtk::{gio, glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use nexus_control::notifications::{Image, Notification, PICTURE, Server, Urgency};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

/// Popups on screen at once; older ones remain in the history.
const MAX_POPUPS: usize = 4;
const SLIDE: u32 = 160;
/// A card with the room its shadow needs, which the revealer around it would clip.
const POPUP_WIDTH: i32 = 404;
const CENTER_WIDTH: i32 = 380;

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
        // Decoded at the size shown, not the file's own, and away from the UI thread:
        // a photograph takes long enough to stall every animation on screen.
        Image::File(path) => {
            let widget = gtk::Image::new();
            let (image, path) = (widget.clone(), path.clone());
            glib::spawn_future_local(async move {
                match gio::spawn_blocking(move || decode(&path)).await {
                    Ok(Some(texture)) => image.set_paintable(Some(&texture)),
                    _ => image.set_visible(false),
                }
            });
            widget
        }
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
fn decode(path: &str) -> Option<gtk::gdk::MemoryTexture> {
    let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(path, PICTURE, PICTURE, true).ok()?;
    Some(gtk::gdk::MemoryTexture::new(
        pixbuf.width(),
        pixbuf.height(),
        if pixbuf.has_alpha() {
            gtk::gdk::MemoryFormat::R8g8b8a8
        } else {
            gtk::gdk::MemoryFormat::R8g8b8
        },
        &pixbuf.read_pixel_bytes(),
        pixbuf.rowstride() as usize,
    ))
}
/// The sender's icon: the one it named, else that of its desktop entry or of an
/// application with its name. Without one, its name says it alone: the same bell
/// on every card would say nothing.
fn app_icon(n: &Notification) -> Option<gtk::Image> {
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
/// One notification. `lines` bounds the body; `clicked` follows any click on it.
fn card(n: &Notification, server: &Rc<Server>, lines: i32, clicked: Rc<dyn Fn()>) -> gtk::Box {
    let card = vbox(8);
    card.add_css_class("notification");
    if n.urgency == Urgency::Critical {
        card.add_css_class("critical");
    }
    let head = hbox(8);
    if let Some(icon) = app_icon(n) {
        head.append(&icon);
    }
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
    app.set_max_width_chars(24);
    head.append(&app);
    if n.phone {
        let from = gtk::Image::from_icon_name("phone-symbolic");
        from.set_pixel_size(12);
        from.add_css_class("notification-from");
        from.set_tooltip_text(Some("From your phone"));
        head.append(&from);
    }
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
        summary.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        summary.set_max_width_chars(1);
        text.append(&summary);
    }
    if !n.body.is_empty() {
        let body = label("", "body");
        body.set_markup(&n.body);
        body.set_lines(lines);
        body.set_ellipsize(gtk::pango::EllipsizeMode::End);
        body.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        // A wrapping label asks for its whole text on one line: without this
        // cap, a long message stretches the card across the screen. The text
        // then fills the width its container gives it.
        body.set_max_width_chars(1);
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
        }
        clicked();
    });
    card.add_controller(click);
    card
}

/// Dragging the card toward the screen edge (`dir` 1 right, -1 left) shifts
/// it; let go far enough and it flies off the screen before `dismiss` runs,
/// else it springs back.
fn swipe(slide: &Slide, dir: f64, dismiss: impl Fn() + 'static) {
    let drag = gtk::GestureDrag::new();
    let s = slide.downgrade();
    drag.connect_drag_update(move |gesture, dx, _| {
        let Some(slide) = s.upgrade() else { return };
        let dx = dx * dir;
        // Past a few pixels this is a swipe, not a click on the card.
        if dx > 6. {
            gesture.set_state(gtk::EventSequenceState::Claimed);
        }
        slide.drag_to((dx.max(0.) * dir) as f32, 0.);
    });
    let s = slide.downgrade();
    let dismiss = Rc::new(dismiss);
    drag.connect_drag_end(move |_, dx, _| {
        let Some(slide) = s.upgrade() else { return };
        let width = slide.width() as f32;
        if (dx * dir) as f32 > (width * 0.3).min(120.) {
            let dismiss = dismiss.clone();
            let away = Pose {
                x: width * dir as f32,
                opacity: 0.,
                ..Pose::REST
            };
            slide.spring_to(away, slide::LEAVE, move || dismiss());
        } else {
            slide.spring_to(Pose::REST, slide::ARRIVE, || {});
        }
    });
    slide.add_controller(drag);
}

struct Popup {
    id: u32,
    revealer: gtk::Revealer,
    slide: Slide,
    timeout: Cell<Option<Duration>>,
    timer: RefCell<Option<glib::SourceId>>,
}
/// Notifications as they arrive, separate cards sliding in under the bar, the
/// newest at the top.
pub struct Popups {
    window: gtk::Window,
    list: gtk::Box,
    server: Rc<Server>,
    shown: RefCell<Vec<Rc<Popup>>>,
    /// Do not disturb: only critical notifications pop up.
    quiet: Cell<bool>,
    left: bool,
}
impl Popups {
    /// Under the bar on `side`: `Edge::Right`, or `Edge::Left` for the phone's.
    pub fn new(app: &gtk::Application, server: Rc<Server>, side: Edge) -> Rc<Self> {
        let left = side == Edge::Left;
        let window = gtk::Window::builder()
            .application(app)
            .title("Nexus notifications")
            .build();
        window.add_css_class("nexus");
        window.add_css_class("notification-popups");
        if left {
            window.add_css_class("left");
        }
        window.init_layer_shell();
        window.set_namespace(Some("nexus-notifications"));
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::None);
        // Over everything, whatever reserves the edges.
        window.set_exclusive_zone(-1);
        window.set_anchor(Edge::Top, true);
        window.set_anchor(side, true);
        window.set_margin(Edge::Top, BAR + GAP);
        let list = vbox(0);
        list.add_css_class("popup-panel");
        list.set_width_request(POPUP_WIDTH);
        window.set_child(Some(&list));
        Rc::new(Self {
            window,
            list,
            server,
            shown: RefCell::default(),
            quiet: Cell::new(false),
            left,
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
                popup.slide.set_child(&content);
                self.fit();
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
        let slide = Slide::default();
        slide.set_measure_width(POPUP_WIDTH);
        slide.set_child(content);
        revealer.set_child(Some(&slide));
        let weak = Rc::downgrade(self);
        swipe(&slide, if self.left { -1. } else { 1. }, move || {
            if let Some(s) = weak.upgrade() {
                s.hide(id, false);
            }
        });
        let popup = Rc::new(Popup {
            id,
            revealer: revealer.clone(),
            slide,
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
        // In from beyond the edge, as the cards below make room.
        let dir = if self.left { -1. } else { 1. };
        popup.slide.place(Pose {
            x: dir * POPUP_WIDTH as f32,
            opacity: 0.,
            ..Pose::REST
        });
        popup.slide.spring_to(Pose::REST, slide::ARRIVE, || {});
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if revealer.parent().is_none() {
                return;
            }
            revealer.set_reveal_child(true);
            if let Some(s) = weak.upgrade() {
                s.fit_during(&revealer);
            }
        });
        popup
    }
    /// A window keeps its size as its content shrinks: size it to its cards.
    fn fit(&self) {
        let Some(child) = self.window.child() else {
            return;
        };
        let (_, width, _, _) = child.measure(gtk::Orientation::Horizontal, -1);
        let (_, height, _, _) = child.measure(gtk::Orientation::Vertical, width);
        self.window.set_default_size(width, height);
    }
    /// Fits the window every frame of a card's slide, so that the panel rises
    /// and sinks with it.
    fn fit_during(self: &Rc<Self>, revealer: &gtk::Revealer) {
        let weak = Rc::downgrade(self);
        revealer.add_tick_callback(move |revealer, _| {
            if let Some(s) = weak.upgrade() {
                s.fit();
            }
            if revealer.reveals_child() == revealer.is_child_revealed() {
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
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
    pub fn hide(self: &Rc<Self>, id: u32, expired: bool) {
        let popup = {
            let mut shown = self.shown.borrow_mut();
            let Some(i) = shown.iter().position(|p| p.id == id) else {
                return;
            };
            shown.remove(i)
        };
        Self::stop(&popup);
        // Gone before it ever appeared: its revealer would never report closing.
        if !popup.revealer.reveals_child() {
            self.list.remove(&popup.revealer);
            if self.list.first_child().is_none() {
                self.window.set_visible(false);
            }
            if expired {
                self.server.expire(id);
            }
            return;
        }
        let dir = if self.left { -1. } else { 1. };
        popup.slide.spring_to(
            Pose {
                x: dir * POPUP_WIDTH as f32,
                opacity: 0.,
                ..Pose::REST
            },
            slide::LEAVE,
            || {},
        );
        popup.revealer.set_reveal_child(false);
        self.fit_during(&popup.revealer);
        if expired {
            self.server.expire(id);
        }
    }
    pub fn clear(self: &Rc<Self>) {
        let ids: Vec<_> = self.shown.borrow().iter().map(|p| p.id).collect();
        for id in ids {
            self.hide(id, false);
        }
    }
}

/// The calendar and the notifications still live, in a panel at the right of the screen.
pub struct Center {
    pub widget: gtk::Box,
    /// Do not disturb.
    pub quiet: gtk::Switch,
    calendar: gtk::Calendar,
    /// Closes the panel, once it exists, when a notification is acted on.
    pub close: RefCell<Rc<dyn Fn()>>,
    list: gtk::Box,
    scroll: gtk::ScrolledWindow,
    empty: gtk::Label,
    status: gtk::Label,
    count: gtk::Label,
    clear: gtk::Button,
    server: Rc<Server>,
}
/// The calendar under "November 2026 ‹ ›".
fn calendar_card(calendar: &gtk::Calendar) -> gtk::Box {
    let month = label("", "calendar-month");
    month.set_hexpand(true);
    month.set_xalign(0.);
    let head = hbox(2);
    head.add_css_class("calendar-head");
    head.append(&month);
    for (icon, title, by) in [
        ("go-previous-symbolic", "Previous month", -1),
        ("go-next-symbolic", "Next month", 1),
    ] {
        let step = gtk::Button::from_icon_name(icon);
        step.add_css_class("calendar-step");
        step.set_tooltip_text(Some(title));
        step.update_property(&[gtk::accessible::Property::Label(title)]);
        let calendar = calendar.clone();
        step.connect_clicked(move |_| {
            if let Ok(date) = calendar.date().add_months(by) {
                calendar.select_day(&date);
            }
        });
        head.append(&step);
    }
    let title = move |calendar: &gtk::Calendar| {
        if let Ok(text) = calendar.date().format("%B %Y") {
            month.set_text(&text);
        }
    };
    title(calendar);
    // A click on a day of another month moves it too.
    calendar.connect_notify_local(None, move |calendar, _| title(calendar));
    let card = vbox(2);
    card.add_css_class("center-calendar");
    card.append(&head);
    card.append(calendar);
    card
}
impl Center {
    pub fn new(server: Rc<Server>) -> Rc<Self> {
        let widget = vbox(12);
        widget.add_css_class("notification-center");
        widget.set_width_request(CENTER_WIDTH);
        let calendar = gtk::Calendar::new();
        calendar.set_show_week_numbers(false);
        // Its own heading has a pair of steppers each for the month and the year.
        calendar.set_show_heading(false);
        widget.append(&calendar_card(&calendar));
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
        // A scrolled window asks for its height at no width, for which wrapped text
        // answers as if squeezed narrow: blank space under the list.
        let sized = Slide::default();
        sized.set_measure_width(CENTER_WIDTH - 24);
        sized.set_child(&list);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(520)
            .child(&sized)
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
            calendar,
            close: RefCell::new(Rc::new(|| {})),
            list,
            scroll,
            empty,
            status,
            count,
            clear,
            server,
        })
    }
    /// Back to today. Only on opening: a notification arriving must not undo
    /// the browsing of months. It may have stayed up for days; today moves on.
    pub fn today(&self) {
        if let Ok(now) = glib::DateTime::now_local() {
            self.calendar.select_day(&now);
        }
    }
    pub fn refresh(&self) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let all = self.server.list();
        let close = self.close.borrow().clone();
        for n in &all {
            // A click that opens nothing leaves the list where it is, to be read on.
            let clicked: Rc<dyn Fn()> = if n.default_action() {
                close.clone()
            } else {
                Rc::new(|| {})
            };
            self.list.append(&card(n, &self.server, 8, clicked));
        }
        self.scroll.set_visible(!all.is_empty());
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
