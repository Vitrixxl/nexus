use crate::*;
use anyhow::{Context, Result, bail};
use std::{
    collections::HashMap,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use zbus::{
    blocking::{Connection, Proxy},
    zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value},
};
type Props = HashMap<String, OwnedValue>;
type Objects = HashMap<OwnedObjectPath, HashMap<String, Props>>;
type Fields = HashMap<String, String>;
/// A NetworkManager connection profile: `a{sa{sv}}`.
type Settings = HashMap<String, Props>;
const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const NM_WIRELESS: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const NM_ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";
const NM_SETTINGS: &str = "org.freedesktop.NetworkManager.Settings";
const NM_CONNECTION: &str = "org.freedesktop.NetworkManager.Settings.Connection";
/// NetworkManager calls its secret agents at this fixed path.
const NM_AGENT_PATH: &str = "/org/freedesktop/NetworkManager/SecretAgent";
type Answer = Option<Fields>;
#[derive(Default)]
pub struct Prompts {
    pending: Mutex<Option<(Prompt, async_channel::Sender<Answer>)>>,
    sequence: AtomicU64,
    /// The user dismissed a Wi-Fi password request: the failure that follows
    /// needs no message of its own.
    wifi_declined: AtomicBool,
}
impl Prompts {
    pub fn current(&self) -> Option<Prompt> {
        self.pending.lock().unwrap().as_ref().map(|p| p.0.clone())
    }
    pub fn answer(&self, id: u64, values: Fields, accepted: bool) -> Result<()> {
        let mut pending = self.pending.lock().unwrap();
        let Some((prompt, sender)) = pending.as_ref() else {
            bail!("This request has expired");
        };
        if prompt.id != id {
            bail!("This request has expired");
        }
        if !prompt.display_only {
            sender.try_send(if accepted { Some(values) } else { None })?;
        }
        pending.take();
        Ok(())
    }
    pub fn cancel(&self) {
        if let Some((_, sender)) = self.pending.lock().unwrap().take() {
            let _ = sender.try_send(None);
        }
    }
    fn display(&self, title: String, detail: String) {
        let (tx, _) = async_channel::bounded(1);
        *self.pending.lock().unwrap() = Some((
            Prompt {
                id: self.sequence.fetch_add(1, Ordering::Relaxed) + 1,
                title,
                detail,
                fields: vec![],
                display_only: true,
            },
            tx,
        ));
    }
    async fn ask(&self, title: String, detail: String, fields: Vec<String>) -> Option<Fields> {
        self.cancel();
        let (tx, rx) = async_channel::bounded(1);
        let id = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        *self.pending.lock().unwrap() = Some((
            Prompt {
                id,
                title,
                detail,
                fields,
                display_only: false,
            },
            tx,
        ));
        let result = futures_lite::future::or(async { rx.recv().await.ok().flatten() }, async {
            async_io::Timer::after(Duration::from_secs(90)).await;
            None
        })
        .await;
        let mut pending = self.pending.lock().unwrap();
        if pending.as_ref().is_some_and(|p| p.0.id == id) {
            pending.take();
        }
        result
    }
}
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.NetworkManager.SecretAgent")]
pub enum WifiError {
    UserCanceled(String),
    NoSecrets(String),
    #[zbus(error)]
    ZBus(zbus::Error),
}
/// NetworkManager secret agent: asks for a Wi-Fi password when a saved one is
/// rejected or missing. Secrets go back to NetworkManager, which stores them.
pub struct WifiAgent(pub Arc<Prompts>);
#[zbus::interface(name = "org.freedesktop.NetworkManager.SecretAgent")]
impl WifiAgent {
    async fn get_secrets(
        &self,
        connection: Settings,
        _connection_path: OwnedObjectPath,
        setting_name: String,
        _hints: Vec<String>,
        flags: u32,
    ) -> std::result::Result<HashMap<String, HashMap<String, Value<'static>>>, WifiError> {
        const ALLOW_INTERACTION: u32 = 0x1;
        const REQUEST_NEW: u32 = 0x2;
        if flags & ALLOW_INTERACTION == 0 {
            return Err(WifiError::NoSecrets("Nexus only asks the user".into()));
        }
        let key_mgmt = connection
            .get("802-11-wireless-security")
            .map(|s| string(s, "key-mgmt"))
            .unwrap_or_default();
        let (field, key) = match (setting_name.as_str(), key_mgmt.as_str()) {
            ("802-11-wireless-security", "wpa-psk" | "sae") => ("Passphrase", "psk"),
            ("802-11-wireless-security", "none") => ("Passphrase", "wep-key0"),
            ("802-1x", _) => ("Password", "password"),
            _ => {
                return Err(WifiError::NoSecrets(
                    "Unsupported authentication method".into(),
                ));
            }
        };
        let name = connection
            .get("802-11-wireless")
            .and_then(|w| owned::<Vec<u8>>(w, "ssid"))
            .map(|ssid| String::from_utf8_lossy(&ssid).into_owned())
            .unwrap_or_else(|| "this network".into());
        let detail = if flags & REQUEST_NEW != 0 {
            format!("The password for “{name}” was not accepted. Enter it again.")
        } else {
            format!("Enter the password for “{name}”.")
        };
        let Some(answers) = self
            .0
            .ask("Connect to Wi-Fi".into(), detail, vec![field.into()])
            .await
        else {
            self.0.wifi_declined.store(true, Ordering::Relaxed);
            return Err(WifiError::UserCanceled("Connection canceled".into()));
        };
        let secret = answers.get(field).cloned().unwrap_or_default();
        Ok(HashMap::from([(
            setting_name,
            HashMap::from([(key.to_owned(), Value::from(secret))]),
        )]))
    }
    fn cancel_get_secrets(&self, _connection_path: OwnedObjectPath, _setting_name: String) {
        self.0.cancel();
    }
    // NetworkManager keeps the secrets of the profiles Nexus creates itself.
    fn save_secrets(&self, _connection: Settings, _connection_path: OwnedObjectPath) {}
    fn delete_secrets(&self, _connection: Settings, _connection_path: OwnedObjectPath) {}
}
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
pub enum BluetoothError {
    Rejected(String),
    #[zbus(error)]
    ZBus(zbus::Error),
}
pub struct BluetoothAgent(pub Arc<Prompts>);
impl BluetoothAgent {
    async fn ask(
        &self,
        device: OwnedObjectPath,
        detail: String,
        fields: Vec<String>,
    ) -> std::result::Result<Fields, BluetoothError> {
        self.0
            .ask(
                "Bluetooth pairing".into(),
                format!(
                    "{detail}\n{}",
                    device.as_str().rsplit('/').next().unwrap_or_default()
                ),
                fields,
            )
            .await
            .ok_or_else(|| BluetoothError::Rejected("Pairing canceled".into()))
    }
}
#[zbus::interface(name = "org.bluez.Agent1")]
impl BluetoothAgent {
    async fn request_pin_code(
        &self,
        device: OwnedObjectPath,
    ) -> std::result::Result<String, BluetoothError> {
        let a = self
            .ask(device, "Enter the device PIN.".into(), vec!["PIN".into()])
            .await?;
        let pin = a.get("PIN").cloned().unwrap_or_default();
        if pin.is_empty() || pin.len() > 16 {
            return Err(BluetoothError::Rejected("Invalid PIN".into()));
        }
        Ok(pin)
    }
    async fn request_passkey(
        &self,
        device: OwnedObjectPath,
    ) -> std::result::Result<u32, BluetoothError> {
        let a = self
            .ask(
                device,
                "Enter the six-digit passkey.".into(),
                vec!["Passkey".into()],
            )
            .await?;
        a.get("Passkey")
            .and_then(|p| p.parse::<u32>().ok())
            .filter(|p| *p <= 999999)
            .ok_or_else(|| BluetoothError::Rejected("Invalid passkey".into()))
    }
    async fn request_confirmation(
        &self,
        device: OwnedObjectPath,
        passkey: u32,
    ) -> std::result::Result<(), BluetoothError> {
        self.ask(
            device,
            format!("Confirm that the device shows this code: {passkey:06}"),
            vec![],
        )
        .await
        .map(|_| ())
    }
    async fn request_authorization(
        &self,
        device: OwnedObjectPath,
    ) -> std::result::Result<(), BluetoothError> {
        self.ask(device, "Allow this device to pair?".into(), vec![])
            .await
            .map(|_| ())
    }
    async fn authorize_service(
        &self,
        device: OwnedObjectPath,
        uuid: String,
    ) -> std::result::Result<(), BluetoothError> {
        self.ask(
            device,
            format!("Allow the requested Bluetooth service?\n{uuid}"),
            vec![],
        )
        .await
        .map(|_| ())
    }
    fn display_pin_code(&self, _device: OwnedObjectPath, pincode: String) {
        self.0.display(
            "Bluetooth pairing".into(),
            format!("Enter this PIN on your device: {pincode}"),
        );
    }
    fn display_passkey(&self, _device: OwnedObjectPath, passkey: u32, entered: u16) {
        self.0.display(
            "Bluetooth pairing".into(),
            format!("Enter {passkey:06} on your device ({entered} digits entered)."),
        );
    }
    fn cancel(&self) {
        self.0.cancel();
    }
    fn release(&self) {
        self.0.cancel();
    }
}
pub struct Backend {
    pub connection: Connection,
    pub prompts: Arc<Prompts>,
    bluetooth_scanning: Arc<Mutex<bool>>,
}
fn string(p: &Props, k: &str) -> String {
    p.get(k)
        .and_then(|v| <&str>::try_from(v).ok())
        .unwrap_or_default()
        .into()
}
fn boolean(p: &Props, k: &str) -> bool {
    p.get(k)
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false)
}
fn number(p: &Props, k: &str) -> u32 {
    p.get(k).and_then(|v| u32::try_from(v).ok()).unwrap_or(0)
}
fn owned<T: TryFrom<OwnedValue>>(p: &Props, k: &str) -> Option<T> {
    T::try_from(p.get(k)?.try_clone().ok()?).ok()
}
fn ssid_id(ssid: &[u8]) -> String {
    ssid.iter().map(|b| format!("{b:02x}")).collect()
}
/// Security of an access point, from its NM80211ApFlags and NM80211ApSecurityFlags.
fn ap_security(ap: &Props) -> &'static str {
    let keys = number(ap, "WpaFlags") | number(ap, "RsnFlags");
    if keys & (0x200 | 0x2000) != 0 {
        "ieee8021x"
    } else if keys & 0x100 != 0 {
        "psk"
    } else if keys & 0x400 != 0 {
        "sae"
    } else if keys & 0x800 != 0 {
        "owe"
    } else if number(ap, "Flags") & 0x1 != 0 {
        "wep"
    } else {
        "none"
    }
}
/// NMDeviceState of the Wi-Fi device, for the network it is working on.
fn device_state(state: u32) -> &'static str {
    match state {
        40..=90 => "connecting",
        100 => "connected",
        110 => "disconnecting",
        120 => "failed",
        _ => "",
    }
}
/// The Wi-Fi device, as NetworkManager sees it in one pass.
struct Radio {
    enabled: bool,
    networks: Vec<Network>,
    device: OwnedObjectPath,
    /// Strongest access point of each network.
    access_points: HashMap<String, OwnedObjectPath>,
    /// Saved profiles of each network, most recently used first.
    saved: HashMap<String, Vec<OwnedObjectPath>>,
}
/// Follows one activation to its end: `Ok` once connected, otherwise the
/// NMActiveConnectionStateReason it ended with (0 when unknown).
async fn activation(
    connection: &zbus::Connection,
    active: &OwnedObjectPath,
) -> zbus::Result<std::result::Result<(), u32>> {
    let p: zbus::Proxy = zbus::proxy::Builder::new(connection)
        .destination(NM)?
        .path(active.as_ref())?
        .interface(NM_ACTIVE)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;
    let mut changes = p.receive_signal("StateChanged").await?;
    // Read after subscribing, so no change can slip in between.
    match p.get_property::<u32>("State").await {
        Ok(2) => return Ok(Ok(())),
        Ok(4) | Err(_) => return Ok(Err(0)),
        _ => {}
    }
    while let Some(message) = futures_lite::StreamExt::next(&mut changes).await {
        match message.body().deserialize::<(u32, u32)>()? {
            (2, _) => return Ok(Ok(())),
            (4, reason) => return Ok(Err(reason)),
            _ => {}
        }
    }
    Ok(Err(0))
}
impl Backend {
    pub fn new() -> Result<Self> {
        let prompts = Arc::new(Prompts::default());
        let connection = zbus::blocking::connection::Builder::system()?
            .method_timeout(Duration::from_secs(110))
            .serve_at(NM_AGENT_PATH, WifiAgent(prompts.clone()))?
            .serve_at(
                "/io/github/vitrixxl/Nexus/Bluetooth",
                BluetoothAgent(prompts.clone()),
            )?
            .build()?;
        Ok(Self {
            connection,
            prompts,
            bluetooth_scanning: Arc::new(Mutex::new(false)),
        })
    }
    fn proxy<'a>(&'a self, dest: &'a str, path: &'a str, interface: &'a str) -> Result<Proxy<'a>> {
        Ok(Proxy::new(&self.connection, dest, path, interface)?)
    }
    pub fn register_agents(&self) {
        // Fails harmlessly when this connection is already registered.
        if let Ok(p) = self.proxy(
            NM,
            "/org/freedesktop/NetworkManager/AgentManager",
            "org.freedesktop.NetworkManager.AgentManager",
        ) {
            let _: zbus::Result<()> = p.call("Register", &("io.github.vitrixxl.Nexus",));
        }
        if let Ok(p) = self.proxy("org.bluez", "/org/bluez", "org.bluez.AgentManager1") {
            let _: zbus::Result<()> = p.call(
                "RegisterAgent",
                &(
                    ObjectPath::try_from("/io/github/vitrixxl/Nexus/Bluetooth").unwrap(),
                    "KeyboardDisplay",
                ),
            );
        }
    }
    pub fn wifi(&self) -> Result<(bool, Vec<Network>)> {
        let radio = self.radio()?;
        Ok((radio.enabled, radio.networks))
    }
    /// Saved Wi-Fi profiles with their SSID, most recently used first.
    fn saved_wifi(&self) -> Result<Vec<(OwnedObjectPath, Vec<u8>)>> {
        let paths: Vec<OwnedObjectPath> = self
            .proxy(NM, "/org/freedesktop/NetworkManager/Settings", NM_SETTINGS)?
            .call("ListConnections", &())?;
        let mut saved: Vec<_> = paths
            .into_iter()
            .filter_map(|path| {
                let settings: Settings = self
                    .proxy(NM, path.as_str(), NM_CONNECTION)
                    .ok()?
                    .call("GetSettings", &())
                    .ok()?;
                let connection = settings.get("connection")?;
                let wireless = settings.get("802-11-wireless")?;
                if string(connection, "type") != "802-11-wireless"
                    || matches!(string(wireless, "mode").as_str(), "ap" | "adhoc" | "mesh")
                {
                    return None;
                }
                let used = connection
                    .get("timestamp")
                    .and_then(|v| u64::try_from(v).ok())
                    .unwrap_or(0);
                Some((used, path, owned::<Vec<u8>>(wireless, "ssid")?))
            })
            .collect();
        saved.sort_by_key(|(used, ..)| std::cmp::Reverse(*used));
        Ok(saved
            .into_iter()
            .map(|(_, path, ssid)| (path, ssid))
            .collect())
    }
    fn radio(&self) -> Result<Radio> {
        let objects: Objects = self
            .proxy(NM, "/org/freedesktop", "org.freedesktop.DBus.ObjectManager")?
            .call("GetManagedObjects", &())
            .context("NetworkManager is not running")?;
        let manager = OwnedObjectPath::try_from(NM_PATH)?;
        let enabled = objects
            .get(&manager)
            .and_then(|o| o.get(NM))
            .is_some_and(|p| boolean(p, "WirelessEnabled"));
        // The first Wi-Fi device NetworkManager manages (state 10 is unmanaged).
        let mut devices: Vec<_> = objects
            .iter()
            .filter(|(_, o)| o.contains_key(NM_WIRELESS))
            .filter_map(|(path, o)| Some((path, o.get(NM_DEVICE)?, o.get(NM_WIRELESS)?)))
            .collect();
        devices.sort_by_key(|(path, device, _)| (number(device, "State") == 10, path.to_string()));
        let (device_path, device, wireless) = devices
            .into_iter()
            .next()
            .context("No Wi-Fi adapter found")?;
        let mut saved: HashMap<String, Vec<OwnedObjectPath>> = HashMap::new();
        let mut saved_ssid = HashMap::new();
        for (path, ssid) in self.saved_wifi().unwrap_or_default() {
            saved_ssid.insert(path.clone(), ssid.clone());
            saved.entry(ssid_id(&ssid)).or_default().push(path);
        }
        let ap = |path: &OwnedObjectPath| {
            objects
                .get(path)
                .and_then(|o| o.get("org.freedesktop.NetworkManager.AccessPoint"))
        };
        // The network the device is working on: that of its active profile,
        // else that of its access point.
        let active_ssid = owned::<OwnedObjectPath>(device, "ActiveConnection")
            .and_then(|active| objects.get(&active)?.get(NM_ACTIVE))
            .and_then(|active| saved_ssid.get(&owned::<OwnedObjectPath>(active, "Connection")?))
            .cloned()
            .or_else(|| {
                owned::<OwnedObjectPath>(wireless, "ActiveAccessPoint")
                    .and_then(|path| owned::<Vec<u8>>(ap(&path)?, "Ssid"))
            })
            .map(|ssid| ssid_id(&ssid));
        let state = device_state(number(device, "State"));
        let mut networks: HashMap<String, (Network, OwnedObjectPath)> = HashMap::new();
        for path in owned::<Vec<OwnedObjectPath>>(wireless, "AccessPoints").unwrap_or_default() {
            let Some(props) = ap(&path) else { continue };
            let Some(ssid) = owned::<Vec<u8>>(props, "Ssid").filter(|s| !s.is_empty()) else {
                continue; // Hidden networks cannot be joined without their name.
            };
            let id = ssid_id(&ssid);
            let signal = props
                .get("Strength")
                .and_then(|v| u8::try_from(v).ok())
                .unwrap_or(0);
            if networks.get(&id).is_some_and(|(n, _)| n.signal >= signal) {
                continue;
            }
            let network = Network {
                name: String::from_utf8_lossy(&ssid).into_owned(),
                signal,
                security: ap_security(props).into(),
                state: if active_ssid.as_ref() == Some(&id) {
                    state.into()
                } else {
                    String::new()
                },
                saved: saved.contains_key(&id),
                id: id.clone(),
            };
            networks.insert(id, (network, path));
        }
        let mut access_points = HashMap::new();
        let mut list: Vec<_> = networks
            .into_values()
            .map(|(network, path)| {
                access_points.insert(network.id.clone(), path);
                network
            })
            .collect();
        list.sort_by_key(|n| {
            (
                n.state.is_empty(),
                std::cmp::Reverse(n.signal),
                n.name.clone(),
            )
        });
        Ok(Radio {
            enabled,
            networks: list,
            device: device_path.clone(),
            access_points,
            saved,
        })
    }
    /// Settings completing a new profile for `network`, asking for its
    /// credentials first; `None` when the user cancels.
    fn new_profile(
        &self,
        network: &Network,
    ) -> Result<Option<HashMap<&'static str, HashMap<&'static str, Value<'static>>>>> {
        let ask = |fields: &[&str]| {
            async_io::block_on(self.prompts.ask(
                "Connect to Wi-Fi".into(),
                format!("Enter the password for “{}”.", network.name),
                fields.iter().map(|f| f.to_string()).collect(),
            ))
        };
        let security = |entries: Vec<(&'static str, Value<'static>)>| {
            HashMap::from([("802-11-wireless-security", entries.into_iter().collect())])
        };
        Ok(Some(match network.security.as_str() {
            "none" => HashMap::new(),
            "owe" => security(vec![("key-mgmt", "owe".into())]),
            "psk" | "sae" => {
                let Some(a) = ask(&["Passphrase"]) else {
                    return Ok(None);
                };
                let psk = a.get("Passphrase").cloned().unwrap_or_default();
                let hex = psk.len() == 64 && psk.bytes().all(|b| b.is_ascii_hexdigit());
                if !(8..=63).contains(&psk.len()) && !hex {
                    bail!("A Wi-Fi password has 8 to 63 characters");
                }
                let key_mgmt = if network.security == "sae" {
                    "sae"
                } else {
                    "wpa-psk"
                };
                security(vec![("key-mgmt", key_mgmt.into()), ("psk", psk.into())])
            }
            "wep" => {
                let Some(a) = ask(&["Passphrase"]) else {
                    return Ok(None);
                };
                let key = a.get("Passphrase").cloned().unwrap_or_default();
                // A literal 40/104-bit key (ASCII or hex), otherwise a passphrase.
                let literal = matches!(key.len(), 5 | 13)
                    || (matches!(key.len(), 10 | 26) && key.bytes().all(|b| b.is_ascii_hexdigit()));
                security(vec![
                    ("key-mgmt", "none".into()),
                    ("wep-key0", key.into()),
                    (
                        "wep-key-type",
                        Value::from(if literal { 1u32 } else { 2u32 }),
                    ),
                ])
            }
            // Enterprise networks: PEAP with MSCHAPv2, the common campus setup.
            _ => {
                let Some(a) = ask(&["Identity", "Password"]) else {
                    return Ok(None);
                };
                let mut settings = security(vec![("key-mgmt", "wpa-eap".into())]);
                settings.insert(
                    "802-1x",
                    HashMap::from([
                        ("eap", Value::from(vec!["peap"])),
                        ("phase2-auth", "mschapv2".into()),
                        (
                            "identity",
                            a.get("Identity").cloned().unwrap_or_default().into(),
                        ),
                        (
                            "password",
                            a.get("Password").cloned().unwrap_or_default().into(),
                        ),
                    ]),
                );
                settings
            }
        }))
    }
    /// Reports the outcome of an activation Nexus started, and drops the
    /// profile it created for it if that never connected.
    fn watch(&self, active: OwnedObjectPath, name: String, created: Option<OwnedObjectPath>) {
        let connection = self.connection.clone();
        let prompts = self.prompts.clone();
        prompts.wifi_declined.store(false, Ordering::Relaxed);
        std::thread::spawn(move || {
            let outcome = async_io::block_on(futures_lite::future::or(
                async {
                    activation(connection.inner(), &active)
                        .await
                        .unwrap_or(Err(0))
                },
                async {
                    async_io::Timer::after(Duration::from_secs(180)).await;
                    Ok(())
                },
            ));
            let Err(reason) = outcome else { return };
            if let Some(path) = created
                && let Ok(p) = Proxy::new(&connection, NM, path, NM_CONNECTION)
            {
                let _: zbus::Result<()> = p.call("Delete", &());
            }
            // Disconnected on purpose, or the profile was removed.
            if prompts.wifi_declined.swap(false, Ordering::Relaxed) || matches!(reason, 2 | 11) {
                return;
            }
            let hint = match reason {
                5 => " No network address was obtained.",
                6 => " The network did not answer in time.",
                9 => " Check the password and try again.",
                _ => "",
            };
            prompts.display(
                "Wi-Fi connection failed".into(),
                format!("Could not connect to “{name}”.{hint}"),
            );
        });
    }
    fn objects(&self) -> Result<Objects> {
        Ok(self
            .proxy("org.bluez", "/", "org.freedesktop.DBus.ObjectManager")?
            .call("GetManagedObjects", &())?)
    }
    fn adapter(&self) -> Result<String> {
        let mut adapters: Vec<_> = self
            .objects()?
            .into_iter()
            .filter(|(_, p)| p.contains_key("org.bluez.Adapter1"))
            .map(|(p, _)| p.to_string())
            .collect();
        adapters.sort();
        adapters
            .into_iter()
            .next()
            .context("No Bluetooth adapter found")
    }
    pub fn bluetooth(&self) -> Result<(bool, Vec<Device>)> {
        let objects = self.objects()?;
        let adapter = self.adapter()?;
        let enabled = objects
            .get(&OwnedObjectPath::try_from(adapter.as_str())?)
            .and_then(|p| p.get("org.bluez.Adapter1"))
            .is_some_and(|p| boolean(p, "Powered"));
        let mut devices: Vec<_> = objects
            .into_iter()
            .filter_map(|(path, mut interfaces)| {
                interfaces.remove("org.bluez.Device1").map(|p| Device {
                    path: path.to_string(),
                    name: string(&p, "Alias"),
                    address: string(&p, "Address"),
                    connected: boolean(&p, "Connected"),
                    paired: boolean(&p, "Paired"),
                    trusted: boolean(&p, "Trusted"),
                })
            })
            .collect();
        devices.sort_by_key(|d| (!d.connected, !d.paired, d.name.clone()));
        Ok((enabled, devices))
    }
    pub fn action(&self, op: &str, target: &str, value: &str) -> Result<()> {
        match op {
            "wifi-power" => {
                self.proxy(NM, NM_PATH, NM)?
                    .set_property("WirelessEnabled", parse_bool(value)?)?;
            }
            "wifi-scan" => {
                let radio = self.radio()?;
                if !radio.enabled {
                    return Ok(());
                }
                let scan = self
                    .proxy(NM, radio.device.as_str(), NM_WIRELESS)?
                    .call::<_, _, ()>("RequestScan", &(HashMap::<&str, Value>::new(),));
                match scan {
                    // A scan is already running or has just finished.
                    Err(zbus::Error::MethodError(name, ..)) if name.ends_with(".NotAllowed") => {}
                    result => result?,
                }
            }
            "wifi-connect" | "wifi-disconnect" | "wifi-forget" => {
                let radio = self.radio()?;
                let network = radio
                    .networks
                    .iter()
                    .find(|n| n.id == target)
                    .context("Unknown Wi-Fi network")?;
                let saved = radio.saved.get(target).cloned().unwrap_or_default();
                match op {
                    "wifi-connect" => {
                        self.register_agents();
                        let manager = self.proxy(NM, NM_PATH, NM)?;
                        let root = ObjectPath::try_from("/")?;
                        let (active, created) = if let Some(profile) = saved.first() {
                            let active: OwnedObjectPath = manager
                                .call("ActivateConnection", &(profile, &radio.device, root))?;
                            (active, None)
                        } else {
                            let Some(settings) = self.new_profile(network)? else {
                                return Ok(());
                            };
                            let ap = radio
                                .access_points
                                .get(target)
                                .context("Unknown Wi-Fi network")?;
                            let (profile, active): (OwnedObjectPath, OwnedObjectPath) = manager
                                .call("AddAndActivateConnection", &(settings, &radio.device, ap))?;
                            (active, Some(profile))
                        };
                        self.watch(active, network.name.clone(), created);
                    }
                    "wifi-disconnect" => {
                        if network.state.is_empty() {
                            bail!("This network is not connected");
                        }
                        self.proxy(NM, radio.device.as_str(), NM_DEVICE)?
                            .call::<_, _, ()>("Disconnect", &())?;
                    }
                    _ => {
                        for profile in &saved {
                            self.proxy(NM, profile.as_str(), NM_CONNECTION)?
                                .call::<_, _, ()>("Delete", &())?;
                        }
                    }
                }
            }
            "bluetooth-power" => {
                let path = self.adapter()?;
                self.proxy("org.bluez", &path, "org.bluez.Adapter1")?
                    .set_property("Powered", parse_bool(value)?)?;
            }
            "bluetooth-scan" => {
                let mut scanning = self.bluetooth_scanning.lock().unwrap();
                if *scanning {
                    return Ok(());
                }
                let adapter = self.adapter()?;
                let p = self.proxy("org.bluez", &adapter, "org.bluez.Adapter1")?;
                if !p.get_property::<bool>("Powered")? {
                    return Ok(());
                }
                match p.call::<_, _, ()>("StartDiscovery", &()) {
                    // Recover a lease whose earlier StopDiscovery failed.
                    Err(zbus::Error::MethodError(name, ..)) if name.ends_with(".InProgress") => {}
                    result => result?,
                }
                *scanning = true;
                drop(p);
                let connection = self.connection.clone();
                let scanning = self.bluetooth_scanning.clone();
                std::thread::spawn(move || {
                    // Finish before the next 15-second scan, including manual
                    // refreshes, and release only our own BlueZ discovery lease.
                    std::thread::sleep(Duration::from_secs(10));
                    if let Ok(p) =
                        Proxy::new(&connection, "org.bluez", adapter, "org.bluez.Adapter1")
                    {
                        let _: zbus::Result<()> = p.call("StopDiscovery", &());
                    }
                    *scanning.lock().unwrap() = false;
                });
            }
            "bluetooth-pair"
            | "bluetooth-connect"
            | "bluetooth-disconnect"
            | "bluetooth-forget"
            | "bluetooth-trust" => {
                if !self.bluetooth()?.1.iter().any(|d| d.path == target) {
                    bail!("Unknown Bluetooth device");
                }
                let p = self.proxy("org.bluez", target, "org.bluez.Device1")?;
                match op {
                    "bluetooth-pair" => {
                        self.register_agents();
                        p.call::<_, _, ()>("Pair", &())?;
                        self.prompts.cancel();
                        p.set_property("Trusted", true)?;
                        p.call::<_, _, ()>("Connect", &())?;
                    }
                    "bluetooth-connect" => {
                        p.call::<_, _, ()>("Connect", &())?;
                    }
                    "bluetooth-disconnect" => {
                        p.call::<_, _, ()>("Disconnect", &())?;
                    }
                    "bluetooth-trust" => {
                        p.set_property("Trusted", parse_bool(value)?)?;
                    }
                    _ => {
                        let adapter = target.rsplit_once('/').context("Invalid device")?.0;
                        self.proxy("org.bluez", adapter, "org.bluez.Adapter1")?
                            .call::<_, _, ()>("RemoveDevice", &(ObjectPath::try_from(target)?,))?;
                    }
                }
            }
            "volume" | "microphone" => {
                let n = percent(value)?;
                run(
                    "wpctl",
                    &[
                        "set-volume",
                        if op == "volume" {
                            "@DEFAULT_AUDIO_SINK@"
                        } else {
                            "@DEFAULT_AUDIO_SOURCE@"
                        },
                        &format!("{n}%"),
                    ],
                )?;
            }
            "mute" | "mic-mute" => {
                run(
                    "wpctl",
                    &[
                        "set-mute",
                        if op == "mute" {
                            "@DEFAULT_AUDIO_SINK@"
                        } else {
                            "@DEFAULT_AUDIO_SOURCE@"
                        },
                        if parse_bool(value)? { "1" } else { "0" },
                    ],
                )?;
            }
            "audio-output" | "audio-input" => {
                let kind = if op == "audio-output" {
                    "sinks"
                } else {
                    "sources"
                };
                if !audio_devices(kind)?.iter().any(|d| d.name == target) {
                    bail!("Unknown audio device");
                }
                run(
                    "pactl",
                    &[
                        if op == "audio-output" {
                            "set-default-sink"
                        } else {
                            "set-default-source"
                        },
                        target,
                    ],
                )?;
            }
            "stream-volume" | "stream-mute" => {
                let id = target
                    .parse::<u32>()
                    .context("Invalid application stream")?;
                if !app_streams()?.iter().any(|s| s.id == id) {
                    bail!("This application is no longer playing sound");
                }
                let id = id.to_string();
                if op == "stream-volume" {
                    run(
                        "pactl",
                        &[
                            "set-sink-input-volume",
                            &id,
                            &format!("{}%", percent(value)?),
                        ],
                    )?;
                } else {
                    run(
                        "pactl",
                        &[
                            "set-sink-input-mute",
                            &id,
                            if parse_bool(value)? { "1" } else { "0" },
                        ],
                    )?;
                }
            }
            "brightness" => {
                let n = percent(value)?.max(1);
                run(
                    "brightnessctl",
                    &["--class=backlight", "set", &format!("{n}%")],
                )?;
            }
            "night-light" => {
                if parse_bool(value)? {
                    // A daytime profile leaves a near-neutral temperature behind.
                    if night_light()?.temperature >= 6000 {
                        sunset(&["temperature", "4000"])?;
                    }
                    sunset(&["identity", "false"])?;
                } else {
                    sunset(&["identity", "true"])?;
                }
            }
            "night-light-temperature" => {
                let k = value
                    .parse::<u16>()
                    .ok()
                    .filter(|k| (NIGHT_LIGHT_MIN..=NIGHT_LIGHT_MAX).contains(k))
                    .with_context(|| {
                        format!(
                            "Temperature must be between {NIGHT_LIGHT_MIN} and {NIGHT_LIGHT_MAX} K"
                        )
                    })?;
                sunset(&["temperature", &k.to_string()])?;
            }
            // Back to the profile hyprsunset.conf schedules for the current time.
            "night-light-schedule" => {
                sunset(&["reset"])?;
            }
            "sleep" | "restart" | "shutdown" => {
                self.proxy(
                    "org.freedesktop.login1",
                    "/org/freedesktop/login1",
                    "org.freedesktop.login1.Manager",
                )?
                .call::<_, _, ()>(
                    match op {
                        "sleep" => "Suspend",
                        "restart" => "Reboot",
                        _ => "PowerOff",
                    },
                    &(false,),
                )?;
            }
            _ => bail!("Unknown action: {op}"),
        }
        Ok(())
    }
}
pub fn percent(value: &str) -> Result<u8> {
    let n = value.parse::<u8>()?;
    if n > 100 {
        bail!("Value must be between 0 and 100");
    }
    Ok(n)
}
fn parse_bool(value: &str) -> Result<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => bail!("Expected true or false"),
    }
}
pub fn run(program: &str, args: &[&str]) -> Result<String> {
    // timeout also bounds child lifetime, including dead PipeWire/brightness helpers.
    let out = Command::new("timeout")
        .arg("8s")
        .arg(program)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .with_context(|| format!("Could not start {program}"))?;
    if !out.status.success() {
        bail!("{program}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().into())
}
pub fn audio_devices(kind: &str) -> Result<Vec<AudioDevice>> {
    let out = run("pactl", &["--format=json", "list", kind])?;
    let json: Vec<serde_json::Value> = serde_json::from_str(&out)?;
    Ok(json
        .iter()
        .filter_map(|v| {
            Some(AudioDevice {
                name: v["name"].as_str()?.into(),
                description: v["description"].as_str().unwrap_or("Audio device").into(),
            })
        })
        .collect())
}
/// Application playback streams, with the average level across channels.
pub fn app_streams() -> Result<Vec<AppStream>> {
    let out = run("pactl", &["--format=json", "list", "sink-inputs"])?;
    let json: Vec<serde_json::Value> = serde_json::from_str(&out)?;
    Ok(json
        .iter()
        .filter_map(|v| {
            let props = &v["properties"];
            let prop = |key: &str| {
                props[key]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            };
            let binary = prop("application.process.binary");
            let name = prop("application.name")
                .or_else(|| binary.clone())
                .unwrap_or_else(|| "Application".into());
            let levels: Vec<f64> = v["volume"]
                .as_object()?
                .values()
                .filter_map(|c| {
                    c["value_percent"]
                        .as_str()?
                        .trim_end_matches('%')
                        .parse()
                        .ok()
                })
                .collect();
            let level = if levels.is_empty() {
                100.
            } else {
                levels.iter().sum::<f64>() / levels.len() as f64
            };
            Some(AppStream {
                id: u32::try_from(v["index"].as_u64()?).ok()?,
                detail: prop("media.name")
                    .filter(|m| *m != name)
                    .unwrap_or_default(),
                icon: prop("application.icon_name").or(binary.map(|b| b.to_lowercase())),
                name,
                volume: level.round().clamp(0., 100.) as u8,
                muted: v["mute"].as_bool().unwrap_or(false),
            })
        })
        .collect())
}
pub fn volume(source: bool) -> Result<(u8, bool)> {
    let out = run(
        "wpctl",
        &[
            "get-volume",
            if source {
                "@DEFAULT_AUDIO_SOURCE@"
            } else {
                "@DEFAULT_AUDIO_SINK@"
            },
        ],
    )?;
    let level = out
        .split_whitespace()
        .nth(1)
        .context("Missing volume")?
        .parse::<f32>()?;
    Ok((
        (level * 100.).round().clamp(0., 100.) as u8,
        out.contains("MUTED"),
    ))
}
pub fn brightness() -> Result<u8> {
    let current = run("brightnessctl", &["--class=backlight", "get"])?.parse::<f64>()?;
    let max = run("brightnessctl", &["--class=backlight", "max"])?.parse::<f64>()?;
    if max <= 0. {
        bail!("No backlight available");
    }
    Ok((current / max * 100.).round() as u8)
}
pub const NIGHT_LIGHT_MIN: u16 = 2500;
pub const NIGHT_LIGHT_MAX: u16 = 6500;
/// One hyprsunset IPC command through hyprctl.
fn sunset(args: &[&str]) -> Result<String> {
    let mut command = vec!["hyprsunset"];
    command.extend_from_slice(args);
    let reply = run("hyprctl", &command).map_err(|e| {
        if e.to_string().contains("Couldn't connect") {
            anyhow::anyhow!("hyprsunset is not running")
        } else {
            e
        }
    })?;
    if reply.starts_with("Invalid") || reply == "invalid command" || reply.starts_with("No profile")
    {
        bail!("hyprsunset: {reply}");
    }
    Ok(reply)
}
pub fn night_light() -> Result<NightLight> {
    Ok(NightLight {
        enabled: sunset(&["identity", "get"])? == "false",
        temperature: sunset(&["temperature"])?
            .parse()
            .context("Unexpected hyprsunset temperature")?,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_bad_controls() {
        assert!(percent("101").is_err());
        assert!(percent("-1").is_err());
        assert!(percent("42;shutdown").is_err());
        assert_eq!(percent("42").unwrap(), 42);
        assert!(parse_bool("yes").is_err());
    }
    #[test]
    fn prompt_ids_are_checked() {
        let p = Prompts::default();
        p.display("Pair".into(), "Code".into());
        assert!(p.answer(999, HashMap::new(), true).is_err());
        assert!(p.current().is_some());
        p.cancel();
        assert!(p.current().is_none());
    }
}
