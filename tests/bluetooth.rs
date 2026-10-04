//! A private BlueZ mock; never starts discovery on the machine's real radios.
use nexus_control::backend::Backend;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use zbus::blocking::connection::Builder;

#[derive(Default)]
struct State {
    powered: bool,
    discovering: bool,
    starts: usize,
    stops: usize,
}
struct Adapter(Arc<Mutex<State>>);
#[zbus::interface(name = "org.bluez.Adapter1")]
impl Adapter {
    #[zbus(property)]
    fn powered(&self) -> bool {
        self.0.lock().unwrap().powered
    }
    #[zbus(property)]
    fn set_powered(&mut self, enabled: bool) {
        self.0.lock().unwrap().powered = enabled;
    }
    fn start_discovery(&self) -> zbus::fdo::Result<()> {
        let mut s = self.0.lock().unwrap();
        if !s.powered || s.discovering {
            return Err(zbus::fdo::Error::Failed(
                "radio off or scan already running".into(),
            ));
        }
        s.discovering = true;
        s.starts += 1;
        Ok(())
    }
    fn stop_discovery(&self) {
        let mut s = self.0.lock().unwrap();
        s.discovering = false;
        s.stops += 1;
    }
}

#[test]
#[ignore = "requires private D-Bus: dbus-run-session -- cargo test --test bluetooth -- --ignored"]
fn scans_only_when_enabled_and_never_overlap() {
    let address = std::env::var("DBUS_SESSION_BUS_ADDRESS").expect("run inside dbus-run-session");
    // This test binary has only one test, before any D-Bus threads are started.
    unsafe { std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", address) };
    let state = Arc::new(Mutex::new(State::default()));
    let _server = Builder::session()
        .unwrap()
        .name("org.bluez")
        .unwrap()
        .serve_at("/", zbus::fdo::ObjectManager)
        .unwrap()
        .serve_at("/org/bluez/hci0", Adapter(state.clone()))
        .unwrap()
        .build()
        .unwrap();
    let backend = Backend::new().unwrap();
    backend.action("bluetooth-scan", "", "").unwrap();
    assert_eq!(state.lock().unwrap().starts, 0);
    backend.action("bluetooth-power", "", "true").unwrap();
    backend.action("bluetooth-scan", "", "").unwrap();
    backend.action("bluetooth-scan", "", "").unwrap();
    assert_eq!(state.lock().unwrap().starts, 1);
    let deadline = Instant::now() + Duration::from_secs(13);
    while state.lock().unwrap().stops == 0 {
        assert!(
            Instant::now() < deadline,
            "discovery must stop before the next 15-second scan"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    backend.action("bluetooth-power", "", "false").unwrap();
    backend.action("bluetooth-scan", "", "").unwrap();
    assert_eq!(state.lock().unwrap().starts, 1);
    backend.action("bluetooth-power", "", "true").unwrap();
    backend.action("bluetooth-scan", "", "").unwrap();
    assert_eq!(state.lock().unwrap().starts, 2);
}
