use super::super::{hbox, label};
use gtk::{glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use nexus_control::Snapshot;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

pub struct Surface {
    pub window: gtk::ApplicationWindow,
    pub content: gtk::Stack,
    pub revealer: gtk::Revealer,
    generation: Cell<u64>,
    pub page: RefCell<String>,
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
        window.set_keyboard_mode(KeyboardMode::Exclusive);
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
        revealer.set_halign(gtk::Align::Center);
        revealer.set_valign(gtk::Align::Start);
        revealer.set_margin_top(41);
        revealer.set_child(Some(&content));
        overlay.add_overlay(&revealer);
        window.set_child(Some(&overlay));
        let this = Rc::new(Self {
            window,
            content,
            revealer,
            generation: Cell::new(0),
            page: RefCell::new(String::new()),
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
    pub fn hide(self: &Rc<Self>) {
        self.generation.set(self.generation.get() + 1);
        let generation = self.generation.get();
        self.window.set_keyboard_mode(KeyboardMode::None);
        self.revealer.set_reveal_child(false);
        let s = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(130), move || {
            if s.generation.get() == generation {
                s.window.set_visible(false);
                s.page.borrow_mut().clear();
            }
        });
    }
    pub fn show(self: &Rc<Self>, page: &str, monitor: Option<&gtk::gdk::Monitor>) {
        self.generation.set(self.generation.get() + 1);
        if let Some(monitor) = monitor
            && self.window.monitor().as_ref() != Some(monitor)
        {
            self.window.set_visible(false);
            self.window.set_monitor(Some(monitor));
        }
        *self.page.borrow_mut() = page.into();
        let power = page == "power";
        if power {
            self.window.add_css_class("power-overlay");
            self.revealer.set_valign(gtk::Align::Center);
            self.revealer.set_margin_top(0);
        } else {
            self.window.remove_css_class("power-overlay");
            self.revealer.set_valign(gtk::Align::Start);
            self.revealer.set_margin_top(41);
        }
        self.content.set_visible_child_name(if page == "launcher" {
            "launcher"
        } else {
            "settings"
        });
        self.window.set_keyboard_mode(KeyboardMode::Exclusive);
        self.window.present();
        let s = self.clone();
        glib::idle_add_local_once(move || {
            s.revealer.set_reveal_child(true);
            if let Some(surface) = s.window.surface() {
                let top = if s.page.borrow().as_str() == "power" {
                    0
                } else {
                    42
                };
                let rect = gtk::cairo::RectangleInt::new(
                    0,
                    top,
                    surface.width(),
                    (surface.height() - top).max(1),
                );
                surface.set_input_region(&gtk::cairo::Region::create_rectangle(&rect));
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
}
impl Bars {
    pub fn new(app: &gtk::Application, open: Open) -> Rc<Self> {
        let this = Rc::new(Self {
            app: app.clone(),
            bars: RefCell::new(vec![]),
            open,
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
        window.set_margin(Edge::Top, 8);
        window.set_margin(Edge::Left, 12);
        window.set_margin(Edge::Right, 12);
        window.auto_exclusive_zone_enable();
        let center = gtk::CenterBox::new();
        center.set_height_request(34);
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
        center.set_center_widget(Some(&launcher));
        let right = hbox(0);
        let wifi = self.indicator(monitor, "network-wireless-symbolic", "Wi-Fi", "wifi");
        let bluetooth = self.indicator(monitor, "bluetooth-symbolic", "Bluetooth", "bluetooth");
        let sound = self.indicator(monitor, "audio-volume-high-symbolic", "Sound", "sound");
        let brightness = self.indicator(
            monitor,
            "display-brightness-symbolic",
            "Brightness",
            "display",
        );
        let battery = self.indicator(monitor, "battery-symbolic", "Battery", "power");
        let power = self.indicator(
            monitor,
            "system-shutdown-symbolic",
            "Sleep, restart or shut down",
            "power",
        );
        power.button.add_css_class("bar-power");
        for b in [&wifi, &bluetooth, &sound, &brightness, &battery, &power] {
            right.append(&b.button);
        }
        center.set_end_widget(Some(&right));
        window.set_child(Some(&center));
        window.present();
        Bar {
            monitor: monitor.clone(),
            window,
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
        let open = self.open.clone();
        let monitor = monitor.clone();
        b.connect_clicked(move |_| open(page, Some(&monitor)));
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
            let network = s
                .networks
                .iter()
                .find(|n| matches!(n.state.as_str(), "ready" | "online"));
            bar.wifi.icon.set_icon_name(Some(match network {
                Some(n) if n.signal >= 75 => "network-wireless-signal-excellent-symbolic",
                Some(n) if n.signal >= 50 => "network-wireless-signal-good-symbolic",
                Some(n) if n.signal >= 25 => "network-wireless-signal-ok-symbolic",
                Some(_) => "network-wireless-signal-weak-symbolic",
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
fn battery() -> Option<(u8, bool)> {
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
