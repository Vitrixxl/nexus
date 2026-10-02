//! Run inside dbus-run-session: the backend talks to a fake NetworkManager on a
//! private bus, never to the machine's network.
use nexus_control::backend::Backend;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use zbus::{
    ObjectServer,
    blocking::connection::Builder,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

type Settings = HashMap<String, HashMap<String, OwnedValue>>;
const DEVICE: &str = "/org/freedesktop/NetworkManager/Devices/2";

#[derive(Default)]
struct World {
    wireless_enabled: bool,
    device_state: u32,
    active: Option<OwnedObjectPath>,
    active_states: HashMap<String, (u32, OwnedObjectPath)>,
    profiles: HashMap<String, Settings>,
    activated: Vec<(String, String, String)>,
    added: Vec<Settings>,
    scans: u32,
    agents: Vec<String>,
    next: u32,
    ssids: HashMap<String, &'static str>,
}
type Shared = Arc<Mutex<World>>;
fn path(p: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(p).unwrap()
}
fn value(v: Value<'_>) -> OwnedValue {
    OwnedValue::try_from(v).unwrap()
}
fn wifi_profile(ssid: &str, timestamp: u64) -> Settings {
    HashMap::from([
        (
            "connection".into(),
            HashMap::from([
                ("type".into(), value("802-11-wireless".into())),
                ("id".into(), value(ssid.into())),
                ("timestamp".into(), value(timestamp.into())),
            ]),
        ),
        (
            "802-11-wireless".into(),
            HashMap::from([("ssid".into(), value(ssid.as_bytes().to_vec().into()))]),
        ),
    ])
}

struct Manager(Shared);
#[zbus::interface(name = "org.freedesktop.NetworkManager")]
impl Manager {
    #[zbus(property)]
    fn wireless_enabled(&self) -> bool {
        self.0.lock().unwrap().wireless_enabled
    }
    #[zbus(property)]
    fn set_wireless_enabled(&mut self, on: bool) {
        self.0.lock().unwrap().wireless_enabled = on;
    }
    async fn activate_connection(
        &self,
        connection: OwnedObjectPath,
        device: OwnedObjectPath,
        specific: OwnedObjectPath,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> OwnedObjectPath {
        self.0.lock().unwrap().activated.push((
            connection.to_string(),
            device.to_string(),
            specific.to_string(),
        ));
        activate(&self.0, server, connection).await
    }
    async fn add_and_activate_connection(
        &self,
        settings: Settings,
        _device: OwnedObjectPath,
        specific: OwnedObjectPath,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> (OwnedObjectPath, OwnedObjectPath) {
        let profile = {
            let mut w = self.0.lock().unwrap();
            w.added.push(settings);
            w.next += 1;
            let profile = format!("/org/freedesktop/NetworkManager/Settings/{}", 10 + w.next);
            // Like NetworkManager, completed from the access point; it exists
            // before it connects.
            let ssid = w.ssids[specific.as_str()];
            w.profiles.insert(profile.clone(), wifi_profile(ssid, 0));
            profile
        };
        server
            .at(profile.as_str(), Profile(self.0.clone(), profile.clone()))
            .await
            .unwrap();
        let active = activate(&self.0, server, path(&profile)).await;
        (path(&profile), active)
    }
}
async fn activate(w: &Shared, server: &ObjectServer, profile: OwnedObjectPath) -> OwnedObjectPath {
    let active = {
        let mut w = w.lock().unwrap();
        w.next += 1;
        let active = path(&format!(
            "/org/freedesktop/NetworkManager/ActiveConnection/{}",
            w.next
        ));
        w.active_states
            .insert(active.to_string(), (1, profile.clone()));
        w.active = Some(active.clone());
        w.device_state = 50;
        active
    };
    server
        .at(active.as_str(), Active(w.clone(), active.to_string()))
        .await
        .unwrap();
    active
}

struct Active(Shared, String);
#[zbus::interface(name = "org.freedesktop.NetworkManager.Connection.Active")]
impl Active {
    #[zbus(property)]
    fn state(&self) -> u32 {
        self.0.lock().unwrap().active_states[&self.1].0
    }
    #[zbus(property)]
    fn connection(&self) -> OwnedObjectPath {
        self.0.lock().unwrap().active_states[&self.1].1.clone()
    }
}

struct Device(Shared);
#[zbus::interface(name = "org.freedesktop.NetworkManager.Device")]
impl Device {
    #[zbus(property)]
    fn state(&self) -> u32 {
        self.0.lock().unwrap().device_state
    }
    #[zbus(property)]
    fn active_connection(&self) -> OwnedObjectPath {
        self.0.lock().unwrap().active.clone().unwrap_or(path("/"))
    }
    fn disconnect(&self) {
        let mut w = self.0.lock().unwrap();
        w.active = None;
        w.device_state = 30;
    }
}
struct Wireless(Shared);
#[zbus::interface(name = "org.freedesktop.NetworkManager.Device.Wireless")]
impl Wireless {
    #[zbus(property)]
    fn access_points(&self) -> Vec<OwnedObjectPath> {
        (1..=5)
            .map(|n| path(&format!("/org/freedesktop/NetworkManager/AccessPoint/{n}")))
            .collect()
    }
    #[zbus(property)]
    fn active_access_point(&self) -> OwnedObjectPath {
        path("/")
    }
    fn request_scan(&self, _options: HashMap<String, OwnedValue>) {
        self.0.lock().unwrap().scans += 1;
    }
}
struct AccessPoint {
    ssid: &'static str,
    strength: u8,
    rsn: u32,
}
#[zbus::interface(name = "org.freedesktop.NetworkManager.AccessPoint")]
impl AccessPoint {
    #[zbus(property)]
    fn ssid(&self) -> Vec<u8> {
        self.ssid.as_bytes().to_vec()
    }
    #[zbus(property)]
    fn strength(&self) -> u8 {
        self.strength
    }
    #[zbus(property)]
    fn flags(&self) -> u32 {
        u32::from(self.rsn != 0)
    }
    #[zbus(property)]
    fn wpa_flags(&self) -> u32 {
        0
    }
    #[zbus(property)]
    fn rsn_flags(&self) -> u32 {
        self.rsn
    }
}
struct SettingsManager(Shared);
#[zbus::interface(name = "org.freedesktop.NetworkManager.Settings")]
impl SettingsManager {
    fn list_connections(&self) -> Vec<OwnedObjectPath> {
        self.0
            .lock()
            .unwrap()
            .profiles
            .keys()
            .map(|p| path(p))
            .collect()
    }
}
struct Profile(Shared, String);
#[zbus::interface(name = "org.freedesktop.NetworkManager.Settings.Connection")]
impl Profile {
    fn get_settings(&self) -> Settings {
        let w = self.0.lock().unwrap();
        w.profiles[&self.1]
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.iter()
                        .map(|(k, v)| (k.clone(), v.try_clone().unwrap()))
                        .collect(),
                )
            })
            .collect()
    }
    async fn delete(&self, #[zbus(object_server)] server: &ObjectServer) {
        self.0.lock().unwrap().profiles.remove(&self.1);
        server.remove::<Profile, _>(self.1.as_str()).await.unwrap();
    }
}
struct AgentManager(Shared);
#[zbus::interface(name = "org.freedesktop.NetworkManager.AgentManager")]
impl AgentManager {
    fn register(&self, identifier: String) {
        self.0.lock().unwrap().agents.push(identifier);
    }
}

fn wait(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..200 {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("Timed out waiting for {what}");
}

#[test]
#[ignore = "requires a private D-Bus: dbus-run-session -- cargo test --test networkmanager -- --ignored"]
fn wifi_through_networkmanager() {
    let bus = std::env::var("DBUS_SESSION_BUS_ADDRESS").expect("run inside dbus-run-session");
    // The backend uses the system bus; point it at the private one.
    unsafe { std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", bus) };
    let w: Shared = Arc::default();
    {
        let mut w = w.lock().unwrap();
        w.wireless_enabled = true;
        w.device_state = 30;
        w.profiles.insert(
            "/org/freedesktop/NetworkManager/Settings/1".into(),
            wifi_profile("Home", 100),
        );
    }
    let mut builder = Builder::session()
        .unwrap()
        .name("org.freedesktop.NetworkManager")
        .unwrap()
        .serve_at("/org/freedesktop", zbus::fdo::ObjectManager)
        .unwrap()
        .serve_at("/org/freedesktop/NetworkManager", Manager(w.clone()))
        .unwrap()
        .serve_at(DEVICE, Device(w.clone()))
        .unwrap()
        .serve_at(DEVICE, Wireless(w.clone()))
        .unwrap()
        .serve_at(
            "/org/freedesktop/NetworkManager/Settings",
            SettingsManager(w.clone()),
        )
        .unwrap()
        .serve_at(
            "/org/freedesktop/NetworkManager/Settings/1",
            Profile(
                w.clone(),
                "/org/freedesktop/NetworkManager/Settings/1".into(),
            ),
        )
        .unwrap()
        .serve_at(
            "/org/freedesktop/NetworkManager/AgentManager",
            AgentManager(w.clone()),
        )
        .unwrap();
    // Home is seen through two access points; the last one hides its name.
    for (n, ssid, strength, rsn) in [
        (1, "Home", 40, 0x188),
        (2, "Home", 70, 0x188),
        (3, "Café", 55, 0),
        (4, "Work", 30, 0x188),
        (5, "", 90, 0x188),
    ] {
        w.lock().unwrap().ssids.insert(
            format!("/org/freedesktop/NetworkManager/AccessPoint/{n}"),
            ssid,
        );
        builder = builder
            .serve_at(
                format!("/org/freedesktop/NetworkManager/AccessPoint/{n}"),
                AccessPoint {
                    ssid,
                    strength,
                    rsn,
                },
            )
            .unwrap();
    }
    let nm = builder.build().unwrap();
    let backend = Arc::new(Backend::new().unwrap());

    let (on, networks) = backend.wifi().unwrap();
    assert!(on);
    let names: Vec<_> = networks.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(names, ["Home", "Café", "Work"]);
    let home = &networks[0];
    assert_eq!(
        (home.signal, home.security.as_str(), home.saved),
        (70, "psk", true)
    );
    assert_eq!(home.state, "");
    assert_eq!(networks[1].security, "none");
    assert!(!networks[1].saved);
    let id = |name: &str| networks.iter().find(|n| n.name == name).unwrap().id.clone();

    // A saved network activates its profile on the Wi-Fi device.
    backend.action("wifi-connect", &id("Home"), "").unwrap();
    {
        let w = w.lock().unwrap();
        assert_eq!(w.agents, ["io.github.vitrixxl.Nexus"]);
        assert_eq!(
            w.activated,
            [(
                "/org/freedesktop/NetworkManager/Settings/1".into(),
                DEVICE.into(),
                "/".into()
            )]
        );
    }
    assert_eq!(backend.wifi().unwrap().1[0].state, "connecting");
    w.lock().unwrap().device_state = 100;
    assert!(backend.wifi().unwrap().1[0].connected());
    backend.action("wifi-disconnect", &id("Home"), "").unwrap();
    assert_eq!(backend.wifi().unwrap().1[0].state, "");
    assert!(backend.action("wifi-disconnect", &id("Home"), "").is_err());

    // A new secured network asks for its password before the profile is made.
    let b = backend.clone();
    let work = id("Work");
    let connecting = std::thread::spawn(move || b.action("wifi-connect", &work, ""));
    wait("the password prompt", || {
        backend.prompts.current().is_some()
    });
    let prompt = backend.prompts.current().unwrap();
    assert_eq!(prompt.fields, ["Passphrase"]);
    assert!(prompt.detail.contains("“Work”"));
    backend
        .prompts
        .answer(
            prompt.id,
            HashMap::from([("Passphrase".into(), "correct horse".into())]),
            true,
        )
        .unwrap();
    connecting.join().unwrap().unwrap();
    {
        let w = w.lock().unwrap();
        let security = &w.added[0]["802-11-wireless-security"];
        assert_eq!(<&str>::try_from(&security["key-mgmt"]).unwrap(), "wpa-psk");
        assert_eq!(<&str>::try_from(&security["psk"]).unwrap(), "correct horse");
    }

    // A failed activation drops the profile it created and says so.
    let created = w.lock().unwrap().profiles.len();
    backend.action("wifi-connect", &id("Café"), "").unwrap();
    let (active, profile) = {
        let w = w.lock().unwrap();
        assert_eq!(w.profiles.len(), created + 1);
        assert!(w.added[1].is_empty(), "an open network needs no settings");
        let active = w.active.clone().unwrap();
        (active.clone(), w.active_states[active.as_str()].1.clone())
    };
    std::thread::sleep(Duration::from_millis(300));
    w.lock()
        .unwrap()
        .active_states
        .get_mut(active.as_str())
        .unwrap()
        .0 = 4;
    nm.emit_signal(
        None::<()>,
        active.as_str(),
        "org.freedesktop.NetworkManager.Connection.Active",
        "StateChanged",
        &(4u32, 6u32),
    )
    .unwrap();
    wait("the profile to be deleted", || {
        !w.lock().unwrap().profiles.contains_key(profile.as_str())
    });
    wait("the failure message", || {
        backend.prompts.current().is_some()
    });
    let failure = backend.prompts.current().unwrap();
    assert!(failure.display_only);
    assert!(failure.detail.contains("“Café”"));
    assert!(failure.detail.contains("did not answer in time"));
    backend.prompts.cancel();

    backend.action("wifi-forget", &id("Home"), "").unwrap();
    assert!(
        !w.lock()
            .unwrap()
            .profiles
            .contains_key("/org/freedesktop/NetworkManager/Settings/1")
    );
    assert!(!backend.wifi().unwrap().1[0].saved);
    assert!(backend.action("wifi-connect", "00", "").is_err());

    backend.action("wifi-scan", "", "").unwrap();
    assert_eq!(w.lock().unwrap().scans, 1);
    backend.action("wifi-power", "", "false").unwrap();
    assert!(!w.lock().unwrap().wireless_enabled);
    assert!(!backend.wifi().unwrap().0);
}
