use gtk::{gio, glib, prelude::*};
use nexus_control::tray::{self, Host};
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
};

fn layout(
    id: i32,
    properties: &[(&str, glib::Variant)],
    children: Vec<glib::Variant>,
) -> glib::Variant {
    (
        id,
        properties
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect::<HashMap<_, _>>(),
        children,
    )
        .to_variant()
}
#[test]
fn validates_pixels_and_menu_properties() {
    let raw = vec![(1i32, 1i32, vec![128u8, 10, 20, 30])].to_variant();
    assert_eq!(tray::pixels(&raw), Some((1, 1, vec![10, 20, 30, 128])));
    assert!(tray::pixels(&vec![(i32::MAX, 1, vec![0u8])].to_variant()).is_none());
    assert!(tray::pixels(&vec![(2i32, 2i32, vec![0u8; 3])].to_variant()).is_none());
    assert!(tray::endpoint("bad name/a").is_none());
    assert!(tray::endpoint(":1.5/invalid-path").is_none());
    let root = layout(
        0,
        &[],
        vec![
            layout(
                1,
                &[
                    ("label", "_Show".to_variant()),
                    ("enabled", false.to_variant()),
                ],
                vec![],
            ),
            layout(2, &[("type", "separator".to_variant())], vec![]),
            layout(
                3,
                &[
                    ("toggle-type", "checkmark".to_variant()),
                    ("toggle-state", 1i32.to_variant()),
                    ("visible", false.to_variant()),
                ],
                vec![],
            ),
            layout(4, &[("children-display", "submenu".to_variant())], vec![]),
        ],
    );
    let parsed = tray::parse_menu(&root, 0).unwrap();
    assert!(!parsed.children[0].enabled);
    assert_eq!(parsed.children[0].label, "_Show");
    assert!(parsed.children[1].separator);
    assert_eq!(parsed.children[2].toggle, Some(true));
    assert!(!parsed.children[2].visible);
    assert!(parsed.children[3].submenu);
}
async fn connection() -> gio::DBusConnection {
    gio::DBusConnection::for_address_future(
        &std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap(),
        gio::DBusConnectionFlags::AUTHENTICATION_CLIENT
            | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
        None,
    )
    .await
    .unwrap()
}
async fn until(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !check() {
        assert!(Instant::now() < deadline, "D-Bus tray update timed out");
        glib::timeout_future(Duration::from_millis(10)).await;
    }
}
const ITEM_XML: &str = r#"<node><interface name="org.kde.StatusNotifierItem">
<property name="Title" type="s" access="read"/><property name="Id" type="s" access="read"/>
<property name="Status" type="s" access="read"/><property name="IconName" type="s" access="read"/>
<property name="IconPixmap" type="a(iiay)" access="read"/><property name="Menu" type="o" access="read"/>
<property name="ItemIsMenu" type="b" access="read"/>
<method name="Activate"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
<signal name="NewStatus"><arg type="s"/></signal><signal name="NewIcon"/>
</interface></node>"#;
const MENU_XML: &str = r#"<node><interface name="com.canonical.dbusmenu">
<method name="AboutToShow"><arg type="i" direction="in"/><arg type="b" direction="out"/></method>
<method name="GetLayout"><arg type="i" direction="in"/><arg type="i" direction="in"/><arg type="as" direction="in"/><arg type="u" direction="out"/><arg type="(ia{sv}av)" direction="out"/></method>
<method name="Event"><arg type="i" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="in"/><arg type="u" direction="in"/></method>
</interface></node>"#;
#[test]
#[ignore = "requires private D-Bus: dbus-run-session -- cargo test --test tray -- --ignored"]
fn status_notifier_and_menu_roundtrip() {
    let context = glib::MainContext::new();
    context
        .with_thread_default(|| {
            context.block_on(async {
                let bus = connection().await;
                let seen = Rc::new(RefCell::new(vec![]));
                let received = seen.clone();
                let _host =
                    Host::on_bus(bus.clone(), move |items| *received.borrow_mut() = items).unwrap();
                let acquired = Rc::new(RefCell::new(false));
                let flag = acquired.clone();
                let watch = gio::bus_watch_name_on_connection(
                    &bus,
                    tray::WATCHER,
                    gio::BusNameWatcherFlags::NONE,
                    move |_, _, _| *flag.borrow_mut() = true,
                    |_, _| {},
                );
                until(|| *acquired.borrow()).await;
                let app = connection().await;
                let status = Rc::new(RefCell::new("Active".to_string()));
                let state = status.clone();
                let events = Rc::new(RefCell::new(vec![]));
                let calls = events.clone();
                let info = gio::DBusNodeInfo::for_xml(ITEM_XML)
                    .unwrap()
                    .lookup_interface(tray::ITEM)
                    .unwrap();
                let _item = app
                    .register_object("/StatusNotifierItem", &info)
                    .property(move |_, _, _, _, name| match name {
                        "Status" => state.borrow().to_variant(),
                        "Title" => "Test tray app".to_variant(),
                        "Id" => "test".to_variant(),
                        "Menu" => glib::variant::ObjectPath::try_from("/Menu")
                            .unwrap()
                            .to_variant(),
                        "ItemIsMenu" => false.to_variant(),
                        "IconPixmap" => vec![(1i32, 1i32, vec![255u8, 20, 30, 40])].to_variant(),
                        _ => "".to_variant(),
                    })
                    .method_call(move |_, _, _, _, method, _, invocation| {
                        calls.borrow_mut().push(method.to_string());
                        invocation.return_value(None);
                    })
                    .build()
                    .unwrap();
                let info = gio::DBusNodeInfo::for_xml(MENU_XML)
                    .unwrap()
                    .lookup_interface(tray::MENU)
                    .unwrap();
                let calls = events.clone();
                let _menu = app
                    .register_object("/Menu", &info)
                    .method_call(move |_, _, _, _, method, params, invocation| {
                        calls
                            .borrow_mut()
                            .push(format!("{method}:{}", params.print(false)));
                        match method {
                            "AboutToShow" => invocation.return_value(Some(&(true,).to_variant())),
                            "GetLayout" => {
                                let node = layout(
                                    0,
                                    &[],
                                    vec![layout(
                                        7,
                                        &[("label", "Open application".to_variant())],
                                        vec![],
                                    )],
                                );
                                invocation.return_value(Some(&glib::Variant::tuple_from_iter([
                                    1u32.to_variant(),
                                    node,
                                ])));
                            }
                            _ => invocation.return_value(None),
                        }
                    })
                    .build()
                    .unwrap();
                let watcher = format!("{}{}", tray::WATCHER, tray::PATH);
                for _ in 0..2 {
                    tray::call(
                        &app,
                        &watcher,
                        tray::WATCHER,
                        "RegisterStatusNotifierItem",
                        ("/StatusNotifierItem",).to_variant(),
                    )
                    .await
                    .unwrap();
                }
                until(|| seen.borrow().len() == 1).await;
                let item = seen.borrow()[0].clone();
                assert_eq!(item.title, "Test tray app");
                assert_eq!(item.pixels, Some((1, 1, vec![20, 30, 40, 255])));
                tray::call(
                    &bus,
                    &item.address,
                    tray::ITEM,
                    "Activate",
                    (0i32, 0i32).to_variant(),
                )
                .await
                .unwrap();
                assert!(events.borrow().contains(&"Activate".into()));
                let menu = tray::menu(&bus, &item, true).await.unwrap();
                assert_eq!(menu.children[0].label, "Open application");
                let menu_address = format!("{}/Menu", app.unique_name().unwrap());
                tray::call(
                    &bus,
                    &menu_address,
                    tray::MENU,
                    "Event",
                    (7i32, "clicked", 0i32.to_variant(), 0u32).to_variant(),
                )
                .await
                .unwrap();
                assert!(
                    events
                        .borrow()
                        .iter()
                        .any(|e| e.starts_with("AboutToShow:"))
                );
                assert!(
                    events
                        .borrow()
                        .iter()
                        .any(|e| e.starts_with("Event:(7, 'clicked'"))
                );
                *status.borrow_mut() = "Passive".into();
                app.emit_signal(
                    None,
                    "/StatusNotifierItem",
                    tray::ITEM,
                    "NewStatus",
                    Some(&("Passive",).to_variant()),
                )
                .unwrap();
                until(|| seen.borrow().is_empty()).await;
                *status.borrow_mut() = "NeedsAttention".into();
                app.emit_signal(
                    None,
                    "/StatusNotifierItem",
                    tray::ITEM,
                    "NewStatus",
                    Some(&("NeedsAttention",).to_variant()),
                )
                .unwrap();
                until(|| seen.borrow().len() == 1).await;
                assert!(seen.borrow()[0].attention);
                // A second host reads the already registered item through the existing watcher.
                let second_seen = Rc::new(RefCell::new(vec![]));
                let received = second_seen.clone();
                let _second = Host::on_bus(connection().await, move |items| {
                    *received.borrow_mut() = items
                })
                .unwrap();
                until(|| second_seen.borrow().len() == 1).await;
                app.close_future().await.unwrap();
                until(|| seen.borrow().is_empty() && second_seen.borrow().is_empty()).await;
                gio::bus_unwatch_name(watch);
            })
        })
        .unwrap();
}
