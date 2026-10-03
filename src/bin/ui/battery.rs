use gtk::{gio, glib, prelude::*};
use nexus_control::power::{self, Profiles};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

pub struct Battery {
    pub widget: gtk::Box,
    buttons: Vec<gtk::Button>,
    estimate: gtk::Label,
    status: gtk::Label,
    bus: RefCell<Option<gio::DBusConnection>>,
    profiles: RefCell<Option<Profiles>>,
    refreshing: Cell<bool>,
    changing: Cell<bool>,
    generation: Cell<u64>,
}

impl Battery {
    pub fn new() -> Rc<Self> {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 12);
        widget.add_css_class("battery-panel");
        widget.set_width_request(280);
        widget.append(&super::super::label("Battery", "section-title"));
        let estimate = super::super::label("Reading battery…", "dim-label");
        widget.append(&estimate);
        let modes = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let mut buttons = Vec::new();
        for (_, name, icon) in power::MODES {
            let button = gtk::Button::new();
            button.add_css_class("power-profile");
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            row.append(&gtk::Image::from_icon_name(icon));
            let name = super::super::label(name, "profile-name");
            name.set_hexpand(true);
            row.append(&name);
            let check = gtk::Image::from_icon_name("object-select-symbolic");
            check.add_css_class("profile-check");
            row.append(&check);
            button.set_child(Some(&row));
            button.set_sensitive(false);
            modes.append(&button);
            buttons.push(button);
        }
        widget.append(&modes);
        let status = super::super::label("", "dim-label");
        status.set_wrap(true);
        status.set_max_width_chars(34);
        status.set_visible(false);
        widget.append(&status);
        let this = Rc::new(Self {
            widget,
            buttons,
            estimate,
            status,
            bus: RefCell::default(),
            profiles: RefCell::default(),
            refreshing: Cell::new(false),
            changing: Cell::new(false),
            generation: Cell::new(0),
        });
        for (button, (mode, _, _)) in this.buttons.iter().zip(power::MODES) {
            let weak = Rc::downgrade(&this);
            button.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.select(mode);
                }
            });
        }
        let weak = Rc::downgrade(&this);
        this.widget.connect_map(move |_| {
            if let Some(this) = weak.upgrade() {
                this.refresh();
            }
        });
        let weak = Rc::downgrade(&this);
        glib::timeout_add_local(Duration::from_secs(3), move || {
            let Some(this) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if this.widget.is_mapped() {
                this.refresh();
            }
            glib::ControlFlow::Continue
        });
        this
    }

    fn message(&self, text: &str, error: bool) {
        self.status.set_text(text);
        self.status.set_tooltip_text(None);
        self.status.set_visible(!text.is_empty());
        if error {
            self.status.add_css_class("error");
        } else {
            self.status.remove_css_class("error");
        }
    }

    fn render(&self) {
        let profiles = self.profiles.borrow();
        for (button, (mode, _, _)) in self.buttons.iter().zip(power::MODES) {
            let active = profiles.as_ref().is_some_and(|p| p.active == mode);
            if active {
                button.add_css_class("active");
            } else {
                button.remove_css_class("active");
            }
            button.set_sensitive(
                !self.changing.get() && profiles.as_ref().is_some_and(|p| p.supports(mode)),
            );
            button.set_tooltip_text(profiles.as_ref().and_then(|p| {
                if mode == "performance" && !p.inhibited.is_empty() {
                    Some(p.inhibited.as_str())
                } else if !p.supports(mode) {
                    Some("Not supported on this device")
                } else {
                    None
                }
            }));
        }
    }

    fn refresh(self: &Rc<Self>) {
        if self.changing.get() || self.refreshing.replace(true) {
            return;
        }
        let this = self.clone();
        let generation = this.generation.get();
        glib::spawn_future_local(async move {
            let cached = this.bus.borrow().clone();
            let bus = match cached {
                Some(bus) if !bus.is_closed() => Ok(bus),
                _ => gio::bus_get_future(gio::BusType::System).await,
            };
            match bus {
                Ok(bus) => {
                    *this.bus.borrow_mut() = Some(bus.clone());
                    let profiles = power::profiles(&bus).await;
                    let estimate = power::battery_status(&bus).await;
                    // An older poll must never overwrite a newly selected profile.
                    if this.generation.get() == generation {
                        match profiles {
                            Ok(p) => {
                                if this.profiles.borrow().is_none()
                                    || !this.status.has_css_class("error")
                                {
                                    this.message(
                                        if p.active == "performance" && !p.degraded.is_empty() {
                                            "Performance temporarily limited"
                                        } else {
                                            ""
                                        },
                                        false,
                                    );
                                }
                                *this.profiles.borrow_mut() = Some(p);
                            }
                            Err(e) => {
                                *this.profiles.borrow_mut() = None;
                                this.message("Power profiles unavailable", true);
                                this.status.set_tooltip_text(Some(&e.to_string()));
                            }
                        }
                        this.estimate.set_text(
                            &estimate.unwrap_or_else(|_| "Battery estimate unavailable".into()),
                        );
                        this.render();
                    }
                }
                Err(e) => {
                    *this.profiles.borrow_mut() = None;
                    this.message("Power service unavailable", true);
                    this.status.set_tooltip_text(Some(&e.to_string()));
                    this.estimate.set_text("Battery estimate unavailable");
                    this.render();
                }
            }
            this.refreshing.set(false);
        });
    }

    fn select(self: &Rc<Self>, mode: &'static str) {
        if self.changing.get() {
            return;
        }
        let (Some(bus), Some(profiles)) =
            (self.bus.borrow().clone(), self.profiles.borrow().clone())
        else {
            return;
        };
        if profiles.active == mode || !profiles.supports(mode) {
            return;
        }
        self.changing.set(true);
        self.generation.set(self.generation.get() + 1);
        self.render();
        self.message("Switching profile…", false);
        let this = self.clone();
        glib::spawn_future_local(async move {
            match power::set_profile(&bus, &profiles, mode).await {
                Ok(()) => {
                    // Read the actual state back; a successful call is not an optimistic selection.
                    match power::profiles(&bus).await {
                        Ok(p) => {
                            *this.profiles.borrow_mut() = Some(p);
                            this.message("", false);
                        }
                        Err(_) => {
                            *this.profiles.borrow_mut() = None;
                            this.message("Unable to read the active profile", true);
                        }
                    }
                }
                Err(e) => {
                    this.message("Unable to change power profile", true);
                    this.status.set_tooltip_text(Some(&e.to_string()));
                }
            }
            this.changing.set(false);
            this.render();
        });
    }
}
