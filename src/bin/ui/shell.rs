use super::super::{hbox, signal_icon};
use super::{
    dock::{self, Dock},
    osd::Osd,
    slide::{self, Pose, Slide},
};
use gtk::{glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use nexus_control::{Request, Snapshot, request};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

pub struct Surface {
    pub window: gtk::ApplicationWindow,
    pub content: gtk::Stack,
    slide: Slide,
    /// Mapped with the window and before it, so always under it.
    catcher: gtk::ApplicationWindow,
    pub page: RefCell<String>,
    /// The panel is on its way out; the window hides once it is gone.
    closing: Cell<bool>,
    anchors: RefCell<Vec<PanelAnchor>>,
}
struct PanelAnchor {
    page: &'static str,
    monitor: gtk::gdk::Monitor,
    button: glib::WeakRef<gtk::Button>,
}
/// Height of the bar, whatever its shape; panels hang below it.
pub const BAR: i32 = 32;
/// Between the bar and what hangs from it.
pub const GAP: i32 = 6;
/// Between a floating surface and the side of the screen.
pub const INSET: i32 = 8;
/// Matches the power tiles' leaving animations in the theme.
const POWER_LEAVE: Duration = Duration::from_millis(120);
/// A panel just before it opens and just after it closes: tucked toward the bar.
// Never faded: Hyprland only blurs where a surface is opaque enough, so a fade
// would show the panel clear first and the blur arrive with a jump at the end.
const TUCKED: Pose = Pose {
    x: 0.,
    y: -6.,
    opacity: 1.,
};
/// Room a panel's surface keeps around it for its shadow: 32px of blur, 14 down.
const SHADOW: i32 = 40;
/// Full-screen and clear, under a panel: takes the clicks around it and never
/// redraws, so that only the panel's own small surface does, with its blur.
fn click_away(app: &gtk::Application) -> gtk::ApplicationWindow {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Nexus click-away")
        .build();
    window.add_css_class("nexus");
    window.add_css_class("shell-overlay");
    window.init_layer_shell();
    window.set_namespace(Some("nexus-click-away"));
    window.set_layer(Layer::Overlay);
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    window.set_exclusive_zone(-1);
    window.set_margin(Edge::Top, BAR);
    window.set_keyboard_mode(KeyboardMode::None);
    // A surface that paints nothing gets no buffer and never maps: one pixel,
    // too faint to see, instead of a whole screen darkened by as little.
    let seed = gtk::Box::new(gtk::Orientation::Vertical, 0);
    seed.add_css_class("click-away-seed");
    seed.set_halign(gtk::Align::Start);
    seed.set_valign(gtk::Align::Start);
    window.set_child(Some(&seed));
    window
}
/// Calls `close` on a click on `window` outside `keep`.
fn close_on_click(
    window: &gtk::ApplicationWindow,
    keep: Option<gtk::Widget>,
    close: impl Fn() + 'static,
) {
    let click = gtk::GestureClick::new();
    click.connect_released(move |click, _, x, y| {
        let point = gtk::graphene::Point::new(x as f32, y as f32);
        let inside = keep.as_ref().is_some_and(|keep| {
            click
                .widget()
                .and_then(|window| keep.compute_bounds(&window))
                .is_some_and(|bounds| bounds.contains_point(&point))
        });
        if !inside {
            close();
        }
    });
    window.add_controller(click);
}
/// Where a surface goes for a panel at `left`, `width` wide, keeping room for its
/// shadow where the screen has some: the surface's margin, and the room on each side.
fn shadow_room(left: i32, width: i32, screen: i32) -> (i32, i32, i32) {
    let before = left.clamp(0, SHADOW);
    (
        left - before,
        before,
        (screen - left - width).clamp(0, SHADOW),
    )
}
impl Surface {
    pub fn new(app: &gtk::Application) -> Rc<Self> {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Nexus")
            .build();
        window.add_css_class("nexus");
        window.add_css_class("shell-overlay");
        window.init_layer_shell();
        window.set_namespace(Some("nexus-panel"));
        window.set_layer(Layer::Overlay);
        window.set_anchor(Edge::Top, true);
        window.set_anchor(Edge::Bottom, true);
        window.set_exclusive_zone(-1);
        window.set_margin(Edge::Top, BAR);
        // On demand rather than exclusive: Hyprland still focuses the panel when it
        // maps, but an exclusive layer would also take every click, the bar's too.
        window.set_keyboard_mode(KeyboardMode::OnDemand);
        let content = gtk::Stack::new();
        content.set_hhomogeneous(false);
        content.set_vhomogeneous(false);
        content.add_css_class("shell-panel");
        content.set_overflow(gtk::Overflow::Hidden);
        let slide = Slide::default();
        slide.set_child(&content);
        slide.set_halign(gtk::Align::Center);
        slide.set_valign(gtk::Align::Start);
        window.set_child(Some(&slide));
        let this = Rc::new(Self {
            window,
            content,
            slide,
            catcher: click_away(app),
            page: RefCell::new(String::new()),
            closing: Cell::new(false),
            anchors: RefCell::default(),
        });
        let close = |s: &Rc<Self>| {
            let weak = Rc::downgrade(s);
            move || {
                if let Some(s) = weak.upgrade() {
                    s.hide();
                }
            }
        };
        close_on_click(&this.catcher, None, close(&this));
        close_on_click(
            &this.window,
            Some(this.content.clone().upcast()),
            close(&this),
        );
        let weak = Rc::downgrade(&this);
        this.window.connect_close_request(move |_| {
            if let Some(s) = weak.upgrade() {
                s.hide();
            }
            glib::Propagation::Stop
        });
        let weak = Rc::downgrade(&this);
        let key = gtk::EventControllerKey::new();
        key.set_propagation_phase(gtk::PropagationPhase::Capture);
        key.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                if let Some(s) = weak.upgrade() {
                    s.hide();
                }
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        this.window.add_controller(key);
        this
    }
    pub fn is_open(&self, page: &str) -> bool {
        self.window.is_visible() && self.page.borrow().as_str() == page
    }
    /// Sends the panel back under the bar, then hides the window. The power
    /// tiles fly back out first.
    pub fn hide(self: &Rc<Self>) {
        self.page.borrow_mut().clear();
        let power = self.window.has_css_class("power-overlay");
        if power && self.window.is_visible() && !self.window.has_css_class("leaving") {
            self.closing.set(true);
            self.window.add_css_class("leaving");
            let s = self.clone();
            glib::timeout_add_local_once(POWER_LEAVE, move || {
                // Unless it opened again meanwhile; still leaving, it now hides.
                if s.closing.get() {
                    s.hide();
                }
            });
            return;
        }
        self.window.remove_css_class("leaving");
        if power || !self.window.is_visible() {
            self.window.set_keyboard_mode(KeyboardMode::None);
            self.closing.set(false);
            self.window.set_visible(false);
            self.catcher.set_visible(false);
            return;
        }
        self.closing.set(true);
        let s = self.clone();
        self.slide.spring_to(TUCKED, slide::LEAVE, move || {
            // Focus comes with mapping: a panel reopened on its way out keeps it.
            if s.closing.replace(false) {
                s.window.set_keyboard_mode(KeyboardMode::None);
                s.window.set_visible(false);
                s.catcher.set_visible(false);
            }
        });
    }
    /// Both surfaces leave for `monitor`: the catcher stays under, as it maps first.
    fn set_monitor(&self, monitor: &gtk::gdk::Monitor) {
        if self.window.monitor().as_ref() != Some(monitor) {
            self.window.set_visible(false);
            self.catcher.set_visible(false);
            self.window.set_monitor(Some(monitor));
            self.catcher.set_monitor(Some(monitor));
        }
    }
    pub fn show(self: &Rc<Self>, page: &str, monitor: Option<&gtk::gdk::Monitor>) {
        if let Some(monitor) = monitor {
            self.set_monitor(monitor);
        }
        *self.page.borrow_mut() = page.into();
        let was_shown = self.window.is_visible() && !self.closing.get();
        self.closing.set(false);
        self.window.remove_css_class("leaving");
        let power = page == "power";
        // The power tiles fly in from beyond the panel, which must not clip them.
        self.content.set_overflow(if power {
            gtk::Overflow::Visible
        } else {
            gtk::Overflow::Hidden
        });
        self.slide.set_valign(gtk::Align::Start);
        self.slide.set_margin_top(GAP);
        if power {
            // Dims and blurs the whole screen, the bar included.
            self.window.add_css_class("power-overlay");
            self.window.set_margin(Edge::Top, 0);
            self.slide.set_valign(gtk::Align::Center);
            self.slide.set_margin_top(0);
        } else {
            self.window.remove_css_class("power-overlay");
            self.window.set_margin(Edge::Top, BAR);
        }
        self.content.set_visible_child_name(page);
        let monitor = self.window.monitor();
        if page == "launcher" {
            // In the upper part of the screen: it grows down as results come, not around.
            let height = monitor.as_ref().map_or(1000, |m| m.geometry().height());
            self.slide.set_margin_top((height / 5 - BAR).max(GAP));
        }
        let below = self.anchors.borrow().iter().find_map(|anchor| {
            (anchor.page == page && monitor.as_ref().is_none_or(|m| m == &anchor.monitor))
                .then(|| anchor.button.upgrade().map(|b| (b, anchor.monitor.clone())))
                .flatten()
        });
        if let Some((_, monitor)) = &below {
            self.set_monitor(monitor);
        }
        // As wide as the panel and its shadow, centred by the compositor unless it
        // hangs from a button. The height stays, so that a page unfolding inside
        // never resizes the surface. Set once: each change here is a configure.
        let room = if power { 0 } else { SHADOW };
        let (margin, before, after) = below
            .and_then(|(button, monitor)| self.below(&button, &monitor))
            .unwrap_or((0, room, room));
        self.window
            .set_anchor(Edge::Left, power || margin + before > room);
        self.window.set_anchor(Edge::Right, power);
        self.window.set_margin(Edge::Left, margin);
        self.slide.set_margin_start(before);
        self.slide.set_margin_end(after);
        self.window.set_keyboard_mode(KeyboardMode::OnDemand);
        if power {
            // The tiles animate on their own.
            self.slide.place(Pose::REST);
        } else if !was_shown {
            if !self.window.is_visible() {
                self.slide.place(TUCKED);
            }
            self.slide.spring_to(Pose::REST, slide::ARRIVE, || {});
        }
        // ponytail: also under the power chooser, which needs no catcher, at about 25%
        // more compositor work there; hiding it means remapping the panel above it.
        self.catcher.present();
        self.window.present();
        // Only the launcher and the power chooser are typed at. A key meant for the
        // window behind must not land on the panel's first button, say the Wi-Fi's.
        if page != "launcher" && !power {
            let window = self.window.clone();
            glib::idle_add_local_once(move || {
                gtk::prelude::GtkWindowExt::set_focus(&window, None::<&gtk::Widget>)
            });
        }
    }
    pub(super) fn anchor(
        &self,
        page: &'static str,
        monitor: &gtk::gdk::Monitor,
        button: &gtk::Button,
    ) {
        let mut anchors = self.anchors.borrow_mut();
        anchors.retain(|anchor| anchor.button.upgrade().is_some());
        anchors.push(PanelAnchor {
            page,
            monitor: monitor.clone(),
            button: button.downgrade(),
        });
    }
    /// The surface's margin and the room for the shadow on each side, to centre the
    /// panel under its button, as far as the screen allows.
    fn below(&self, button: &gtk::Button, monitor: &gtk::gdk::Monitor) -> Option<(i32, i32, i32)> {
        let window = button.root().and_downcast::<gtk::ApplicationWindow>()?;
        let bounds = button.compute_bounds(&window)?;
        // Of the panel alone: the slide's would count its room for the shadow.
        let (_, width, _, _) = self.content.measure(gtk::Orientation::Horizontal, -1);
        let width = width as f32;
        let centre = bounds.x() + bounds.width() / 2.;
        let screen = monitor.geometry().width() as f32;
        let left = (centre - width / 2.).clamp(
            INSET as f32,
            (screen - width - INSET as f32).max(INSET as f32),
        );
        Some(shadow_room(left as i32, width as i32, screen as i32))
    }
}

/// A floating panel at the right of the screen, sliding in from beyond the edge.
pub struct Drawer {
    window: gtk::ApplicationWindow,
    slide: Slide,
    catcher: gtk::ApplicationWindow,
    open: Cell<bool>,
}
impl Drawer {
    pub fn new(app: &gtk::Application, child: &impl IsA<gtk::Widget>) -> Rc<Self> {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Nexus drawer")
            .build();
        window.add_css_class("nexus");
        window.add_css_class("drawer");
        window.init_layer_shell();
        window.set_namespace(Some("nexus-drawer"));
        window.set_layer(Layer::Overlay);
        for edge in [Edge::Top, Edge::Bottom, Edge::Right] {
            window.set_anchor(edge, true);
        }
        window.set_exclusive_zone(-1);
        window.set_margin(Edge::Top, BAR);
        window.set_keyboard_mode(KeyboardMode::None);
        let panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
        panel.add_css_class("drawer-panel");
        panel.set_overflow(gtk::Overflow::Hidden);
        panel.append(child);
        let slide = Slide::default();
        slide.set_halign(gtk::Align::End);
        slide.set_valign(gtk::Align::Start);
        slide.set_margin_top(GAP);
        slide.set_margin_start(SHADOW);
        slide.set_margin_end(INSET);
        slide.set_margin_bottom(INSET);
        slide.set_child(&panel);
        window.set_child(Some(&slide));
        let this = Rc::new(Self {
            window,
            slide,
            catcher: click_away(app),
            open: Cell::new(false),
        });
        let close = |s: &Rc<Self>| {
            let weak = Rc::downgrade(s);
            move || {
                if let Some(s) = weak.upgrade() {
                    s.hide();
                }
            }
        };
        close_on_click(&this.catcher, None, close(&this));
        close_on_click(&this.window, Some(panel.upcast()), close(&this));
        let weak = Rc::downgrade(&this);
        this.window.connect_close_request(move |_| {
            if let Some(s) = weak.upgrade() {
                s.hide();
            }
            glib::Propagation::Stop
        });
        let weak = Rc::downgrade(&this);
        let key = gtk::EventControllerKey::new();
        key.set_propagation_phase(gtk::PropagationPhase::Capture);
        key.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                if let Some(s) = weak.upgrade() {
                    s.hide();
                }
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        this.window.add_controller(key);
        this
    }
    pub fn is_open(&self) -> bool {
        self.open.get()
    }
    /// Just past the edge of the screen.
    fn away(&self) -> Pose {
        Pose {
            x: self.slide.natural_size().0 - SHADOW as f32 + 2. * INSET as f32,
            ..Pose::REST
        }
    }
    pub fn show(self: &Rc<Self>, monitor: Option<&gtk::gdk::Monitor>) {
        if let Some(monitor) = monitor
            && self.window.monitor().as_ref() != Some(monitor)
        {
            self.window.set_visible(false);
            self.catcher.set_visible(false);
            self.window.set_monitor(Some(monitor));
            self.catcher.set_monitor(Some(monitor));
        }
        if !self.window.is_visible() {
            self.slide.place(self.away());
        }
        self.open.set(true);
        self.window.set_keyboard_mode(KeyboardMode::OnDemand);
        self.slide.spring_to(Pose::REST, slide::ARRIVE, || {});
        self.catcher.present();
        self.window.present();
        // As for a panel: a key meant for the window behind must not land on its switch.
        let window = self.window.clone();
        glib::idle_add_local_once(move || {
            gtk::prelude::GtkWindowExt::set_focus(&window, None::<&gtk::Widget>)
        });
    }
    pub fn hide(self: &Rc<Self>) {
        self.open.set(false);
        let s = self.clone();
        self.slide.spring_to(self.away(), slide::LEAVE, move || {
            if !s.open.get() {
                s.window.set_keyboard_mode(KeyboardMode::None);
                s.window.set_visible(false);
                s.catcher.set_visible(false);
            }
        });
    }
}

pub type Open = Rc<dyn Fn(&str, Option<&gtk::gdk::Monitor>)>;
struct Indicator {
    button: gtk::Button,
    icon: gtk::Image,
    value: gtk::Label,
}
struct Bar {
    monitor: gtk::gdk::Monitor,
    window: gtk::ApplicationWindow,
    dock: Rc<Dock>,
    app: gtk::Label,
    notifications: Indicator,
    /// The paired phone: its battery while connected, dimmed while away, red
    /// when the link is broken; hidden until a phone has paired.
    phone: Indicator,
    tray: Indicator,
    record: Indicator,
    wifi: Indicator,
    bluetooth: Indicator,
    sound: Indicator,
    brightness: Indicator,
    battery: Indicator,
    clock: gtk::Label,
    workspaces: gtk::Box,
    /// The accent gliding behind the active workspace.
    pill: Slide,
    workspace_ids: Vec<i64>,
    workspace_active: Option<i64>,
}
pub struct Bars {
    app: gtk::Application,
    bars: RefCell<Vec<Bar>>,
    open: Open,
    surface: Rc<Surface>,
    osd: Rc<Osd>,
    /// Applications by window class, and what the dock keeps at hand.
    apps: RefCell<dock::Apps>,
    /// The compositor's last state, for what depends on it and something else.
    windows: RefCell<Option<super::workspaces::State>>,
    /// Live notifications and do not disturb, for bars created later.
    notices: Cell<(usize, bool)>,
    tray_count: Cell<usize>,
    /// When the screen recording in progress started.
    recording: Cell<Option<Instant>>,
    /// Volume and brightness as last known, and when the wheel last set them.
    levels: Cell<[(Option<u8>, Option<Instant>); 2]>,
    /// The levels last shown, so that only a change brings up the overlay.
    shown: Cell<[Option<(u8, bool)>; 2]>,
    setters: [std::sync::mpsc::Sender<u8>; 2],
    this: std::rc::Weak<Self>,
}
/// The daemon setting `op`, one value at a time: a burst of wheel steps sends
/// only the last one, rather than queueing up behind each other.
fn setter(op: &'static str) -> std::sync::mpsc::Sender<u8> {
    let (tx, rx) = std::sync::mpsc::channel::<u8>();
    std::thread::spawn(move || {
        while let Ok(mut value) = rx.recv() {
            while let Ok(newer) = rx.try_recv() {
                value = newer;
            }
            let _ = request(&Request::Action {
                op: op.into(),
                target: String::new(),
                value: value.to_string(),
            });
        }
    });
    tx
}
/// How long a wheel change outweighs the state read before it.
const WHEEL_HOLD: Duration = Duration::from_millis(1500);
/// Width of a workspace button, and of the pill behind the active one.
const WORKSPACE_WIDTH: i32 = 26;
/// The Nexus mark: three nodes joined. Drawn, so that it follows the text colour.
fn brand_mark() -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_content_width(16);
    area.set_content_height(16);
    area.set_valign(gtk::Align::Center);
    area.set_draw_func(|area, cr, w, h| {
        cr.set_source_color(&area.color());
        let (w, h) = (f64::from(w), f64::from(h));
        let nodes = [(0.5, 0.16), (0.14, 0.82), (0.86, 0.82)].map(|(x, y)| (x * w, y * h));
        cr.set_line_width(1.5);
        cr.move_to(nodes[0].0, nodes[0].1);
        cr.line_to(nodes[1].0, nodes[1].1);
        cr.line_to(nodes[2].0, nodes[2].1);
        cr.close_path();
        let _ = cr.stroke();
        for (x, y) in nodes {
            cr.arc(x, y, 2.5, 0., std::f64::consts::TAU);
            let _ = cr.fill();
        }
    });
    area
}
/// A bar label stays on one line: a wrapping one gets squeezed and clipped.
fn bar_label(text: &str, class: &str) -> gtk::Label {
    let l = super::super::label(text, class);
    l.set_wrap(false);
    l
}
const VOLUME: usize = 0;
const BRIGHTNESS: usize = 1;
impl Bars {
    pub fn new(app: &gtk::Application, open: Open, surface: Rc<Surface>) -> Rc<Self> {
        let this = Rc::new_cyclic(|this| Self {
            app: app.clone(),
            bars: RefCell::new(vec![]),
            open,
            surface,
            osd: Osd::new(app),
            apps: RefCell::new(dock::Apps::saved()),
            windows: RefCell::default(),
            notices: Cell::new((0, false)),
            tray_count: Cell::new(0),
            recording: Cell::new(None),
            levels: Cell::new([(None, None); 2]),
            shown: Cell::new([None; 2]),
            setters: [setter("volume"), setter("brightness")],
            this: this.clone(),
        });
        this.sync();
        this.follow_clock();
        let weak = Rc::downgrade(&this);
        gtk::gdk::Display::default()
            .unwrap()
            .monitors()
            .connect_items_changed(move |_, _, _, _| {
                if let Some(this) = weak.upgrade() {
                    this.sync();
                }
            });
        this
    }
    /// Shows the time as each minute starts, and only then.
    fn follow_clock(self: &Rc<Self>) {
        let second = glib::DateTime::now_local().map_or(0., |t| t.seconds());
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_secs_f64(60.05 - second), move || {
            if let Some(this) = weak.upgrade() {
                let time = clock_text();
                for bar in this.bars.borrow().iter() {
                    bar.clock.set_text(&time);
                }
                this.follow_clock();
            }
        });
    }
    fn sync(&self) {
        let model = gtk::gdk::Display::default().unwrap().monitors();
        let monitors: Vec<_> = (0..model.n_items())
            .filter_map(|i| model.item(i).and_downcast::<gtk::gdk::Monitor>())
            .collect();
        let mut bars = self.bars.borrow_mut();
        bars.retain(|b| {
            if monitors.contains(&b.monitor) {
                true
            } else {
                b.window.close();
                b.dock.close();
                false
            }
        });
        for monitor in monitors {
            if !bars.iter().any(|b| b.monitor == monitor) {
                bars.push(self.create(&monitor));
            }
        }
        drop(bars);
        // A bar made after the compositor's last event still has to show it.
        let state = self.windows.borrow().clone();
        if let Some(state) = state {
            self.workspaces(&state);
        }
    }
    fn create(&self, monitor: &gtk::gdk::Monitor) -> Bar {
        let window = gtk::ApplicationWindow::builder()
            .application(&self.app)
            .title("Nexus bar")
            .build();
        window.add_css_class("nexus");
        window.add_css_class("nexus-bar");
        window.init_layer_shell();
        window.set_namespace(Some("nexus-bar"));
        window.set_monitor(Some(monitor));
        window.set_layer(Layer::Top);
        window.set_keyboard_mode(KeyboardMode::None);
        for edge in [Edge::Top, Edge::Left, Edge::Right] {
            window.set_anchor(edge, true);
        }
        window.auto_exclusive_zone_enable();
        // Left: the system menu, the application in front and the workspaces.
        let left = hbox(0);
        left.add_css_class("bar-island");
        let brand = self.button(monitor, "", "System", "system");
        brand.add_css_class("bar-brand");
        brand.set_child(Some(&brand_mark()));
        brand.update_property(&[gtk::accessible::Property::Label("System menu")]);
        self.surface.anchor("system", monitor, &brand);
        left.append(&brand);
        let app = bar_label("Desktop", "bar-app");
        app.set_ellipsize(gtk::pango::EllipsizeMode::End);
        app.set_max_width_chars(32);
        left.append(&app);
        let workspaces = hbox(2);
        workspaces.set_homogeneous(true);
        workspaces.set_valign(gtk::Align::Center);
        let pill_shape = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        pill_shape.add_css_class("workspace-pill");
        pill_shape.set_size_request(WORKSPACE_WIDTH, 24);
        let pill = Slide::default();
        pill.set_child(&pill_shape);
        pill.set_halign(gtk::Align::Start);
        pill.set_valign(gtk::Align::Center);
        pill.set_visible(false);
        pill.set_can_target(false);
        let track = gtk::Overlay::new();
        track.set_child(Some(&pill));
        track.add_overlay(&workspaces);
        track.set_measure_overlay(&workspaces, true);
        track.set_margin_end(4);
        left.append(&track);
        // Right: only what is worth a glance, then search, Control Center and the time.
        let right = hbox(0);
        right.add_css_class("bar-island");
        let record = self.indicator(monitor, "media-record-symbolic", "", "");
        record
            .button
            .connect_clicked(|_| run_nexus(&["screenshot", "--record"]));
        show_recording(&record, self.recording.get());
        let tray = self.indicator(
            monitor,
            "application-x-executable-symbolic",
            "Tray applications",
            "tray",
        );
        tray.button.set_visible(self.tray_count.get() > 0);
        self.surface.anchor("tray", monitor, &tray.button);
        let notifications = self.indicator(
            monitor,
            "preferences-system-notifications-symbolic",
            "Notifications",
            "notifications",
        );
        let phone = self.indicator(monitor, "phone-symbolic", "Phone", "phone");
        phone.button.add_css_class("bar-phone");
        phone.button.set_visible(false);
        let wifi = self.indicator(monitor, "network-wireless-symbolic", "Wi-Fi", "quick-wifi");
        let bluetooth = self.indicator(
            monitor,
            "bluetooth-symbolic",
            "Bluetooth",
            "quick-bluetooth",
        );
        bluetooth.button.set_visible(false);
        let sound = self.indicator(
            monitor,
            "audio-volume-high-symbolic",
            "Sound",
            "quick-sound",
        );
        let brightness = self.indicator(
            monitor,
            "display-brightness-symbolic",
            "Brightness",
            "quick-display",
        );
        let battery = self.indicator(monitor, "battery-symbolic", "Battery", "battery");
        self.surface.anchor("battery", monitor, &battery.button);
        for (which, indicator) in [(VOLUME, &sound), (BRIGHTNESS, &brightness)] {
            let scroll = gtk::EventControllerScroll::new(
                gtk::EventControllerScrollFlags::VERTICAL
                    | gtk::EventControllerScrollFlags::DISCRETE,
            );
            let bars = self.this.clone();
            scroll.connect_scroll(move |_, _, dy| {
                if let Some(bars) = bars.upgrade() {
                    bars.wheel(which, dy);
                }
                glib::Propagation::Stop
            });
            indicator.button.add_controller(scroll);
        }
        let search = self.indicator(
            monitor,
            "system-search-symbolic",
            "Search applications · Super+Space",
            "launcher",
        );
        let quick = self.indicator(monitor, "view-grid-symbolic", "Control Center", "quick");
        self.surface.anchor("quick", monitor, &quick.button);
        for b in [
            &record,
            &tray,
            &phone,
            &notifications,
            &wifi,
            &bluetooth,
            &sound,
            &brightness,
            &battery,
            &search,
            &quick,
        ] {
            right.append(&b.button);
        }
        let clock_button = self.button(monitor, "", "Notifications and calendar", "notifications");
        let clock = bar_label(&clock_text(), "bar-clock");
        clock_button.set_child(Some(&clock));
        right.append(&clock_button);
        let center = gtk::CenterBox::new();
        center.add_css_class("bar-content");
        center.set_start_widget(Some(&left));
        center.set_end_widget(Some(&right));
        center.set_hexpand(true);
        let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        body.add_css_class("bar-body");
        body.append(&center);
        window.set_child(Some(&body));
        window.present();
        let (count, quiet) = self.notices.get();
        show_notices(&notifications, count, quiet);
        Bar {
            monitor: monitor.clone(),
            window,
            dock: Dock::new(&self.app, monitor, {
                let shell = self.this.clone();
                Rc::new(move |id, keep| {
                    if let Some(shell) = shell.upgrade() {
                        shell.pin(id, keep);
                    }
                })
            }),
            app,
            notifications,
            phone,
            tray,
            record,
            wifi,
            bluetooth,
            sound,
            brightness,
            battery,
            clock,
            workspaces,
            pill,
            workspace_ids: vec![],
            workspace_active: None,
        }
    }
    fn button(
        &self,
        monitor: &gtk::gdk::Monitor,
        text: &str,
        tooltip: &str,
        page: &'static str,
    ) -> gtk::Button {
        let b = gtk::Button::with_label(text);
        b.set_tooltip_text(Some(tooltip));
        if !page.is_empty() {
            let open = self.open.clone();
            let monitor = monitor.clone();
            b.connect_clicked(move |_| open(page, Some(&monitor)));
        }
        b
    }
    fn indicator(
        &self,
        monitor: &gtk::gdk::Monitor,
        icon: &str,
        tooltip: &str,
        page: &'static str,
    ) -> Indicator {
        let button = self.button(monitor, "", tooltip, page);
        let row = hbox(6);
        let icon = gtk::Image::from_icon_name(icon);
        icon.set_pixel_size(16);
        row.append(&icon);
        let value = bar_label("", "bar-value");
        value.set_visible(false);
        row.append(&value);
        button.set_child(Some(&row));
        Indicator {
            button,
            icon,
            value,
        }
    }
    /// Shows a recording and its length on every bar while it runs.
    pub fn set_recording(self: &Rc<Self>, started: Option<Instant>) {
        self.recording.set(started);
        for bar in self.bars.borrow().iter() {
            show_recording(&bar.record, started);
        }
        if started.is_some() {
            let weak = Rc::downgrade(self);
            glib::timeout_add_local(Duration::from_secs(1), move || {
                let Some(this) = weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                // A later recording runs its own timer.
                if this.recording.get() != started {
                    return glib::ControlFlow::Break;
                }
                for bar in this.bars.borrow().iter() {
                    show_recording(&bar.record, started);
                }
                glib::ControlFlow::Continue
            });
        }
    }
    /// One wheel step over the sound or brightness button: 5% up or down.
    fn wheel(&self, which: usize, dy: f64) {
        let mut levels = self.levels.get();
        let Some(current) = levels[which].0 else {
            return;
        };
        let floor = if which == BRIGHTNESS { 1 } else { 0 };
        let step = if dy < 0. { 5 } else { -5 };
        let next = (i16::from(current) + step).clamp(floor, 100) as u8;
        levels[which] = (Some(next), Some(Instant::now()));
        self.levels.set(levels);
        let _ = self.setters[which].send(next);
        let muted = which == VOLUME && self.shown.get()[which].is_some_and(|shown| shown.1);
        self.show_level(which, next, muted);
    }
    /// Brings up the overlay when a level differs from the last one shown; the
    /// first reading after startup is not a change.
    fn show_level(&self, which: usize, value: u8, muted: bool) {
        let mut shown = self.shown.get();
        let was = shown[which].replace((value, muted));
        self.shown.set(shown);
        if was.is_some_and(|was| was != (value, muted)) {
            if which == VOLUME {
                self.osd
                    .show(sound_icon(muted, value), if muted { 0 } else { value });
            } else {
                self.osd.show("display-brightness-symbolic", value);
            }
        }
    }
    /// The backlight as read just now, faster than the daemon's state follows it.
    pub fn brightness(&self, value: u8) {
        let mut levels = self.levels.get();
        if levels[BRIGHTNESS]
            .1
            .is_none_or(|t| t.elapsed() > WHEEL_HOLD)
        {
            levels[BRIGHTNESS].0 = Some(value);
            self.levels.set(levels);
            self.show_level(BRIGHTNESS, value, false);
        }
    }
    /// `read` from the daemon, unless the wheel changed it just before.
    fn level(&self, which: usize, read: Option<u8>) -> Option<u8> {
        let mut levels = self.levels.get();
        // The backlight is read directly and more often than the daemon reads it.
        let live = which == BRIGHTNESS && levels[which].0.is_some();
        if levels[which].1.is_none_or(|t| t.elapsed() > WHEEL_HOLD) {
            levels[which] = (if live { levels[which].0 } else { read }, None);
            self.levels.set(levels);
        }
        levels[which].0
    }
    pub fn tray(&self, count: usize) {
        self.tray_count.set(count);
        for bar in self.bars.borrow().iter() {
            bar.tray.button.set_visible(count > 0);
            bar.tray
                .button
                .set_tooltip_text(Some(&format!("Tray applications · {count}")));
        }
    }
    pub fn update(&self, s: &Snapshot) {
        // Also here: the minute's timer stands still while the machine sleeps.
        let time = clock_text();
        let battery = battery();
        let (volume, brightness) = (
            self.level(VOLUME, s.volume),
            self.level(BRIGHTNESS, s.brightness),
        );
        if let Some(volume) = volume {
            self.show_level(VOLUME, volume, s.muted);
        }
        let phone = phone_indicator(&s.phone);
        for bar in self.bars.borrow().iter() {
            bar.clock.set_text(&time);
            bar.phone.button.set_visible(phone.is_some());
            if let Some((class, value, tooltip)) = &phone {
                bar.phone.button.set_tooltip_text(Some(tooltip));
                for c in ["away", "error", "low"] {
                    bar.phone.button.remove_css_class(c);
                }
                if !class.is_empty() {
                    bar.phone.button.add_css_class(class);
                }
                bar.phone.value.set_visible(!value.is_empty());
                bar.phone.value.set_text(value);
            }
            let network = s.networks.iter().find(|n| n.connected());
            // The cable carries the traffic whenever it is up.
            let wired = s.wired.as_ref().filter(|w| w.state == "connected");
            bar.wifi.icon.set_icon_name(Some(match network {
                _ if wired.is_some() => "network-wired-symbolic",
                Some(n) => signal_icon(n.signal),
                None if s.wifi => "network-wireless-offline-symbolic",
                None => "network-wireless-disabled-symbolic",
            }));
            bar.wifi.button.set_tooltip_text(Some(
                wired
                    .map(|_| "Ethernet")
                    .or(s.wifi_error.as_deref())
                    .or(network.map(|n| n.name.as_str()))
                    .unwrap_or(if s.wifi {
                        "Wi-Fi · Not connected"
                    } else {
                        "Wi-Fi off"
                    }),
            ));
            let connected = s.devices.iter().filter(|d| d.connected).count();
            bar.bluetooth.icon.set_icon_name(Some(if !s.bluetooth {
                "bluetooth-disabled-symbolic"
            } else if connected > 0 {
                "bluetooth-active-symbolic"
            } else {
                "bluetooth-symbolic"
            }));
            // Worth a place in the bar only while something is connected.
            bar.bluetooth.button.set_visible(connected > 0);
            bar.bluetooth
                .button
                .set_tooltip_text(Some(&format!("Bluetooth · {connected} connected")));
            bar.sound
                .icon
                .set_icon_name(Some(sound_icon(s.muted, volume.unwrap_or(0))));
            bar.sound.button.set_tooltip_text(Some(&match volume {
                _ if s.muted => "Sound · Muted".to_string(),
                Some(v) => format!("Sound · {v}%"),
                None => "Sound".to_string(),
            }));
            bar.brightness.button.set_tooltip_text(Some(
                &brightness.map_or("Brightness".to_string(), |v| format!("Brightness · {v}%")),
            ));
            bar.battery.button.set_visible(battery.is_some());
            if let Some((capacity, charging)) = battery {
                bar.battery
                    .icon
                    .set_icon_name(Some(&battery_icon(capacity.into(), charging)));
                bar.battery.value.set_visible(true);
                bar.battery.value.set_text(&format!("{capacity}%"));
                bar.battery.button.set_tooltip_text(Some(if charging {
                    "Battery · Charging"
                } else {
                    "Battery"
                }));
                if capacity <= 15 && !charging {
                    bar.battery.button.add_css_class("error");
                } else {
                    bar.battery.button.remove_css_class("error");
                }
            }
        }
    }
    pub fn notifications(&self, count: usize, quiet: bool) {
        self.notices.set((count, quiet));
        for bar in self.bars.borrow().iter() {
            show_notices(&bar.notifications, count, quiet);
        }
    }
    fn pin(&self, id: &str, keep: bool) {
        {
            let mut apps = self.apps.borrow_mut();
            apps.set_kept(id, keep);
            apps.save();
        }
        let state = self.windows.borrow().clone();
        if let Some(state) = state {
            self.workspaces(&state);
        }
    }
    /// The applications the dock and the bar name, as the daemon lists them.
    pub fn catalogue(&self, catalogue: &nexus_control::launcher::Catalogue) {
        self.apps.borrow_mut().update(catalogue);
        // Windows were named and docked with what was known before.
        let state = self.windows.borrow().clone();
        if let Some(state) = state {
            self.workspaces(&state);
        }
    }
    pub fn workspaces(&self, state: &super::workspaces::State) {
        *self.windows.borrow_mut() = Some(state.clone());
        let (workspaces, monitors, clients, focused) = state;
        let Some(workspaces) = workspaces.as_array() else {
            return;
        };
        let apps = self.apps.borrow();
        let docked = apps.docked(clients);
        for bar in self.bars.borrow_mut().iter_mut() {
            let connector = bar.monitor.connector().unwrap_or_default();
            let active = monitors
                .as_array()
                .and_then(|all| {
                    all.iter()
                        .find(|m| m["name"].as_str() == Some(connector.as_str()))
                })
                .and_then(|m| m["activeWorkspace"]["id"].as_i64())
                .unwrap_or(1);
            let mut ids: Vec<_> = workspaces
                .iter()
                .filter(|w| w["monitor"].as_str() == Some(connector.as_str()))
                .filter_map(|w| w["id"].as_i64())
                .filter(|id| *id > 0)
                .collect();
            ids.sort();
            ids.dedup();
            bar.show_workspaces(ids, active);
            // The application in front of this screen, as the bar's heading.
            let front = (focused["workspace"]["id"].as_i64() == Some(active))
                .then(|| focused["class"].as_str())
                .flatten();
            bar.app
                .set_text(&front.map_or("Desktop".to_string(), |class| apps.name(class)));
            let empty = !clients.as_array().is_some_and(|all| {
                all.iter()
                    .any(|c| c["workspace"]["id"].as_i64() == Some(active))
            });
            bar.dock.update(&docked, empty);
        }
    }
}
impl Bar {
    /// The buttons change with the workspaces; the pill alone follows the focus.
    fn show_workspaces(&mut self, ids: Vec<i64>, active: i64) {
        let same = ids == self.workspace_ids;
        if !same {
            while let Some(c) = self.workspaces.first_child() {
                self.workspaces.remove(&c);
            }
            for id in &ids {
                let id = *id;
                let b = gtk::Button::with_label(&id.to_string());
                b.add_css_class("workspace");
                b.set_tooltip_text(Some(&format!("Workspace {id}")));
                b.connect_clicked(move |_| {
                    std::thread::spawn(move || {
                        let _ = nexus_control::backend::run(
                            "hyprctl",
                            &["dispatch", &format!("hl.dsp.focus({{ workspace = {id} }})")],
                        );
                    });
                });
                self.workspaces.append(&b);
            }
        }
        if same && self.workspace_active == Some(active) {
            return;
        }
        let mut child = self.workspaces.first_child();
        while let Some(c) = child {
            let is_active = c
                .downcast_ref::<gtk::Button>()
                .and_then(|b| b.label())
                .is_some_and(|l| l == active.to_string());
            if is_active {
                c.add_css_class("active");
            } else {
                c.remove_css_class("active");
            }
            child = c.next_sibling();
        }
        let place = ids.iter().position(|id| *id == active);
        self.pill.set_visible(place.is_some());
        if let Some(place) = place {
            let x = place as f32 * (WORKSPACE_WIDTH + 2) as f32;
            let target = Pose { x, ..Pose::REST };
            // From where it stands only if it was standing somewhere.
            if self.workspace_active.is_some() {
                self.pill.spring_to(target, slide::SNAP, || {});
            } else {
                self.pill.place(target);
            }
        }
        self.workspace_ids = ids;
        self.workspace_active = Some(active);
    }
}
fn clock_text() -> String {
    glib::DateTime::now_local()
        .ok()
        .map(|t| {
            let days = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
            let months = [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ];
            format!(
                "{} {} {}  {:02}:{:02}",
                days[(t.day_of_week() - 1) as usize],
                t.day_of_month(),
                months[(t.month() - 1) as usize],
                t.hour(),
                t.minute()
            )
        })
        .unwrap_or_default()
}
/// The path of a binary that may have been replaced while running, as `install.sh`
/// does: Linux then calls it `…/nexus (deleted)`, and the new one is at the old path.
pub fn live_path(exe: std::path::PathBuf) -> std::path::PathBuf {
    exe.to_str()
        .and_then(|path| path.strip_suffix(" (deleted)"))
        .map_or_else(|| exe.clone(), Into::into)
}
/// Runs `nexus` with `args`; the request reaches this process through its socket.
/// A failure is said on screen: a Lock that did nothing must not pass for a locked
/// screen, and its panel is gone by now.
pub fn run_nexus(args: &[&str]) {
    let exe = std::env::current_exe().map_or_else(|_| "nexus".into(), live_path);
    let spawned = std::process::Command::new(exe).args(args).spawn();
    let command = args.join(" ");
    std::thread::spawn(move || {
        let why = match spawned.and_then(|mut child| child.wait()) {
            Err(e) => e.to_string(),
            Ok(status) => match status.code() {
                Some(code) if code != 0 => format!("It exited with status {code}."),
                _ => return,
            },
        };
        say_nexus_failed(&command, &why);
    });
}
/// Off the main thread: the notification server may be this very process.
fn say_nexus_failed(command: &str, why: &str) {
    eprintln!("nexus {command} failed: {why}");
    let summary = if command == "lock" {
        "The screen is not locked".to_string()
    } else {
        format!("Could not run nexus {command}")
    };
    let sent = zbus::blocking::Connection::session().and_then(|bus| {
        bus.call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "Nexus",
                0u32,
                "dialog-error-symbolic",
                summary,
                why,
                Vec::<&str>::new(),
                std::collections::HashMap::<&str, zbus::zvariant::Value>::new(),
                -1i32,
            ),
        )
    });
    if let Err(e) = sent {
        eprintln!("Could not say so: {e}");
    }
}
/// The speaker for a level: crossed out when muted or at nothing.
pub fn sound_icon(muted: bool, level: u8) -> &'static str {
    match level {
        _ if muted => "audio-volume-muted-symbolic",
        0 => "audio-volume-muted-symbolic",
        1..35 => "audio-volume-low-symbolic",
        35..70 => "audio-volume-medium-symbolic",
        _ => "audio-volume-high-symbolic",
    }
}
fn show_recording(indicator: &Indicator, started: Option<Instant>) {
    // Only a recording in progress takes room in the bar; Control Center starts one.
    indicator.button.set_visible(started.is_some());
    let Some(started) = started else {
        indicator.button.remove_css_class("bar-recording");
        indicator.icon.set_icon_name(Some("media-record-symbolic"));
        indicator.value.set_visible(false);
        indicator
            .button
            .set_tooltip_text(Some("Record the screen · Super+Shift+R"));
        return;
    };
    let seconds = started.elapsed().as_secs();
    indicator.button.add_css_class("bar-recording");
    indicator
        .icon
        .set_icon_name(Some("media-playback-stop-symbolic"));
    indicator
        .value
        .set_text(&format!("{}:{:02}", seconds / 60, seconds % 60));
    indicator.value.set_visible(true);
    indicator.button.set_tooltip_text(Some("Stop recording"));
}
fn show_notices(indicator: &Indicator, count: usize, quiet: bool) {
    // The clock opens the list: the bell only says there is something in it.
    indicator.button.set_visible(count > 0 || quiet);
    indicator.icon.set_icon_name(Some(if quiet {
        "notifications-disabled-symbolic"
    } else {
        "preferences-system-notifications-symbolic"
    }));
    indicator.value.set_visible(count > 0);
    indicator.value.set_text(&count.to_string());
    indicator
        .button
        .set_tooltip_text(Some(&match (count, quiet) {
            (0, false) => "No notifications".to_string(),
            (0, true) => "Do not disturb".to_string(),
            (1, _) => "1 notification".to_string(),
            (n, _) => format!("{n} notifications"),
        }));
}
/// The last battery reading, taken off the UI thread by [`read_battery`].
static BATTERY: std::sync::Mutex<Option<(u8, bool)>> = std::sync::Mutex::new(None);
pub fn battery() -> Option<(u8, bool)> {
    *BATTERY.lock().unwrap()
}
/// Reading `capacity` asks the embedded controller through ACPI, which takes
/// about 100 ms on some laptops: it must never block the main loop.
pub fn read_battery() {
    *BATTERY.lock().unwrap() = nexus_control::power::read_battery();
}

/// The Adwaita icon for a battery at `level` percent. Full, it is "charged"
/// rather than "charging": there is no full charging icon.
fn battery_icon(level: u64, charging: bool) -> String {
    let level = level.min(100) / 10 * 10;
    let state = match (charging, level) {
        (false, _) => "",
        (true, 100) => "-charged",
        (true, _) => "-charging",
    };
    format!("battery-level-{level}{state}-symbolic")
}

/// The bar's phone: its class (`away`, `error`, `low`), its battery text and
/// its tooltip; `None` until a phone has paired.
fn phone_indicator(p: &nexus_control::phone::PhoneState) -> Option<(&'static str, String, String)> {
    let Some(name) = &p.name else {
        let last = p.last.as_ref()?;
        return Some(match &p.error {
            Some(e) => ("error", String::new(), format!("{last} · {e}")),
            None => ("away", String::new(), format!("{last} · Not connected")),
        });
    };
    let info = p.info.clone().unwrap_or_default();
    let name = info["model"].as_str().unwrap_or(name);
    let network = info["network"].as_str().unwrap_or("Connected");
    let Some(level) = info["battery"]["level"].as_u64() else {
        return Some(("", String::new(), format!("{name} · {network}")));
    };
    let charging = info["battery"]["charging"] == true;
    Some((
        if level <= 15 && !charging { "low" } else { "" },
        format!("{level}%"),
        format!(
            "{name} · {level}%{} · {network}",
            if charging { ", charging" } else { "" }
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_replaced_binary_is_found_at_its_path() {
        let path = |p: &str| std::path::PathBuf::from(p);
        assert_eq!(live_path(path("/bin/nexus (deleted)")), path("/bin/nexus"));
        assert_eq!(live_path(path("/bin/nexus")), path("/bin/nexus"));
        // Why `current_exe().with_file_name(…)` for a sibling needs no such care.
        assert_eq!(
            path("/bin/nexus (deleted)").with_file_name("nexusd"),
            path("/bin/nexusd")
        );
    }
    #[test]
    #[ignore = "needs a notification server: run it inside the lab"]
    fn a_failed_lock_is_said() {
        say_nexus_failed("lock", "No such file or directory (os error 2)");
    }
    #[test]
    fn shadow_room_stays_on_the_screen() {
        assert_eq!(shadow_room(700, 380, 2048), (660, 40, 40));
        assert_eq!(shadow_room(8, 380, 2048), (0, 8, 40));
        assert_eq!(shadow_room(1660, 380, 2048), (1620, 40, 8));
    }
}
