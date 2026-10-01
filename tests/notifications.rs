//! Run inside dbus-run-session. Applications talk to the server over a private bus.
use gtk::{gio, glib, prelude::*};
use nexus_control::notifications::{Change, NAME, PATH, Server};
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    sync::mpsc,
    time::{Duration, Instant},
};

fn call(bus: &gio::DBusConnection, method: &str, args: Option<glib::Variant>) -> glib::Variant {
    bus.call_sync(
        Some(NAME),
        PATH,
        NAME,
        method,
        args.as_ref(),
        None,
        gio::DBusCallFlags::NONE,
        5000,
        None::<&gio::Cancellable>,
    )
    .unwrap()
}
fn notify(bus: &gio::DBusConnection, replaces: u32, summary: &str) -> u32 {
    let hints: HashMap<String, glib::Variant> = HashMap::new();
    call(
        bus,
        "Notify",
        Some(
            (
                "Test",
                replaces,
                "",
                summary,
                "<b>Body</b> & more",
                vec!["default", "Open"],
                hints,
                -1i32,
            )
                .to_variant(),
        ),
    )
    .child_value(0)
    .get::<u32>()
    .unwrap()
}
#[test]
#[ignore = "requires a private D-Bus: dbus-run-session -- cargo test --test notifications -- --ignored"]
fn notifications_roundtrip() {
    let (changes, received) = mpsc::channel();
    let (commands, pending) = mpsc::channel::<(&str, u32)>();
    // The server lives on its own main context, like the shell's main loop.
    std::thread::spawn(move || {
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let server = Server::start(move |change| {
                    let _ = changes.send(match change {
                        Change::Posted(n) => format!("posted {} {} {}", n.id, n.summary, n.body),
                        Change::Closed(id) => format!("closed {id}"),
                        Change::Status => "status".into(),
                    });
                });
                loop {
                    context.iteration(false);
                    match pending.try_recv() {
                        Ok(("invoke", id)) => server.invoke(id, "default"),
                        Ok((_, id)) => server.dismiss(id),
                        Err(mpsc::TryRecvError::Empty) => {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        Err(mpsc::TryRecvError::Disconnected) => break,
                    }
                }
            })
            .unwrap();
    });
    let next = || received.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(next(), "status", "the server owns the name");

    let context = glib::MainContext::new();
    let _guard = context.acquire().unwrap();
    let signals = Rc::new(RefCell::new(vec![]));
    let bus = context
        .with_thread_default(|| {
            let bus = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>).unwrap();
            let seen = signals.clone();
            let subscription = bus.subscribe_to_signal(
                Some(NAME),
                Some(NAME),
                None,
                Some(PATH),
                None,
                gio::DBusSignalFlags::NONE,
                move |s| {
                    seen.borrow_mut().push(format!(
                        "{} {}",
                        s.signal_name,
                        s.parameters.print(false)
                    ))
                },
            );
            (bus, subscription)
        })
        .unwrap();
    let signal = |expected: &str| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !signals.borrow().iter().any(|s| s == expected) {
            assert!(
                Instant::now() < deadline,
                "no {expected}: {:?}",
                signals.borrow()
            );
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let (bus, _subscription) = bus;

    let caps = call(&bus, "GetCapabilities", None)
        .child_value(0)
        .get::<Vec<String>>()
        .unwrap();
    assert!(caps.contains(&"actions".into()) && caps.contains(&"body-markup".into()));
    let info = call(&bus, "GetServerInformation", None);
    assert_eq!(info.child_value(3).str(), Some("1.2"));

    let first = notify(&bus, 0, "First");
    assert_eq!(
        next(),
        format!("posted {first} First <b>Body</b> &amp; more")
    );
    assert_eq!(notify(&bus, first, "Updated"), first, "replaced in place");
    assert!(next().starts_with(&format!("posted {first} Updated")));
    let second = notify(&bus, 0, "Second");
    assert_ne!(second, first);
    next();

    // The application withdraws it.
    call(&bus, "CloseNotification", Some((first,).to_variant()));
    assert_eq!(next(), format!("closed {first}"));
    signal(&format!("NotificationClosed ({first}, 3)"));
    // Closing an unknown notification is not an error.
    call(&bus, "CloseNotification", Some((first,).to_variant()));

    // A click on it runs the default action, then closes it as dismissed.
    commands.send(("invoke", second)).unwrap();
    assert_eq!(next(), format!("closed {second}"));
    signal(&format!("ActionInvoked ({second}, 'default')"));
    signal(&format!("NotificationClosed ({second}, 2)"));

    let third = notify(&bus, 0, "Third");
    next();
    commands.send(("dismiss", third)).unwrap();
    assert_eq!(next(), format!("closed {third}"));
    signal(&format!("NotificationClosed ({third}, 2)"));

    let bad = bus.call_sync(
        Some(NAME),
        PATH,
        NAME,
        "Notify",
        Some(&("too", "few").to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        5000,
        None::<&gio::Cancellable>,
    );
    assert!(bad.is_err());
}
