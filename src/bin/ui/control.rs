use super::super::{hbox, label, vbox};
use gtk::{glib, prelude::*};
use std::{cell::Cell, rc::Rc, time::Duration};

/// Wi-Fi, Bluetooth, Sound and Display are in Control Center, under the bar.
pub const PAGES: [(&str, &str, &str); 4] = [
    (
        "appearance",
        "Appearance",
        "preferences-desktop-wallpaper-symbolic",
    ),
    ("phone", "Phone", "phone-symbolic"),
    ("monitor", "Monitor", "power-profile-performance-symbolic"),
    ("packages", "Packages", "system-software-install-symbolic"),
];

/// The settings pages live in a regular floating window, detached from the bar.
pub struct ControlCenter {
    pub window: gtk::ApplicationWindow,
    pub stack: gtk::Stack,
    pub power: gtk::Button,
    navs: Vec<(&'static str, gtk::Button)>,
    status: gtk::Label,
    status_generation: Cell<u64>,
}
fn nav_button(title: &str, icon: &str) -> gtk::Button {
    let b = gtk::Button::new();
    b.add_css_class("nav");
    let row = hbox(10);
    let image = gtk::Image::from_icon_name(icon);
    image.add_css_class("nav-icon");
    image.set_pixel_size(16);
    row.append(&image);
    let text = label(title, "nav-title");
    text.set_wrap(false);
    text.set_valign(gtk::Align::Center);
    row.append(&text);
    b.set_child(Some(&row));
    b
}
impl ControlCenter {
    pub fn new(app: &gtk::Application) -> Rc<Self> {
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Nexus")
            .default_width(900)
            .default_height(640)
            .decorated(false)
            .hide_on_close(true)
            .build();
        window.add_css_class("nexus");
        window.add_css_class("control-center");
        let root = hbox(0);
        let sidebar = vbox(2);
        sidebar.add_css_class("sidebar");
        sidebar.set_width_request(184);
        let stack = gtk::Stack::new();
        stack.set_hexpand(true);
        stack.set_vexpand(true);
        stack.set_hhomogeneous(true);
        stack.set_vhomogeneous(true);
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        stack.set_transition_duration(110);
        let head = hbox(0);
        head.add_css_class("sidebar-head");
        let title = label("Settings", "sidebar-title");
        title.set_wrap(false);
        title.set_hexpand(true);
        title.set_valign(gtk::Align::Center);
        head.append(&title);
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("control-close");
        close.add_css_class("flat");
        close.set_valign(gtk::Align::Center);
        close.set_tooltip_text(Some("Close"));
        close.update_property(&[gtk::accessible::Property::Label("Close")]);
        let weak_window = window.downgrade();
        close.connect_clicked(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.close();
            }
        });
        head.append(&close);
        sidebar.append(&head);
        let mut navs = vec![];
        for (id, title, icon) in &PAGES {
            let b = nav_button(title, icon);
            let st = stack.clone();
            let id: &'static str = id;
            b.connect_clicked(move |_| st.set_visible_child_name(id));
            sidebar.append(&b);
            navs.push((id, b));
        }
        let all = navs.clone();
        stack.connect_visible_child_name_notify(move |st| {
            let current = st.visible_child_name();
            for (id, b) in &all {
                if current.as_deref() == Some(*id) {
                    b.add_css_class("nav-active");
                } else {
                    b.remove_css_class("nav-active");
                }
            }
        });
        let spacer = vbox(0);
        spacer.set_vexpand(true);
        sidebar.append(&spacer);
        let power = nav_button("Power", "system-shutdown-symbolic");
        sidebar.append(&power);
        // Dragging the sidebar moves the undecorated window.
        let handle = gtk::WindowHandle::new();
        handle.set_child(Some(&sidebar));
        root.append(&handle);
        let content = vbox(0);
        content.set_hexpand(true);
        content.append(&stack);
        let status = label("", "status");
        status.add_css_class("error");
        status.set_visible(false);
        content.append(&status);
        root.append(&content);
        window.set_child(Some(&root));
        let key = gtk::EventControllerKey::new();
        let w = window.clone();
        key.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                w.close();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        window.add_controller(key);
        // Hyprland's dim_around rule makes the whole screen the window's input
        // area: a click on the dimmed backdrop arrives outside its bounds.
        let click = gtk::GestureClick::new();
        let w = window.clone();
        click.connect_pressed(move |_, _, x, y| {
            if x < 0. || y < 0. || x > f64::from(w.width()) || y > f64::from(w.height()) {
                w.close();
            }
        });
        window.add_controller(click);
        Rc::new(Self {
            window,
            stack,
            power,
            navs,
            status,
            status_generation: Cell::new(0),
        })
    }
    /// Opens `page`; the same shortcut again closes the focused window.
    /// `control` stands for whichever page was shown last.
    pub fn toggle(&self, page: &str) {
        let current = self.stack.visible_child_name();
        let page = match (page, current.as_deref()) {
            ("control", Some(current)) => current,
            ("control", None) => PAGES[0].0,
            _ => page,
        };
        let same = self.stack.visible_child_name().as_deref() == Some(page);
        if self.window.is_visible() && self.window.is_active() && same {
            self.window.close();
            return;
        }
        self.stack.set_visible_child_name(page);
        if self.window.is_visible() && !self.window.is_active() {
            // Remap so the window follows the user to the current workspace with focus.
            self.window.set_visible(false);
        }
        self.window.present();
        // Keyboard focus sits on the active entry so no second item looks selected.
        if let Some((_, b)) = self.navs.iter().find(|(id, _)| *id == page) {
            b.grab_focus();
        }
    }
    pub fn report(self: &Rc<Self>, result: Result<(), String>) {
        self.status_generation.set(self.status_generation.get() + 1);
        match result {
            Ok(()) => self.status.set_visible(false),
            Err(e) => {
                self.status.set_text(&e);
                self.status.set_visible(true);
                let generation = self.status_generation.get();
                let s = self.clone();
                glib::timeout_add_local_once(Duration::from_secs(6), move || {
                    if s.status_generation.get() == generation {
                        s.status.set_visible(false);
                    }
                });
            }
        }
    }
}
