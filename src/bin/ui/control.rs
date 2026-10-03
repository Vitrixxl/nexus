use super::super::{hbox, label, vbox};
use gtk::{glib, prelude::*};
use std::{cell::Cell, rc::Rc, time::Duration};

pub const PAGES: [(&str, &str, &str); 5] = [
    ("wifi", "Wi-Fi", "network-wireless-symbolic"),
    ("bluetooth", "Bluetooth", "bluetooth-symbolic"),
    ("sound", "Sound", "audio-volume-high-symbolic"),
    ("display", "Display", "display-brightness-symbolic"),
    (
        "appearance",
        "Appearance",
        "preferences-desktop-wallpaper-symbolic",
    ),
];
/// Sidebar groups, by index into `PAGES`.
const SECTIONS: [(&str, std::ops::Range<usize>); 2] = [("Connections", 0..2), ("System", 2..5)];

/// The settings pages live in a regular floating window, detached from the bar.
pub struct ControlCenter {
    pub window: gtk::ApplicationWindow,
    pub stack: gtk::Stack,
    pub power: gtk::Button,
    navs: Vec<(&'static str, gtk::Button)>,
    details: Vec<(&'static str, gtk::Label)>,
    status: gtk::Label,
    status_generation: Cell<u64>,
}
/// Sidebar entry: icon tile, title and a short trailing status.
fn nav_button(title: &str, icon: &str) -> (gtk::Button, gtk::Label) {
    let b = gtk::Button::new();
    b.add_css_class("nav");
    let row = hbox(14);
    let image = gtk::Image::from_icon_name(icon);
    image.add_css_class("nav-icon");
    image.set_pixel_size(18);
    row.append(&image);
    let text = label(title, "nav-title");
    text.set_wrap(false);
    text.set_hexpand(true);
    text.set_valign(gtk::Align::Center);
    row.append(&text);
    let detail = label("", "nav-detail");
    detail.set_wrap(false);
    detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
    detail.set_max_width_chars(11);
    detail.set_valign(gtk::Align::Center);
    row.append(&detail);
    b.set_child(Some(&row));
    (b, detail)
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
        let sidebar = vbox(4);
        sidebar.add_css_class("sidebar");
        sidebar.set_width_request(250);
        let stack = gtk::Stack::new();
        stack.set_hexpand(true);
        stack.set_vexpand(true);
        // Vertical carousel: pages below the current one slide up, pages above slide down.
        stack.set_hhomogeneous(true);
        stack.set_vhomogeneous(true);
        stack.set_transition_type(gtk::StackTransitionType::SlideUpDown);
        stack.set_transition_duration(220);
        let mut navs = vec![];
        let mut details = vec![];
        for (section, pages) in SECTIONS {
            sidebar.append(&label(section, "sidebar-section"));
            for (id, title, icon) in &PAGES[pages] {
                let (b, detail) = nav_button(title, icon);
                let st = stack.clone();
                let id: &'static str = id;
                b.connect_clicked(move |_| st.set_visible_child_name(id));
                sidebar.append(&b);
                navs.push((id, b));
                details.push((id, detail));
            }
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
        let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
        separator.set_margin_bottom(8);
        sidebar.append(&separator);
        let (power, _) = nav_button("Power", "system-shutdown-symbolic");
        power.add_css_class("power-nav");
        sidebar.append(&power);
        // Dragging the sidebar moves the undecorated window.
        let handle = gtk::WindowHandle::new();
        handle.set_child(Some(&sidebar));
        root.append(&handle);
        let content = vbox(0);
        content.set_hexpand(true);
        let window_actions = hbox(0);
        window_actions.set_halign(gtk::Align::End);
        window_actions.set_margin_top(8);
        window_actions.set_margin_end(8);
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("control-close");
        close.set_tooltip_text(Some("Close"));
        close.update_property(&[gtk::accessible::Property::Label("Close")]);
        let weak_window = window.downgrade();
        close.connect_clicked(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.close();
            }
        });
        window_actions.append(&close);
        content.append(&window_actions);
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
        Rc::new(Self {
            window,
            stack,
            power,
            navs,
            details,
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
    /// Short live status next to a sidebar entry, such as the Wi-Fi network.
    pub fn set_detail(&self, page: &str, text: &str) {
        if let Some((_, l)) = self.details.iter().find(|(id, _)| *id == page) {
            l.set_text(text);
            l.set_tooltip_text((!text.is_empty()).then_some(text));
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
