use crate::*;
use anyhow::{Context, Result, bail};
use std::{
    collections::HashMap,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use zbus::{
    blocking::{Connection, Proxy},
    zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value},
};
type Props = HashMap<String, OwnedValue>;
type Objects = HashMap<OwnedObjectPath, HashMap<String, Props>>;
type Fields = HashMap<String, String>;
type Answer = Option<Fields>;
#[derive(Default)]
pub struct Prompts {
    pending: Mutex<Option<(Prompt, async_channel::Sender<Answer>)>>,
    sequence: AtomicU64,
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
#[zbus(prefix = "net.connman.Agent.Error")]
pub enum WifiError {
    Canceled(String),
    #[zbus(error)]
    ZBus(zbus::Error),
}
pub struct WifiAgent(pub Arc<Prompts>);
#[zbus::interface(name = "net.connman.Agent")]
impl WifiAgent {
    async fn request_input(
        &self,
        service: OwnedObjectPath,
        fields: Props,
    ) -> std::result::Result<HashMap<String, Value<'static>>, WifiError> {
        let names: Vec<_> = ["Name", "Identity", "Username", "Passphrase", "Password"]
            .into_iter()
            .filter(|key| fields.contains_key(*key))
            .map(str::to_owned)
            .collect();
        if names.is_empty() {
            return Err(WifiError::Canceled(
                "Unsupported authentication method".into(),
            ));
        }
        let answers = self
            .0
            .ask(
                "Connect to Wi-Fi".into(),
                format!(
                    "Enter the credentials requested by ConnMan.\n{}",
                    service.as_str().rsplit('/').next().unwrap_or_default()
                ),
                names.clone(),
            )
            .await
            .ok_or_else(|| WifiError::Canceled("Connection canceled".into()))?;
        Ok(names
            .into_iter()
            .filter_map(|key| answers.get(&key).map(|v| (key, Value::from(v.clone()))))
            .collect())
    }
    fn report_error(&self, _service: OwnedObjectPath, error: String) {
        self.0.display("Wi-Fi connection failed".into(), error);
    }
    fn request_browser(&self, _service: OwnedObjectPath, url: String) {
        self.0.display(
            "Network sign-in required".into(),
            format!("Open this address in your browser:\n{url}"),
        );
    }
    fn cancel(&self) {
        self.0.cancel();
    }
    fn release(&self) {
        self.0.cancel();
    }
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
impl Backend {
    pub fn new() -> Result<Self> {
        let prompts = Arc::new(Prompts::default());
        let connection = zbus::blocking::connection::Builder::system()?
            .method_timeout(Duration::from_secs(110))
            .serve_at("/io/github/vitrixxl/Nexus/Wifi", WifiAgent(prompts.clone()))?
            .serve_at(
                "/io/github/vitrixxl/Nexus/Bluetooth",
                BluetoothAgent(prompts.clone()),
            )?
            .build()?;
        Ok(Self {
            connection,
            prompts,
        })
    }
    fn proxy<'a>(&'a self, dest: &'a str, path: &'a str, interface: &'a str) -> Result<Proxy<'a>> {
        Ok(Proxy::new(&self.connection, dest, path, interface)?)
    }
    pub fn register_agents(&self) {
        if let Ok(p) = self.proxy("net.connman", "/", "net.connman.Manager") {
            let _: zbus::Result<()> = p.call(
                "RegisterAgent",
                &(ObjectPath::try_from("/io/github/vitrixxl/Nexus/Wifi").unwrap(),),
            );
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
        let p = self.proxy("net.connman", "/", "net.connman.Manager")?;
        let technologies: Vec<(OwnedObjectPath, Props)> = p.call("GetTechnologies", &())?;
        let enabled = technologies
            .iter()
            .find(|(_, p)| string(p, "Type") == "wifi")
            .map(|(_, p)| boolean(p, "Powered"))
            .context("No Wi-Fi adapter found")?;
        let services: Vec<(OwnedObjectPath, Props)> = p.call("GetServices", &())?;
        let mut networks: Vec<_> = services
            .into_iter()
            .filter(|(_, p)| string(p, "Type") == "wifi")
            .map(|(path, p)| {
                let security = p
                    .get("Security")
                    .and_then(|v| Vec::<String>::try_from(v.try_clone().ok()?).ok())
                    .unwrap_or_default()
                    .join(", ");
                Network {
                    path: path.to_string(),
                    name: {
                        let s = string(&p, "Name");
                        if s.is_empty() {
                            "Hidden network".into()
                        } else {
                            s
                        }
                    },
                    signal: p
                        .get("Strength")
                        .and_then(|v| u8::try_from(v).ok())
                        .unwrap_or(0),
                    security,
                    state: string(&p, "State"),
                    saved: boolean(&p, "Favorite") || boolean(&p, "Immutable"),
                }
            })
            .collect();
        networks.sort_by_key(|n| {
            (
                !matches!(n.state.as_str(), "ready" | "online"),
                std::cmp::Reverse(n.signal),
                n.name.clone(),
            )
        });
        Ok((enabled, networks))
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
                let p = self.proxy(
                    "net.connman",
                    "/net/connman/technology/wifi",
                    "net.connman.Technology",
                )?;
                p.call::<_, _, ()>("SetProperty", &("Powered", Value::from(parse_bool(value)?)))?;
            }
            "wifi-scan" => {
                self.proxy(
                    "net.connman",
                    "/net/connman/technology/wifi",
                    "net.connman.Technology",
                )?
                .call::<_, _, ()>("Scan", &())?;
            }
            "wifi-connect" | "wifi-disconnect" | "wifi-forget" => {
                if !self.wifi()?.1.iter().any(|n| n.path == target) {
                    bail!("Unknown Wi-Fi network");
                }
                self.register_agents();
                let method = match op {
                    "wifi-connect" => "Connect",
                    "wifi-disconnect" => "Disconnect",
                    _ => "Remove",
                };
                self.proxy("net.connman", target, "net.connman.Service")?
                    .call::<_, _, ()>(method, &())?;
            }
            "bluetooth-power" => {
                let path = self.adapter()?;
                self.proxy("org.bluez", &path, "org.bluez.Adapter1")?
                    .set_property("Powered", parse_bool(value)?)?;
            }
            "bluetooth-scan" => {
                let adapter = self.adapter()?;
                let p = self.proxy("org.bluez", &adapter, "org.bluez.Adapter1")?;
                p.call::<_, _, ()>("StartDiscovery", &())?;
                drop(p);
                let connection = self.connection.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(20));
                    if let Ok(p) =
                        Proxy::new(&connection, "org.bluez", adapter, "org.bluez.Adapter1")
                    {
                        let _: zbus::Result<()> = p.call("StopDiscovery", &());
                    }
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
            "brightness" => {
                let n = percent(value)?.max(1);
                run(
                    "brightnessctl",
                    &["--class=backlight", "set", &format!("{n}%")],
                )?;
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
