use super::super::{hbox, label, signal_icon};
use gtk::{glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use nexus_control::Snapshot;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

pub struct Surface {
    pub window: gtk::ApplicationWindow,
    pub content: gtk::Stack,
    pub revealer: gtk::Revealer,
    clip: gtk::Box,
    corners: [gtk::Widget; 4],
    pub page: RefCell<String>,
    /// The panel is folding up; the window hides once it is gone.
    closing: Cell<bool>,
    anchors: RefCell<Vec<PanelAnchor>>,
}
struct PanelAnchor {
    page: &'static str,
    monitor: gtk::gdk::Monitor,
    button: glib::WeakRef<gtk::Button>,
}
const FILLET: i32 = 14;
/// The bar's fillets sweep further along the screen edge than down its sides.
const BAR_FILLET: i32 = 30;
/// Bottom edge of the bar (34px high, flush with the top of the screen), less 1px
/// so the panel joins it.
const BAR_BOTTOM: i32 = 33;
/// Share of the monitor width left empty on each side of the bar.
const BAR_INSET: f64 = 0.15;
/// Room around the panel for its drop shadow; margins are not part of the input
/// target, so clicks there still reach the dismissing backdrop.
const SHADOW: i32 = 64;
/// Concave corner joining a top edge to the side of the surface hanging from it:
/// the bar from the screen edge, and a dropped panel from the bar, so each flows
/// out of the edge above instead of meeting it at a right angle. It is `width`
/// wide plus a solid column laid over the surface's edge, so no seam shows
/// between the two at fractional scales.
fn fillet(right: bool, width: i32) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.add_css_class("panel-fillet");
    area.set_content_width(width + 1);
    area.set_content_height(FILLET);
    area.set_valign(gtk::Align::Start);
    area.set_draw_func(move |area, cr, w, h| {
        let (w, h) = (f64::from(w), f64::from(h));
        let color = area.color();
        cr.set_source_rgba(
            f64::from(color.red()),
            f64::from(color.green()),
            f64::from(color.blue()),
            f64::from(color.alpha()),
        );
        // Square minus a quarter ellipse centred on the outer bottom corner.
        use std::f64::consts::{FRAC_PI_2, PI};
        let quarter = |cr: &gtk::cairo::Context, x: f64, from: f64, to: f64| {
            cr.save().ok();
            cr.translate(x, h);
            cr.scale(w - 1., h);
            cr.arc_negative(0., 0., 1., from, to);
            cr.restore().ok();
        };
        cr.move_to(0., 0.);
        cr.line_to(w, 0.);
        if right {
            quarter(cr, w, -FRAC_PI_2, -PI);
            cr.line_to(0., h);
        } else {
            cr.line_to(w, h);
            cr.line_to(w - 1., h);
            quarter(cr, 0., 0., -FRAC_PI_2);
        }
        cr.close_path();
        let _ = cr.fill();
    });
    area
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
        for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            window.set_anchor(edge, true);
        }
        window.set_exclusive_zone(-1);
        window.set_margin(Edge::Top, BAR_BOTTOM);
        // On demand rather than exclusive: Hyprland still focuses the panel when it
        // maps, but an exclusive layer would also take every click, the bar's too.
        window.set_keyboard_mode(KeyboardMode::OnDemand);
        let overlay = gtk::Overlay::new();
        let backdrop = gtk::Box::new(gtk::Orientation::Vertical, 0);
        backdrop.set_hexpand(true);
        backdrop.set_vexpand(true);
        overlay.set_child(Some(&backdrop));
        let content = gtk::Stack::new();
        content.set_hhomogeneous(false);
        content.set_vhomogeneous(false);
        content.add_css_class("shell-panel");
        content.set_overflow(gtk::Overflow::Hidden);
        let revealer = gtk::Revealer::new();
        revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
        revealer.set_transition_duration(120);
        // The fillets are overlays so they paint above the panel's shadow.
        let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let gutters = [
            gtk::Box::new(gtk::Orientation::Horizontal, 0),
            gtk::Box::new(gtk::Orientation::Horizontal, 0),
        ];
        body.append(&gutters[0]);
        body.append(&content);
        body.append(&gutters[1]);
        let panel = gtk::Overlay::new();
        panel.set_child(Some(&body));
        let fillets = [fillet(false, FILLET), fillet(true, FILLET)];
        for (gutter, (corner, align)) in gutters
            .iter()
            .zip(fillets.iter().zip([gtk::Align::Start, gtk::Align::End]))
        {
            gutter.set_width_request(FILLET);
            corner.set_halign(align);
            panel.add_overlay(corner);
        }
        panel.set_margin_start(SHADOW);
        panel.set_margin_end(SHADOW);
        panel.set_margin_bottom(SHADOW);
        revealer.set_child(Some(&panel));
        let [left, right] = fillets;
        let [gutter_left, gutter_right] = gutters;
        let corners = [
            left.upcast(),
            right.upcast(),
            gutter_left.upcast(),
            gutter_right.upcast(),
        ];
        // The revealer does not clip its sliding child here, so the panel would be
        // drawn over the bar while it animates; this box cuts it at the bar's edge.
        let clip = gtk::Box::new(gtk::Orientation::Vertical, 0);
        clip.set_overflow(gtk::Overflow::Hidden);
        clip.set_halign(gtk::Align::Center);
        clip.set_valign(gtk::Align::Start);
        clip.append(&revealer);
        overlay.add_overlay(&clip);
        window.set_child(Some(&overlay));
        let this = Rc::new(Self {
            window,
            content,
            revealer,
            clip,
            corners,
            page: RefCell::new(String::new()),
            closing: Cell::new(false),
            anchors: RefCell::default(),
        });
        let weak = Rc::downgrade(&this);
        this.revealer
            .connect_child_revealed_notify(move |revealer| {
                if let Some(s) = weak.upgrade()
                    && !revealer.is_child_revealed()
                    && s.closing.replace(false)
                {
                    s.window.set_visible(false);
                }
            });
        let weak = Rc::downgrade(&this);
        let click = gtk::GestureClick::new();
        click.connect_released(move |_, _, _, _| {
            if let Some(s) = weak.upgrade() {
                s.hide();
            }
        });
        backdrop.add_controller(click);
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
    /// Folds the panel back up under the bar, then hides the window. The power
    /// chooser, and a panel still sliding in, close at once.
    pub fn hide(self: &Rc<Self>) {
        self.window.set_keyboard_mode(KeyboardMode::None);
        self.page.borrow_mut().clear();
        if self.window.has_css_class("power-overlay") || !self.revealer.is_child_revealed() {
            self.closing.set(false);
            self.window.set_visible(false);
            // Collapse without animation so the next open slides in again.
            self.revealer
                .set_transition_type(gtk::RevealerTransitionType::None);
        } else {
            self.closing.set(true);
            self.revealer
                .set_transition_type(gtk::RevealerTransitionType::SlideDown);
            self.revealer.set_transition_duration(160);
        }
        self.revealer.set_reveal_child(false);
    }
    pub fn show(self: &Rc<Self>, page: &str, monitor: Option<&gtk::gdk::Monitor>) {
        if let Some(monitor) = monitor
            && self.window.monitor().as_ref() != Some(monitor)
        {
            self.window.set_visible(false);
            self.window.set_monitor(Some(monitor));
        }
        *self.page.borrow_mut() = page.into();
        self.closing.set(false);
        let power = page == "power";
        self.clip.set_halign(gtk::Align::Center);
        self.clip.set_margin_start(0);
        for corner in &self.corners {
            corner.set_visible(!power);
        }
        // The launcher surface starts under the bar, so nothing it renders (even a
        // stale frame) can cover the bar; the power chooser dims the whole screen.
        // The power tiles fly in from beyond the panel, which must not clip them.
        let overflow = if power {
            gtk::Overflow::Visible
        } else {
            gtk::Overflow::Hidden
        };
        for widget in [
            self.content.upcast_ref::<gtk::Widget>(),
            self.revealer.upcast_ref(),
            self.clip.upcast_ref(),
        ] {
            widget.set_overflow(overflow);
        }
        if power {
            self.window.add_css_class("power-overlay");
            self.window.set_margin(Edge::Top, 0);
            self.clip.set_valign(gtk::Align::Center);
            self.clip.set_margin_top(SHADOW);
        } else {
            self.window.remove_css_class("power-overlay");
            self.window.set_margin(Edge::Top, BAR_BOTTOM);
            self.clip.set_valign(gtk::Align::Start);
            self.clip.set_margin_top(0);
        }
        self.content.set_visible_child_name(page);
        let monitor = self.window.monitor();
        if let Some((button, monitor)) = self.anchors.borrow().iter().find_map(|anchor| {
            (anchor.page == page && monitor.as_ref().is_none_or(|m| m == &anchor.monitor))
                .then(|| anchor.button.upgrade().map(|b| (b, anchor.monitor.clone())))
                .flatten()
        }) {
            self.window.set_monitor(Some(&monitor));
            self.align_below(&button, &monitor, page == "battery");
        }
        self.window.set_keyboard_mode(KeyboardMode::OnDemand);
        self.window.present();
        let s = self.clone();
        glib::idle_add_local_once(move || {
            // The power tiles animate on their own.
            s.revealer.set_transition_type(if power {
                gtk::RevealerTransitionType::None
            } else {
                gtk::RevealerTransitionType::SlideDown
            });
            s.revealer.set_transition_duration(120);
            s.revealer.set_reveal_child(true);
        });
    }
    fn anchor(&self, page: &'static str, monitor: &gtk::gdk::Monitor, button: &gtk::Button) {
        let mut anchors = self.anchors.borrow_mut();
        anchors.retain(|anchor| anchor.button.upgrade().is_some());
        anchors.push(PanelAnchor {
            page,
            monitor: monitor.clone(),
            button: button.downgrade(),
        });
    }
    /// Anchor the panel to its button, excluding the transparent shadow gutters.
    fn align_below(&self, button: &gtk::Button, monitor: &gtk::gdk::Monitor, right_aligned: bool) {
        let Some(window) = button.root().and_downcast::<gtk::ApplicationWindow>() else {
            return;
        };
        let Some(bounds) = button.compute_bounds(&window) else {
            return;
        };
        let (_, width, _, _) = self.clip.measure(gtk::Orientation::Horizontal, -1);
        let left = if right_aligned {
            let right = window.margin(Edge::Left) + (bounds.x() + bounds.width()) as i32;
            right - width + SHADOW + FILLET
        } else {
            let centre = window.margin(Edge::Left) + (bounds.x() + bounds.width() / 2.) as i32;
            centre - width / 2
        };
        let left = left.clamp(0, (monitor.geometry().width() - width).max(0));
        self.clip.set_halign(gtk::Align::Start);
        self.clip.set_margin_start(left);
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
    notifications: Indicator,
    tray: Indicator,
    record: Indicator,
    wifi: Indicator,
    bluetooth: Indicator,
    sound: Indicator,
    brightness: Indicator,
    battery: Indicator,
    clock: gtk::Label,
    workspaces: gtk::Box,
    workspace_key: String,
}
pub struct Bars {
    app: gtk::Application,
    bars: RefCell<Vec<Bar>>,
    open: Open,
    surface: Rc<Surface>,
    /// Live notifications and do not disturb, for bars created later.
    notices: Cell<(usize, bool)>,
    tray_count: Cell<usize>,
    /// When the screen recording in progress started.
    recording: Cell<Option<Instant>>,
}
impl Bars {
    pub fn new(app: &gtk::Application, open: Open, surface: Rc<Surface>) -> Rc<Self> {
        let this = Rc::new(Self {
            app: app.clone(),
            bars: RefCell::new(vec![]),
            open,
            surface,
            notices: Cell::new((0, false)),
            tray_count: Cell::new(0),
            recording: Cell::new(None),
        });
        this.sync();
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
                false
            }
        });
        for monitor in monitors {
            if !bars.iter().any(|b| b.monitor == monitor) {
                bars.push(self.create(&monitor));
            }
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
        // A little narrower than the screen and centred, hanging from its top edge;
        // the margins leave room for the fillets on each side.
        let side =
            ((f64::from(monitor.geometry().width()) * BAR_INSET).round() as i32).max(BAR_FILLET);
        window.set_margin(Edge::Left, side - BAR_FILLET);
        window.set_margin(Edge::Right, side - BAR_FILLET);
        window.auto_exclusive_zone_enable();
        let center = gtk::CenterBox::new();
        center.add_css_class("bar-content");
        let left = hbox(4);
        let brand = self.button(monitor, "nexus", "Appearance", "appearance");
        brand.add_css_class("bar-brand");
        left.append(&brand);
        let workspaces = hbox(2);
        left.append(&workspaces);
        center.set_start_widget(Some(&left));
        let launcher = self.button(monitor, "", "Search applications · Super+Space", "launcher");
        launcher.add_css_class("bar-search");
        let middle = hbox(10);
        middle.append(&gtk::Image::from_icon_name("system-search-symbolic"));
        let clock = label("", "bar-clock");
        middle.append(&clock);
        middle.append(&label("Search", "bar-hint"));
        launcher.set_child(Some(&middle));
        // The clock stays at the centre whatever shows on either side of it.
        let middle_group = gtk::CenterBox::new();
        middle_group.set_center_widget(Some(&launcher));
        let captures = hbox(2);
        captures.set_margin_end(2);
        let capture = self.indicator(
            monitor,
            "camera-photo-symbolic",
            "Screenshot · Super+Shift+S",
            "",
        );
        capture
            .button
            .connect_clicked(|_| run_nexus(&["screenshot", "--save"]));
        captures.append(&capture.button);
        let record = self.indicator(monitor, "media-record-symbolic", "", "");
        record
            .button
            .connect_clicked(|_| run_nexus(&["screenshot", "--record"]));
        show_recording(&record, self.recording.get());
        captures.append(&record.button);
        middle_group.set_start_widget(Some(&captures));
        let tray = self.indicator(
            monitor,
            "application-x-executable-symbolic",
            "Tray applications",
            "tray",
        );
        tray.button.set_visible(self.tray_count.get() > 0);
        self.surface.anchor("tray", monitor, &tray.button);
        tray.button.set_margin_start(2);
        middle_group.set_end_widget(Some(&tray.button));
        center.set_center_widget(Some(&middle_group));
        let right = hbox(0);
        let notifications = self.indicator(
            monitor,
            "preferences-system-notifications-symbolic",
            "Notifications",
            "notifications",
        );
        let wifi = self.indicator(monitor, "network-wireless-symbolic", "Wi-Fi", "wifi");
        let bluetooth = self.indicator(monitor, "bluetooth-symbolic", "Bluetooth", "bluetooth");
        let sound = self.indicator(monitor, "audio-volume-high-symbolic", "Sound", "sound");
        let brightness = self.indicator(
            monitor,
            "display-brightness-symbolic",
            "Brightness",
            "display",
        );
        let battery = self.indicator(monitor, "battery-symbolic", "Battery", "battery");
        self.surface.anchor("battery", monitor, &battery.button);
        let power = self.indicator(
            monitor,
            "system-shutdown-symbolic",
            "Sleep, restart or shut down",
            "power",
        );
        power.button.add_css_class("bar-power");
        for b in [
            &notifications,
            &wifi,
            &bluetooth,
            &sound,
            &brightness,
            &battery,
            &power,
        ] {
            right.append(&b.button);
        }
        center.set_end_widget(Some(&right));
        center.add_css_class("bar-body");
        center.set_hexpand(true);
        center.set_margin_start(BAR_FILLET);
        center.set_margin_end(BAR_FILLET);
        let shape = gtk::Overlay::new();
        shape.set_child(Some(&center));
        for (right, align) in [(false, gtk::Align::Start), (true, gtk::Align::End)] {
            let corner = fillet(right, BAR_FILLET);
            corner.set_halign(align);
            shape.add_overlay(&corner);
        }
        window.set_child(Some(&shape));
        window.present();
        let (count, quiet) = self.notices.get();
        show_notices(&notifications, count, quiet);
        Bar {
            monitor: monitor.clone(),
            window,
            notifications,
            tray,
            record,
            wifi,
            bluetooth,
            sound,
            brightness,
            battery,
            clock,
            workspaces,
            workspace_key: String::new(),
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
        let value = label("", "bar-value");
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
        let time = glib::DateTime::now_local()
            .ok()
            .map(|t| {
                let days = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
                let months = [
                    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov",
                    "Dec",
                ];
                format!(
                    "{} {:02} {}  ·  {:02}:{:02}",
                    days[(t.day_of_week() - 1) as usize],
                    t.day_of_month(),
                    months[(t.month() - 1) as usize],
                    t.hour(),
                    t.minute()
                )
            })
            .unwrap_or_default();
        let battery = battery();
        for bar in self.bars.borrow().iter() {
            bar.clock.set_text(&time);
            let network = s.networks.iter().find(|n| n.connected());
            bar.wifi.icon.set_icon_name(Some(match network {
                Some(n) => signal_icon(n.signal),
                None if s.wifi => "network-wireless-offline-symbolic",
                None => "network-wireless-disabled-symbolic",
            }));
            bar.wifi.button.set_tooltip_text(Some(
                s.wifi_error
                    .as_deref()
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
            bar.bluetooth
                .button
                .set_tooltip_text(Some(&format!("Bluetooth · {connected} connected devices")));
            bar.sound
                .icon
                .set_icon_name(Some(if s.muted || s.volume == Some(0) {
                    "audio-volume-muted-symbolic"
                } else if s.volume.unwrap_or(0) < 35 {
                    "audio-volume-low-symbolic"
                } else if s.volume.unwrap_or(0) < 70 {
                    "audio-volume-medium-symbolic"
                } else {
                    "audio-volume-high-symbolic"
                }));
            bar.sound.value.set_visible(!s.muted && s.volume.is_some());
            bar.sound
                .value
                .set_text(&s.volume.map(|v| format!("{v}%")).unwrap_or_default());
            bar.sound.button.set_tooltip_text(Some(if s.muted {
                "Sound · Muted"
            } else {
                "Sound settings"
            }));
            bar.brightness.value.set_visible(s.brightness.is_some());
            bar.brightness
                .value
                .set_text(&s.brightness.map(|v| format!("{v}%")).unwrap_or_default());
            bar.battery.button.set_visible(battery.is_some());
            if let Some((capacity, charging)) = battery {
                bar.battery.icon.set_icon_name(Some(&format!(
                    "battery-level-{}{}-symbolic",
                    (capacity / 10) * 10,
                    if charging { "-charging" } else { "" }
                )));
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
    pub fn workspaces(&self, workspaces: &serde_json::Value, monitors: &serde_json::Value) {
        let Some(workspaces) = workspaces.as_array() else {
            return;
        };
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
            let key = format!("{ids:?}:{active}");
            if key == bar.workspace_key {
                continue;
            }
            bar.workspace_key = key;
            while let Some(c) = bar.workspaces.first_child() {
                bar.workspaces.remove(&c);
            }
            for id in ids {
                let b = gtk::Button::with_label(&id.to_string());
                b.add_css_class("workspace");
                if id == active {
                    b.add_css_class("active");
                }
                b.connect_clicked(move |_| {
                    std::thread::spawn(move || {
                        let _ = nexus_control::backend::run(
                            "hyprctl",
                            &["dispatch", &format!("hl.dsp.focus({{ workspace = {id} }})")],
                        );
                    });
                });
                bar.workspaces.append(&b);
            }
        }
    }
}
/// Runs `nexus` with `args`; the request reaches this process through its socket.
fn run_nexus(args: &[&str]) {
    let exe = std::env::current_exe().unwrap_or_else(|_| "nexus".into());
    match std::process::Command::new(exe).args(args).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => eprintln!("Could not run nexus {}: {e}", args.join(" ")),
    }
}
fn show_recording(indicator: &Indicator, started: Option<Instant>) {
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
fn battery() -> Option<(u8, bool)> {
    *BATTERY.lock().unwrap()
}
/// Reading `capacity` asks the embedded controller through ACPI, which takes
/// about 100 ms on some laptops: it must never block the main loop.
pub fn read_battery() {
    *BATTERY.lock().unwrap() = query_battery();
}
fn query_battery() -> Option<(u8, bool)> {
    for entry in std::fs::read_dir("/sys/class/power_supply").ok()?.flatten() {
        let p = entry.path();
        if std::fs::read_to_string(p.join("type")).ok()?.trim() != "Battery" {
            continue;
        }
        let n = std::fs::read_to_string(p.join("capacity"))
            .ok()?
            .trim()
            .parse()
            .ok()?;
        let charging = std::fs::read_to_string(p.join("status"))
            .unwrap_or_default()
            .trim()
            == "Charging";
        return Some((n, charging));
    }
    None
}
