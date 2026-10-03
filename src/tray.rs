//! StatusNotifier watcher/host and DBusMenu transport, on the GLib main context.
use gtk::{gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
pub const WATCHER: &str = "org.kde.StatusNotifierWatcher";
pub const PATH: &str = "/StatusNotifierWatcher";
pub const ITEM: &str = "org.kde.StatusNotifierItem";
pub const MENU: &str = "com.canonical.dbusmenu";
type Properties = std::collections::HashMap<String, glib::Variant>;
const XML: &str = r#"<node><interface name="org.kde.StatusNotifierWatcher">
<method name="RegisterStatusNotifierItem"><arg type="s" direction="in"/></method>
<method name="RegisterStatusNotifierHost"><arg type="s" direction="in"/></method>
<property name="RegisteredStatusNotifierItems" type="as" access="read"/>
<property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
<property name="ProtocolVersion" type="i" access="read"/>
<signal name="StatusNotifierItemRegistered"><arg type="s"/></signal>
<signal name="StatusNotifierItemUnregistered"><arg type="s"/></signal>
<signal name="StatusNotifierHostRegistered"/>
<signal name="StatusNotifierHostUnregistered"/>
</interface></node>"#;

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub address: String,
    pub title: String,
    pub icon: String,
    pub theme_path: String,
    pub pixels: Option<(i32, i32, Vec<u8>)>,
    pub menu: Option<String>,
    pub menu_only: bool,
    pub attention: bool,
}
/// The wire pixmaps are ARGB, most significant byte first (not native endian).
pub fn pixels(value: &glib::Variant) -> Option<(i32, i32, Vec<u8>)> {
    value
        .get::<Vec<(i32, i32, Vec<u8>)>>()?
        .into_iter()
        .filter(|(w, h, p)| {
            *w > 0 && *h > 0 && *w <= 512 && *h <= 512 && p.len() == (*w as usize * *h as usize * 4)
        })
        .min_by_key(|(w, h, _)| (w - 32).abs() + (h - 32).abs())
        .map(|(w, h, mut p)| {
            for pixel in p.as_chunks_mut::<4>().0 {
                pixel.rotate_left(1);
            }
            (w, h, p)
        })
}
pub fn endpoint(address: &str) -> Option<(&str, &str)> {
    let at = address.find('/')?;
    let (name, path) = address.split_at(at);
    if !gio::dbus_is_name(name) || !glib::Variant::is_object_path(path) {
        return None;
    }
    Some((name, path))
}
pub async fn call(
    bus: &gio::DBusConnection,
    address: &str,
    interface: &str,
    method: &str,
    args: glib::Variant,
) -> Result<glib::Variant, glib::Error> {
    let (name, path) = endpoint(address).ok_or_else(|| {
        glib::Error::new(gio::IOErrorEnum::InvalidArgument, "Invalid tray address")
    })?;
    bus.call_future(
        Some(name),
        path,
        interface,
        method,
        Some(&args),
        None,
        gio::DBusCallFlags::NONE,
        3000,
    )
    .await
}
pub struct Host {
    pub bus: gio::DBusConnection,
    registered: RefCell<BTreeMap<String, String>>,
    items: RefCell<BTreeMap<String, Item>>,
    generations: RefCell<BTreeMap<String, u64>>,
    serial: Cell<u64>,
    owner: Cell<Option<gio::OwnerId>>,
    registration: RefCell<Option<gio::RegistrationId>>,
    subscriptions: RefCell<Vec<gio::SignalSubscription>>,
    changed: Box<dyn Fn(Vec<Item>)>,
}
impl Host {
    pub fn start(changed: impl Fn(Vec<Item>) + 'static) -> Result<Rc<Self>, glib::Error> {
        let bus = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)?;
        Self::on_bus(bus, changed)
    }
    pub fn on_bus(
        bus: gio::DBusConnection,
        changed: impl Fn(Vec<Item>) + 'static,
    ) -> Result<Rc<Self>, glib::Error> {
        let this = Rc::new(Self {
            bus,
            registered: RefCell::default(),
            items: RefCell::default(),
            generations: RefCell::default(),
            serial: Cell::new(0),
            owner: Cell::new(None),
            registration: RefCell::default(),
            subscriptions: RefCell::default(),
            changed: Box::new(changed),
        });
        let weak = Rc::downgrade(&this);
        let prop = weak.clone();
        let info = gio::DBusNodeInfo::for_xml(XML)?
            .lookup_interface(WATCHER)
            .unwrap();
        let id = this
            .bus
            .register_object(PATH, &info)
            .method_call(move |_, sender, _, _, method, params, invocation| {
                let Some(host) = weak.upgrade() else {
                    return;
                };
                if method == "RegisterStatusNotifierHost" {
                    invocation.return_value(None);
                    return;
                }
                let Some((service,)) = params.get::<(String,)>() else {
                    invocation.return_dbus_error(
                        "org.freedesktop.DBus.Error.InvalidArgs",
                        "Expected an item address",
                    );
                    return;
                };
                let address = if service.starts_with('/') {
                    format!("{}{service}", sender.unwrap_or_default())
                } else {
                    format!("{service}/StatusNotifierItem")
                };
                let Some((name, _)) = endpoint(&address) else {
                    invocation.return_dbus_error(
                        "org.freedesktop.DBus.Error.InvalidArgs",
                        "Invalid item address",
                    );
                    return;
                };
                let name = name.to_owned();
                glib::spawn_future_local(async move {
                    let owner = host
                        .bus
                        .call_future(
                            Some("org.freedesktop.DBus"),
                            "/org/freedesktop/DBus",
                            "org.freedesktop.DBus",
                            "GetNameOwner",
                            Some(&(name,).to_variant()),
                            None,
                            gio::DBusCallFlags::NONE,
                            3000,
                        )
                        .await;
                    match owner.and_then(|v| {
                        v.get::<(String,)>().ok_or_else(|| {
                            glib::Error::new(gio::IOErrorEnum::InvalidData, "Invalid owner")
                        })
                    }) {
                        Ok((owner,)) => {
                            let new = host
                                .registered
                                .borrow_mut()
                                .insert(address.clone(), owner)
                                .is_none();
                            invocation.return_value(None);
                            if new {
                                host.emit("StatusNotifierItemRegistered", &address);
                            }
                        }
                        Err(e) => invocation.return_dbus_error(
                            "org.freedesktop.DBus.Error.InvalidArgs",
                            &e.to_string(),
                        ),
                    }
                });
            })
            .property(move |_, _, _, _, name| {
                let host = prop.upgrade().unwrap();
                match name {
                    "RegisteredStatusNotifierItems" => host
                        .registered
                        .borrow()
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .to_variant(),
                    "IsStatusNotifierHostRegistered" => true.to_variant(),
                    _ => 0i32.to_variant(),
                }
            })
            .build()?;
        *this.registration.borrow_mut() = Some(id);
        let weak = Rc::downgrade(&this);
        this.subscriptions
            .borrow_mut()
            .push(this.bus.subscribe_to_signal(
                Some(WATCHER),
                Some(WATCHER),
                None,
                Some(PATH),
                None,
                gio::DBusSignalFlags::NONE,
                move |signal| {
                    let Some(host) = weak.upgrade() else {
                        return;
                    };
                    if let Some((address,)) = signal.parameters.get::<(String,)>() {
                        if signal.signal_name == "StatusNotifierItemRegistered" {
                            host.refresh(&address);
                        }
                        if signal.signal_name == "StatusNotifierItemUnregistered" {
                            host.remove(&address);
                        }
                    }
                },
            ));
        let weak = Rc::downgrade(&this);
        this.subscriptions
            .borrow_mut()
            .push(this.bus.subscribe_to_signal(
                Some("org.freedesktop.DBus"),
                Some("org.freedesktop.DBus"),
                Some("NameOwnerChanged"),
                None,
                None,
                gio::DBusSignalFlags::NONE,
                move |signal| {
                    let Some(host) = weak.upgrade() else {
                        return;
                    };
                    let Some((name, old, new)) =
                        signal.parameters.get::<(String, String, String)>()
                    else {
                        return;
                    };
                    if !old.is_empty() {
                        let stale: Vec<_> = host
                            .registered
                            .borrow()
                            .iter()
                            .filter(|(a, o)| {
                                (name.starts_with(':') && **o == old)
                                    || endpoint(a).is_some_and(|(n, _)| n == name)
                            })
                            .map(|(a, _)| a.clone())
                            .collect();
                        for address in stale {
                            host.registered.borrow_mut().remove(&address);
                            host.emit("StatusNotifierItemUnregistered", &address);
                        }
                    }
                    if name == WATCHER {
                        host.generations.borrow_mut().clear();
                        host.items.borrow_mut().clear();
                        host.notify();
                        if !new.is_empty() {
                            host.discover();
                        }
                    }
                },
            ));
        // SNI uses NewIcon/NewStatus signals as well as PropertiesChanged.
        for interface in [ITEM, "org.freedesktop.DBus.Properties"] {
            let weak = Rc::downgrade(&this);
            this.subscriptions
                .borrow_mut()
                .push(this.bus.subscribe_to_signal(
                    None,
                    Some(interface),
                    None,
                    None,
                    None,
                    gio::DBusSignalFlags::NONE,
                    move |s| {
                        let Some(host) = weak.upgrade() else {
                            return;
                        };
                        let addresses: Vec<_> = host
                            .generations
                            .borrow()
                            .keys()
                            .filter(|a| endpoint(a).is_some_and(|(_, p)| p == s.object_path))
                            .cloned()
                            .collect();
                        for address in addresses {
                            host.refresh(&address);
                        }
                    },
                ));
        }
        let weak = Rc::downgrade(&this);
        let id = gio::bus_own_name_on_connection(
            &this.bus,
            WATCHER,
            gio::BusNameOwnerFlags::NONE,
            move |bus, _| {
                let _ = bus.emit_signal(None, PATH, WATCHER, "StatusNotifierHostRegistered", None);
                if let Some(host) = weak.upgrade() {
                    host.discover();
                }
            },
            |_, _| {},
        );
        this.owner.set(Some(id));
        this.discover();
        Ok(this)
    }
    fn emit(&self, signal: &str, address: &str) {
        let _ = self
            .bus
            .emit_signal(None, PATH, WATCHER, signal, Some(&(address,).to_variant()));
    }
    fn notify(&self) {
        (self.changed)(self.items.borrow().values().cloned().collect());
    }
    fn remove(&self, address: &str) {
        self.generations.borrow_mut().remove(address);
        self.items.borrow_mut().remove(address);
        self.notify();
    }
    fn discover(self: &Rc<Self>) {
        let host = self.clone();
        glib::spawn_future_local(async move {
            let address = format!("{WATCHER}{PATH}");
            let _ = call(
                &host.bus,
                &address,
                WATCHER,
                "RegisterStatusNotifierHost",
                (host.bus.unique_name().unwrap().as_str(),).to_variant(),
            )
            .await;
            if let Ok(v) = call(
                &host.bus,
                &address,
                "org.freedesktop.DBus.Properties",
                "Get",
                (WATCHER, "RegisteredStatusNotifierItems").to_variant(),
            )
            .await
                && let Some(items) = v
                    .child_value(0)
                    .as_variant()
                    .and_then(|v| v.get::<Vec<String>>())
            {
                for address in items {
                    host.refresh(&address);
                }
            }
        });
    }
    fn refresh(self: &Rc<Self>, address: &str) {
        if endpoint(address).is_none() {
            return;
        }
        let generation = self.serial.get().wrapping_add(1);
        self.serial.set(generation);
        self.generations
            .borrow_mut()
            .insert(address.into(), generation);
        let (host, address) = (self.clone(), address.to_owned());
        glib::spawn_future_local(async move {
            let result = call(
                &host.bus,
                &address,
                "org.freedesktop.DBus.Properties",
                "GetAll",
                (ITEM,).to_variant(),
            )
            .await;
            if host.generations.borrow().get(&address) != Some(&generation) {
                return;
            }
            if let Ok(v) = result
                && let Some((props,)) = v.get::<(Properties,)>()
            {
                let string = |key: &str| {
                    props
                        .get(key)
                        .and_then(|v| v.str())
                        .unwrap_or_default()
                        .to_string()
                };
                let status = string("Status");
                if status == "Passive" {
                    host.items.borrow_mut().remove(&address);
                } else {
                    let attention = status == "NeedsAttention";
                    let mut icon = string(if attention {
                        "AttentionIconName"
                    } else {
                        "IconName"
                    });
                    if icon.is_empty() {
                        icon = string("IconName");
                    }
                    let pixels = props
                        .get(if attention {
                            "AttentionIconPixmap"
                        } else {
                            "IconPixmap"
                        })
                        .and_then(pixels)
                        .or_else(|| props.get("IconPixmap").and_then(pixels));
                    let menu = string("Menu");
                    let mut title = string("Title");
                    if title.is_empty() {
                        title = string("Id");
                    }
                    if title.is_empty() {
                        title = "Application".into();
                    }
                    host.items.borrow_mut().insert(
                        address.clone(),
                        Item {
                            address,
                            title,
                            icon,
                            theme_path: string("IconThemePath"),
                            pixels,
                            menu: (!menu.is_empty() && menu != "/").then_some(menu),
                            menu_only: props
                                .get("ItemIsMenu")
                                .and_then(|v| v.get())
                                .unwrap_or(false),
                            attention,
                        },
                    );
                }
                host.notify();
            }
        });
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        if let Some(id) = self.owner.take() {
            gio::bus_unown_name(id);
        }
        if let Some(id) = self.registration.borrow_mut().take() {
            let _ = self.bus.unregister_object(id);
        }
    }
}

#[derive(Clone, Debug)]
pub struct MenuEntry {
    pub id: i32,
    pub label: String,
    pub enabled: bool,
    pub visible: bool,
    pub separator: bool,
    pub toggle: Option<bool>,
    pub submenu: bool,
    pub children: Vec<MenuEntry>,
}
pub fn parse_menu(value: &glib::Variant, depth: usize) -> Option<MenuEntry> {
    if depth > 12 {
        return None;
    }
    let (id, props, children) = value.get::<(i32, Properties, Vec<glib::Variant>)>()?;
    let string = |key| {
        props
            .get(key)
            .and_then(|v| v.str())
            .unwrap_or_default()
            .to_string()
    };
    Some(MenuEntry {
        id,
        label: string("label"),
        enabled: props.get("enabled").and_then(|v| v.get()).unwrap_or(true),
        visible: props.get("visible").and_then(|v| v.get()).unwrap_or(true),
        separator: string("type") == "separator",
        submenu: string("children-display") == "submenu",
        toggle: (!string("toggle-type").is_empty()).then(|| {
            props
                .get("toggle-state")
                .and_then(|v| v.get::<i32>())
                .unwrap_or(0)
                == 1
        }),
        children: children
            .iter()
            .take(256)
            .filter_map(|v| parse_menu(v, depth + 1))
            .collect(),
    })
}
pub async fn menu(
    bus: &gio::DBusConnection,
    item: &Item,
    about_to_show: bool,
) -> Result<MenuEntry, glib::Error> {
    let path = item.menu.as_ref().ok_or_else(|| {
        glib::Error::new(
            gio::IOErrorEnum::NotSupported,
            "This application does not publish a menu",
        )
    })?;
    let address = format!("{}{path}", endpoint(&item.address).unwrap().0);
    if about_to_show {
        let _ = call(bus, &address, MENU, "AboutToShow", (0i32,).to_variant()).await;
    }
    let reply = call(
        bus,
        &address,
        MENU,
        "GetLayout",
        (0i32, -1i32, Vec::<String>::new()).to_variant(),
    )
    .await?;
    parse_menu(&reply.child_value(1), 0)
        .ok_or_else(|| glib::Error::new(gio::IOErrorEnum::InvalidData, "Invalid application menu"))
}
