pub mod apps;
pub mod backend;
pub mod launcher;
pub mod notifications;
pub mod theme;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    time::Duration,
};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Network {
    /// SSID in hexadecimal: names need not be UTF-8 and one network spans
    /// several access points.
    pub id: String,
    pub name: String,
    pub signal: u8,
    /// none, owe, wep, psk, sae or ieee8021x.
    pub security: String,
    /// connecting, connected, disconnecting, failed, or empty when idle.
    pub state: String,
    pub saved: bool,
}
impl Network {
    pub fn connected(&self) -> bool {
        self.state == "connected"
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Device {
    pub path: String,
    pub name: String,
    pub address: String,
    pub connected: bool,
    pub paired: bool,
    pub trusted: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AudioDevice {
    pub name: String,
    pub description: String,
}
/// Playback stream of one application (a PulseAudio sink input).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AppStream {
    pub id: u32,
    pub name: String,
    pub detail: String,
    pub icon: Option<String>,
    pub volume: u8,
    pub muted: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Prompt {
    pub id: u64,
    pub title: String,
    pub detail: String,
    pub fields: Vec<String>,
    pub display_only: bool,
}
/// hyprsunset blue-light filter: whether it tints the screen, and how warm.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct NightLight {
    pub enabled: bool,
    pub temperature: u16,
}
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Snapshot {
    pub wifi: bool,
    pub networks: Vec<Network>,
    pub wifi_error: Option<String>,
    pub bluetooth: bool,
    pub devices: Vec<Device>,
    pub bluetooth_error: Option<String>,
    pub volume: Option<u8>,
    pub muted: bool,
    pub microphone: Option<u8>,
    pub mic_muted: bool,
    pub outputs: Vec<AudioDevice>,
    pub inputs: Vec<AudioDevice>,
    pub default_output: String,
    pub default_input: String,
    #[serde(default)]
    pub streams: Vec<AppStream>,
    pub brightness: Option<u8>,
    pub audio_error: Option<String>,
    pub brightness_error: Option<String>,
    #[serde(default)]
    pub night_light: Option<NightLight>,
    #[serde(default)]
    pub night_light_error: Option<String>,
    pub settings: theme::Settings,
    pub prompt: Option<Prompt>,
    #[serde(default)]
    pub applications_revision: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Status,
    Applications,
    Launch {
        id: String,
    },
    Action {
        op: String,
        #[serde(default)]
        target: String,
        #[serde(default)]
        value: String,
    },
    Answer {
        id: u64,
        values: std::collections::HashMap<String, String>,
        accepted: bool,
    },
    Theme {
        settings: theme::Settings,
    },
}
#[derive(Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    pub error: Option<String>,
    pub state: Option<Snapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalogue: Option<launcher::Catalogue>,
}
impl Reply {
    pub fn result(result: Result<()>) -> Self {
        match result {
            Ok(()) => Self {
                ok: true,
                error: None,
                state: None,
                catalogue: None,
            },
            Err(e) => Self {
                ok: false,
                error: Some(e.to_string()),
                state: None,
                catalogue: None,
            },
        }
    }
}
pub fn runtime() -> Result<PathBuf> {
    let base =
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?);
    let dir = base.join("nexus");
    fs::create_dir_all(&dir)?;
    let meta = fs::symlink_metadata(&dir)?;
    if !meta.is_dir() || meta.uid() != fs::metadata(&base)?.uid() {
        bail!("Unsafe Nexus runtime directory");
    }
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}
pub fn listener(name: &str) -> Result<(UnixListener, fs::File)> {
    use fs2::FileExt;
    let dir = runtime()?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(format!("{name}.lock")))?;
    lock.try_lock_exclusive()
        .context("Nexus is already running")?;
    let path = dir.join(format!("{name}.sock"));
    if path.exists() {
        fs::remove_file(&path)?;
    }
    let server = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok((server, lock))
}
pub fn read_line(stream: &UnixStream) -> Result<String> {
    stream.set_read_timeout(Some(Duration::from_secs(130)))?;
    let mut line = String::new();
    BufReader::new(stream)
        .take(1024 * 1024)
        .read_line(&mut line)?;
    if !line.ends_with('\n') {
        bail!("Invalid or oversized IPC message");
    }
    Ok(line)
}
pub fn request(request: &Request) -> Result<Reply> {
    let mut stream = UnixStream::connect(runtime()?.join("daemon.sock"))
        .context("Nexus daemon is not running")?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    serde_json::to_writer(&mut stream, request)?;
    stream.write_all(b"\n")?;
    Ok(serde_json::from_str(&read_line(&stream)?)?)
}
pub fn ensure_daemon() -> Result<()> {
    if request(&Request::Status).is_ok() {
        return Ok(());
    }
    let exe = std::env::current_exe()?.with_file_name("nexusd");
    std::process::Command::new(exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    for _ in 0..50 {
        if request(&Request::Status).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("Could not start nexusd")
}
