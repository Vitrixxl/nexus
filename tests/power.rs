use gtk::{gio, glib, prelude::*};
use nexus_control::power;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
};

const PROPERTIES: &str = r#"<node><interface name="org.freedesktop.DBus.Properties">
<method name="GetAll"><arg type="s" direction="in"/><arg type="a{sv}" direction="out"/></method>
<method name="Set"><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="in"/></method>
</interface></node>"#;

async fn name(bus: &gio::DBusConnection, method: &str, name: &str) {
    let args = if method == "RequestName" {
        (name, 0u32).to_variant()
    } else {
        (name,).to_variant()
    };
    bus.call_future(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        method,
        Some(&args),
        None,
        gio::DBusCallFlags::NONE,
        1000,
    )
    .await
    .unwrap();
}

#[test]
#[ignore = "requires private D-Bus: dbus-run-session -- cargo test --test power -- --ignored"]
fn profiles_and_battery_roundtrip() {
    let context = glib::MainContext::new();
    context
        .with_thread_default(|| {
            context.block_on(async {
                // Both mocks and client use only this private session bus, never real power services.
                let bus = gio::bus_get_future(gio::BusType::Session).await.unwrap();
                let info = gio::DBusNodeInfo::for_xml(PROPERTIES)
                    .unwrap()
                    .lookup_interface("org.freedesktop.DBus.Properties")
                    .unwrap();
                let active = Rc::new(RefCell::new("balanced".to_string()));
                let allowed = Rc::new(Cell::new(true));
                let inhibited = Rc::new(Cell::new(false));
                let modern = "org.freedesktop.UPower.PowerProfiles";
                let legacy = "net.hadess.PowerProfiles";
                let mut registrations = vec![];
                for (interface, path) in [
                    (modern, "/org/freedesktop/UPower/PowerProfiles"),
                    (legacy, "/net/hadess/PowerProfiles"),
                ] {
                    let (active, allowed, inhibited) =
                        (active.clone(), allowed.clone(), inhibited.clone());
                    registrations.push(
                        bus.register_object(path, &info)
                            .method_call(move |_, _, _, _, method, args, invocation| {
                                assert_eq!(
                                    args.child_value(0).get::<String>().as_deref(),
                                    Some(interface)
                                );
                                match method {
                                    "GetAll" => {
                                        let modes: Vec<HashMap<String, glib::Variant>> =
                                            power::MODES
                                                .iter()
                                                .map(|(id, _, _)| {
                                                    HashMap::from([(
                                                        "Profile".to_string(),
                                                        id.to_variant(),
                                                    )])
                                                })
                                                .collect();
                                        let p = HashMap::from([
                                            ("ActiveProfile", active.borrow().to_variant()),
                                            ("Profiles", modes.to_variant()),
                                            (
                                                "PerformanceInhibited",
                                                (if inhibited.get() { "lap-detected" } else { "" })
                                                    .to_variant(),
                                            ),
                                        ]);
                                        invocation.return_value(Some(&(p,).to_variant()));
                                    }
                                    "Set" if !allowed.get() => invocation.return_dbus_error(
                                        "org.freedesktop.DBus.Error.AccessDenied",
                                        "Profile change denied",
                                    ),
                                    "Set" => {
                                        assert_eq!(
                                            args.child_value(1).get::<String>().as_deref(),
                                            Some("ActiveProfile")
                                        );
                                        *active.borrow_mut() = args
                                            .child_value(2)
                                            .as_variant()
                                            .unwrap()
                                            .get()
                                            .unwrap();
                                        invocation.return_value(None);
                                    }
                                    _ => unreachable!(),
                                }
                            })
                            .build()
                            .unwrap(),
                    );
                }
                name(&bus, "RequestName", modern).await;
                let initial = power::profiles(&bus).await.unwrap();
                assert_eq!(initial.active, "balanced");
                assert!(initial.supports("performance"));
                power::set_profile(&bus, &initial, "power-saver")
                    .await
                    .unwrap();
                assert_eq!(power::profiles(&bus).await.unwrap().active, "power-saver");
                assert!(power::set_profile(&bus, &initial, "invalid").await.is_err());
                allowed.set(false);
                assert!(
                    power::set_profile(&bus, &initial, "performance")
                        .await
                        .is_err()
                );
                assert_eq!(*active.borrow(), "power-saver");
                inhibited.set(true);
                let limited = power::profiles(&bus).await.unwrap();
                assert!(!limited.supports("performance"));
                assert!(
                    power::set_profile(&bus, &limited, "performance")
                        .await
                        .is_err()
                );
                // An external profile change must appear on the next read.
                *active.borrow_mut() = "balanced".into();
                assert_eq!(power::profiles(&bus).await.unwrap().active, "balanced");
                name(&bus, "ReleaseName", modern).await;
                name(&bus, "RequestName", legacy).await;
                let fallback = power::profiles(&bus).await.unwrap();
                allowed.set(true);
                power::set_profile(&bus, &fallback, "power-saver")
                    .await
                    .unwrap();
                assert_eq!(*active.borrow(), "power-saver");
                name(&bus, "ReleaseName", legacy).await;
                assert!(power::profiles(&bus).await.is_err());

                let state = Rc::new(Cell::new(1u32));
                let present = Rc::new(Cell::new(true));
                let (mock_state, mock_present) = (state.clone(), present.clone());
                registrations.push(
                    bus.register_object("/org/freedesktop/UPower/devices/DisplayDevice", &info)
                        .method_call(move |_, _, _, _, method, args, invocation| {
                            assert_eq!(method, "GetAll");
                            assert_eq!(
                                args.get::<(String,)>().unwrap().0,
                                "org.freedesktop.UPower.Device"
                            );
                            let p = HashMap::from([
                                ("IsPresent", mock_present.get().to_variant()),
                                ("State", mock_state.get().to_variant()),
                                ("TimeToFull", 3600i64.to_variant()),
                                ("TimeToEmpty", 5400i64.to_variant()),
                            ]);
                            invocation.return_value(Some(&(p,).to_variant()));
                        })
                        .build()
                        .unwrap(),
                );
                name(&bus, "RequestName", "org.freedesktop.UPower").await;
                assert_eq!(
                    power::battery_status(&bus).await.unwrap(),
                    "1 h 00 min until fully charged"
                );
                state.set(2);
                assert_eq!(
                    power::battery_status(&bus).await.unwrap(),
                    "1 h 30 min remaining"
                );
                state.set(4);
                assert_eq!(power::battery_status(&bus).await.unwrap(), "Fully charged");
                present.set(false);
                assert_eq!(power::battery_status(&bus).await.unwrap(), "No battery");
                name(&bus, "ReleaseName", "org.freedesktop.UPower").await;
                assert!(power::battery_status(&bus).await.is_err());
                for registration in registrations {
                    bus.unregister_object(registration).unwrap();
                }
            })
        })
        .unwrap();
}
