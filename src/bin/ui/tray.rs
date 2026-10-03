use gtk::{gio, glib, prelude::*};
use nexus_control::tray::{self, Host, Item, MenuEntry};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

pub struct Tray {
    pub widget: gtk::Box,
    rows: gtk::Box,
    empty: gtk::Label,
    host: RefCell<Option<Rc<Host>>>,
    items: RefCell<Vec<Item>>,
    dismiss: Rc<dyn Fn()>,
}
impl Tray {
    pub fn new(dismiss: Rc<dyn Fn()>) -> Rc<Self> {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 12);
        widget.add_css_class("tray-panel");
        let rows = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .max_content_width(420)
            .propagate_natural_width(true)
            .propagate_natural_height(true)
            .child(&rows)
            .build();
        widget.append(&scroll);
        let empty = super::super::label("No tray applications", "dim-label");
        widget.append(&empty);
        Rc::new(Self {
            widget,
            rows,
            empty,
            host: RefCell::default(),
            items: RefCell::default(),
            dismiss,
        })
    }
    pub fn start(self: &Rc<Self>, changed: impl Fn(usize) + 'static) {
        let weak = Rc::downgrade(self);
        match Host::start(move |items| {
            if let Some(this) = weak.upgrade() {
                changed(items.len());
                this.update(items);
            }
        }) {
            Ok(host) => *self.host.borrow_mut() = Some(host),
            Err(e) => {
                self.empty.set_text("Tray unavailable: no session bus");
                eprintln!("Tray: {e}");
            }
        }
    }
    fn update(&self, items: Vec<Item>) {
        if *self.items.borrow() == items {
            return;
        }
        self.empty.set_visible(items.is_empty());
        while let Some(child) = self.rows.first_child() {
            self.rows.remove(&child);
        }
        let Some(host) = self.host.borrow().clone() else {
            return;
        };
        for item in &items {
            let button = gtk::Button::new();
            button.add_css_class("tray-item");
            if item.attention {
                button.add_css_class("needs-attention");
            }
            button.set_tooltip_text(Some(&item.title));
            button.update_property(&[gtk::accessible::Property::Label(&item.title)]);
            button.set_child(Some(&icon(item)));
            let (bus, app) = (host.bus.clone(), item.clone());
            let dismiss = self.dismiss.clone();
            button.connect_clicked(move |button| {
                if app.menu_only {
                    popup(button, &bus, &app);
                } else {
                    let (bus, app, dismiss) = (bus.clone(), app.clone(), dismiss.clone());
                    glib::spawn_future_local(async move {
                        if let Err(e) = tray::call(
                            &bus,
                            &app.address,
                            tray::ITEM,
                            "Activate",
                            (0i32, 0i32).to_variant(),
                        )
                        .await
                        {
                            eprintln!("Tray activation: {e}");
                        } else {
                            dismiss();
                        }
                    });
                }
            });
            let gesture = gtk::GestureClick::new();
            gesture.set_button(3);
            let (bus, app, weak) = (host.bus.clone(), item.clone(), button.downgrade());
            gesture.connect_pressed(move |g, _, _, _| {
                g.set_state(gtk::EventSequenceState::Claimed);
                if let Some(button) = weak.upgrade() {
                    popup(&button, &bus, &app);
                }
            });
            button.add_controller(gesture);
            let keys = gtk::EventControllerKey::new();
            let (bus, app, weak) = (host.bus.clone(), item.clone(), button.downgrade());
            keys.connect_key_pressed(move |_, key, _, modifiers| {
                if key == gtk::gdk::Key::Menu
                    || (key == gtk::gdk::Key::F10
                        && modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK))
                {
                    if let Some(button) = weak.upgrade() {
                        popup(&button, &bus, &app);
                    }
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
            button.add_controller(keys);
            self.rows.append(&button);
        }
        *self.items.borrow_mut() = items;
    }
}
fn icon(item: &Item) -> gtk::Image {
    let image = gtk::Image::new();
    image.set_pixel_size(24);
    if !item.icon.is_empty() {
        if std::path::Path::new(&item.icon).is_absolute() {
            image.set_from_file(Some(&item.icon));
        } else {
            if !item.theme_path.is_empty() {
                let theme = gtk::IconTheme::for_display(&gtk::gdk::Display::default().unwrap());
                if !theme
                    .search_path()
                    .iter()
                    .any(|p| p == std::path::Path::new(&item.theme_path))
                {
                    theme.add_search_path(&item.theme_path);
                }
            }
            let theme = gtk::IconTheme::for_display(&gtk::gdk::Display::default().unwrap());
            if theme.has_icon(&item.icon) {
                image.set_icon_name(Some(&item.icon));
            }
        }
    }
    if image.storage_type() == gtk::ImageType::Empty {
        if let Some((w, h, data)) = &item.pixels {
            let texture = gtk::gdk::MemoryTexture::new(
                *w,
                *h,
                gtk::gdk::MemoryFormat::R8g8b8a8,
                &glib::Bytes::from_owned(data.clone()),
                *w as usize * 4,
            );
            image.set_paintable(Some(&texture));
        } else {
            image.set_icon_name(Some("application-x-executable-symbolic"));
        }
    }
    image
}
fn popup(button: &gtk::Button, bus: &gio::DBusConnection, item: &Item) {
    if item.menu.is_none() {
        let (bus, item) = (bus.clone(), item.clone());
        glib::spawn_future_local(async move {
            if let Err(e) = tray::call(
                &bus,
                &item.address,
                tray::ITEM,
                "ContextMenu",
                (0i32, 0i32).to_variant(),
            )
            .await
            {
                eprintln!("Tray context menu: {e}");
            }
        });
        return;
    }
    let popover = gtk::Popover::new();
    popover.add_css_class("tray-menu");
    popover.set_has_arrow(false);
    popover.set_position(gtk::PositionType::Right);
    popover.set_parent(button);
    let rows = gtk::Box::new(gtk::Orientation::Vertical, 2);
    rows.append(&gtk::Label::new(Some("Loading…")));
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .max_content_height(480)
        .propagate_natural_height(true)
        .min_content_width(200)
        .child(&rows)
        .build();
    popover.set_child(Some(&scroll));
    // Detach before the row is destroyed by an item update or application exit.
    let weak = popover.downgrade();
    let unmap = button.connect_unmap(move |_| {
        if let Some(p) = weak.upgrade() {
            p.popdown();
        }
    });
    let weak_button = button.downgrade();
    let unmap = RefCell::new(Some(unmap));
    popover.connect_closed(move |p| {
        if let Some(button) = weak_button.upgrade()
            && let Some(id) = unmap.borrow_mut().take()
        {
            button.disconnect(id);
        }
        if p.parent().is_some() {
            p.unparent();
        }
    });
    let refresh: Rc<dyn Fn()> = {
        let (bus, item, rows, weak) =
            (bus.clone(), item.clone(), rows.clone(), popover.downgrade());
        let initial = Cell::new(true);
        let generation = Rc::new(Cell::new(0u64));
        Rc::new(move || {
            let about_to_show = initial.replace(false);
            let generation = generation.clone();
            let serial = generation.get() + 1;
            generation.set(serial);
            let (bus, item, rows, weak) = (bus.clone(), item.clone(), rows.clone(), weak.clone());
            glib::spawn_future_local(async move {
                let result = tray::menu(&bus, &item, about_to_show).await;
                if generation.get() != serial {
                    return;
                }
                let Some(p) = weak.upgrade().filter(|p| p.is_visible()) else {
                    return;
                };
                while let Some(child) = rows.first_child() {
                    rows.remove(&child);
                }
                match result {
                    Ok(menu) => {
                        let address = format!(
                            "{}{}",
                            tray::endpoint(&item.address).unwrap().0,
                            item.menu.as_ref().unwrap()
                        );
                        menu_rows(&rows, &menu.children, &bus, &address, &p);
                        if rows.first_child().is_none() {
                            rows.append(&gtk::Label::new(Some("No available actions")));
                        }
                    }
                    Err(e) => {
                        rows.append(&gtk::Label::new(Some("Application menu unavailable")));
                        eprintln!("Tray menu: {e}");
                    }
                }
            });
        })
    };
    let update = refresh.clone();
    let subscription = bus.subscribe_to_signal(
        Some(tray::endpoint(&item.address).unwrap().0),
        Some(tray::MENU),
        None,
        item.menu.as_deref(),
        None,
        gio::DBusSignalFlags::NONE,
        move |signal| {
            if ["LayoutUpdated", "ItemsPropertiesUpdated"].contains(&signal.signal_name) {
                update();
            }
        },
    );
    let subscription = RefCell::new(Some(subscription));
    popover.connect_closed(move |_| {
        subscription.borrow_mut().take();
    });
    popover.popup();
    refresh();
}
fn menu_rows(
    rows: &gtk::Box,
    entries: &[MenuEntry],
    bus: &gio::DBusConnection,
    address: &str,
    popover: &gtk::Popover,
) {
    for entry in entries.iter().filter(|e| e.visible) {
        if entry.separator {
            rows.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            continue;
        }
        let text = if let Some(on) = entry.toggle {
            format!("{} {}", if on { "✓" } else { "  " }, entry.label)
        } else {
            entry.label.clone()
        };
        if entry.submenu || !entry.children.is_empty() {
            let expander = gtk::Expander::builder()
                .label(&text)
                .use_underline(true)
                .build();
            expander.set_sensitive(entry.enabled);
            let children = gtk::Box::new(gtk::Orientation::Vertical, 2);
            children.set_margin_start(12);
            menu_rows(&children, &entry.children, bus, address, popover);
            let (bus, address, id, weak) = (
                bus.clone(),
                address.to_string(),
                entry.id,
                popover.downgrade(),
            );
            let child_rows = children.clone();
            expander.connect_expanded_notify(move |expander| {
                if !expander.is_expanded() {
                    return;
                }
                let (bus, address, weak, rows) = (
                    bus.clone(),
                    address.clone(),
                    weak.clone(),
                    child_rows.clone(),
                );
                glib::spawn_future_local(async move {
                    let _ = tray::call(
                        &bus,
                        &address,
                        tray::MENU,
                        "AboutToShow",
                        (id,).to_variant(),
                    )
                    .await;
                    if let Ok(reply) = tray::call(
                        &bus,
                        &address,
                        tray::MENU,
                        "GetLayout",
                        (id, -1i32, Vec::<String>::new()).to_variant(),
                    )
                    .await
                        && let Some(menu) = tray::parse_menu(&reply.child_value(1), 0)
                        && let Some(p) = weak.upgrade().filter(|p| p.is_visible())
                    {
                        while let Some(child) = rows.first_child() {
                            rows.remove(&child);
                        }
                        menu_rows(&rows, &menu.children, &bus, &address, &p);
                    }
                });
            });
            expander.set_child(Some(&children));
            rows.append(&expander);
        } else {
            let button = gtk::Button::with_mnemonic(&text);
            button.set_sensitive(entry.enabled);
            if let Some(label) = button.child().and_downcast::<gtk::Label>() {
                label.set_xalign(0.);
            }
            let (bus, address, id, weak) = (
                bus.clone(),
                address.to_string(),
                entry.id,
                popover.downgrade(),
            );
            button.connect_clicked(move |_| {
                if let Some(p) = weak.upgrade() {
                    p.popdown();
                }
                let (bus, address) = (bus.clone(), address.clone());
                glib::spawn_future_local(async move {
                    if let Err(e) = tray::call(
                        &bus,
                        &address,
                        tray::MENU,
                        "Event",
                        (id, "clicked", 0i32.to_variant(), 0u32).to_variant(),
                    )
                    .await
                    {
                        eprintln!("Tray menu action: {e}");
                    }
                });
            });
            rows.append(&button);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn descendants(widget: &impl IsA<gtk::Widget>) -> Vec<gtk::Widget> {
        let mut result = vec![];
        let mut child = widget.first_child();
        while let Some(w) = child {
            result.extend(descendants(&w));
            child = w.next_sibling();
            result.push(w);
        }
        result
    }
    async fn until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "tray menu did not update");
            glib::timeout_future(Duration::from_millis(10)).await;
        }
    }
    #[test]
    #[ignore = "requires GTK display and private D-Bus"]
    fn tray_menu_opens_activates_and_detaches() {
        gtk::init().unwrap();
        glib::MainContext::default().block_on(async {
            let bus = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>).unwrap();
            let xml = r#"<node><interface name="com.canonical.dbusmenu">
                <method name="AboutToShow"><arg type="i" direction="in"/><arg type="b" direction="out"/></method>
                <method name="GetLayout"><arg type="i" direction="in"/><arg type="i" direction="in"/><arg type="as" direction="in"/><arg type="u" direction="out"/><arg type="(ia{sv}av)" direction="out"/></method>
                <method name="Event"><arg type="i" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="in"/><arg type="u" direction="in"/></method>
                </interface></node>"#;
            let info = gio::DBusNodeInfo::for_xml(xml).unwrap().lookup_interface(tray::MENU).unwrap();
            let invoked = Rc::new(Cell::new(false));
            let event = invoked.clone();
            let registration = bus.register_object("/Menu", &info).method_call(move |_,_,_,_,method,args,invocation| {
                match method {
                    "AboutToShow" => invocation.return_value(Some(&(false,).to_variant())),
                    "GetLayout" => {
                        let props = std::collections::HashMap::from([("label", "Open test app".to_variant())]);
                        let child = (7i32,props,Vec::<glib::Variant>::new()).to_variant();
                        let root = (0i32,std::collections::HashMap::<String,glib::Variant>::new(),vec![child]).to_variant();
                        invocation.return_value(Some(&glib::Variant::tuple_from_iter([1u32.to_variant(),root])));
                    },
                    "Event" => { assert_eq!(args.child_value(0).get::<i32>(),Some(7)); event.set(true); invocation.return_value(None); },
                    _ => unreachable!(),
                }
            }).build().unwrap();
            let item = Item { address:format!("{}/StatusNotifierItem",bus.unique_name().unwrap()), title:"Test app".into(),icon:String::new(),theme_path:String::new(),pixels:Some((1,1,vec![20,30,40,255])),menu:Some("/Menu".into()),menu_only:false,attention:false };
            let button = gtk::Button::new();
            button.set_child(Some(&icon(&item)));
            let window = gtk::Window::builder().default_width(320).default_height(200).child(&button).build();
            window.present();
            until(|| button.is_mapped()).await;
            popup(&button,&bus,&item);
            until(|| descendants(&button).iter().any(|w| w.downcast_ref::<gtk::Button>().is_some_and(|b| b.label().as_deref()==Some("Open test app")))).await;
            let action = descendants(&button).into_iter().find_map(|w| w.downcast::<gtk::Button>().ok().filter(|b| b.label().as_deref()==Some("Open test app"))).unwrap();
            action.emit_clicked();
            until(|| invoked.get()).await;
            until(|| !descendants(&button).iter().any(|w| w.is::<gtk::Popover>())).await;
            // Removing a tray row while its menu is open must also detach the popup.
            popup(&button,&bus,&item);
            window.set_child(None::<&gtk::Widget>);
            until(|| !descendants(&button).iter().any(|w| w.is::<gtk::Popover>())).await;
            window.close();
            bus.unregister_object(registration).unwrap();
        });
    }
}
