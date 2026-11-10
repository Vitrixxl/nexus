//! The phone link. The Nexus Android app connects to the daemon over Tailscale
//! and keeps one TCP connection open: newline-delimited JSON both ways. The
//! phone pushes its notifications; the daemon sends requests (`ls`, `read`,
//! `write`, `sms_threads`, `sms_list`, `sms_send`, `dismiss`…) that the phone
//! answers by id; `notes_*` requests keep Nexus Notes in step (see
//! [`crate::notesync`]). The same port serves the page the pairing QR code opens,
//! which hands the address and token to the app and offers the APK.
//!
//! The token in `$XDG_CONFIG_HOME/nexus/phone.json`, made on the first run and
//! never changed, is the only credential: the QR code stays the same, and any
//! phone that scanned it reconnects with a tap. Deleting the file revokes them
//! all. The listener binds the Tailscale address alone, never the LAN.
use crate::calendar::{self, Change};
use crate::notesync::{self, Side};
use crate::vault;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

pub const PORT: u16 = 47913;
/// Where `nexus phone mirror` listens for the screen, in the runtime directory.
pub const CAST_SOCKET: &str = "phone-cast.sock";
const CHUNK: u64 = 1 << 20;
const MAX_LINE: u64 = 8 << 20;
/// What a connection may send before it has shown the token, in bytes and time.
const MAX_HELLO: u64 = 4 << 10;
const HELLO_WAIT: Duration = Duration::from_secs(10);
/// Connections at a time that have not shown it yet.
const MAX_GREETING: usize = 16;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PhoneState {
    /// What the pairing QR code holds, once the listener is up.
    pub pair_url: Option<String>,
    /// The connected phone.
    pub name: Option<String>,
    /// What the phone last reported of itself: battery, storage, memory, CPU.
    #[serde(default)]
    pub info: Option<Value>,
    pub error: Option<String>,
    /// What the phone shows in its notification shade, newest first.
    #[serde(default)]
    pub notifications: Vec<PhoneNotification>,
    /// The phone that connected last, kept across restarts: the one to wait for.
    #[serde(default)]
    pub last: Option<String>,
    /// When it was last connected, in milliseconds since the epoch.
    #[serde(default)]
    pub seen: Option<i64>,
    /// Disconnect let it go: it waits to be tapped in the app.
    #[serde(default)]
    pub released: bool,
    #[serde(default)]
    pub notes: NotesSync,
    #[serde(default)]
    pub calendar: CalendarSync,
    #[serde(default)]
    pub vault: VaultSync,
}
/// Where the password sync stands. Never anything of the vault itself.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct VaultSync {
    /// This desktop's answer for the connected phone: ask, yes or no.
    pub here: String,
    /// The phone's answer, or empty while unknown.
    pub there: String,
    /// waiting, synced, different, or empty.
    pub status: String,
    pub error: Option<String>,
}
/// Where the calendar stands: the UI reads the events again when it moves.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CalendarSync {
    pub revision: u64,
    pub error: Option<String>,
    /// Changes waiting for the phone.
    pub pending: usize,
    /// Whether the phone has answered once: the copy here means something.
    pub synced: bool,
}
/// Where the notes sync stands, for the Phone page and the phone.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct NotesSync {
    pub on: bool,
    pub busy: bool,
    /// When the two sides last matched, in milliseconds since the epoch.
    pub last: Option<i64>,
    /// The copies kept beside pages both sides changed, while they exist.
    pub conflicts: Vec<String>,
    pub error: Option<String>,
}
/// What the notes sync keeps across restarts.
#[derive(Default, Serialize, Deserialize)]
struct NotesSaved {
    #[serde(default)]
    off: bool,
    /// The phone's notes store last synced with.
    #[serde(default)]
    current: String,
    /// By the id of the phone's notes store: a phone that loses its notes
    /// comes back with a new id, and syncs as for the first time rather than
    /// passing for having deleted them all.
    #[serde(default)]
    stores: HashMap<String, NotesStore>,
    /// Conflict copies in the desktop's notes.
    #[serde(default)]
    conflicts: Vec<String>,
}
#[derive(Default, Serialize, Deserialize)]
struct NotesStore {
    base: notesync::Manifest,
    last: i64,
}
/// A text in the clipboard history.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Clip {
    pub text: String,
    /// Milliseconds since the epoch.
    pub time: i64,
    /// Copied on the phone.
    pub phone: bool,
}
const HISTORY: usize = 50;
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PhoneNotification {
    pub key: String,
    pub app: String,
    pub title: String,
    pub text: String,
    /// Milliseconds since the epoch.
    pub time: i64,
    /// An icon name, or the path of the app's icon.
    pub icon: String,
}
struct Link {
    out: Mutex<TcpStream>,
    pending: Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>,
}
impl Link {
    fn send(&self, v: &Value) -> Result<()> {
        let mut out = self.out.lock().unwrap();
        let mut line = serde_json::to_vec(v)?;
        line.push(b'\n');
        out.write_all(&line)?;
        Ok(())
    }
}
#[derive(Default)]
pub struct Phone {
    token: String,
    link: Mutex<Option<Arc<Link>>>,
    next_id: AtomicU64,
    /// Connections that have not said who they are yet.
    greeting: AtomicUsize,
    pub state: Mutex<PhoneState>,
    /// Desktop notification id of each phone notification, and its post
    /// time, by its key: the phone sends them all again on reconnecting.
    /// Kept on disk, so a restarted daemon does not show them again either.
    shown: Mutex<HashMap<String, (u32, i64)>>,
    /// The clipboard text last shared either way, so that it does not echo.
    clip: Mutex<String>,
    /// The texts copied lately, here or on the phone, newest first. Kept in
    /// memory only: copies often hold what should not reach the disk.
    history: Mutex<Vec<Clip>>,
    bus: OnceLock<Option<zbus::blocking::Connection>>,
    /// Set when the notes should sync soon: the phone connected or changed some.
    kick: (Mutex<bool>, Condvar),
    /// The phone's calendars, and the changes on their way to them.
    calendar: Mutex<calendar::Cache>,
    /// Set when the calendar should sync: connected, changed on either side.
    calendar_kick: (Mutex<bool>, Condvar),
    /// Set when the vault should sync: connected, or either side changed.
    vault_kick: (Mutex<bool>, Condvar),
}

fn config() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
        .join("nexus/phone.json")
}
fn state_file(name: &str) -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
        })
        .join("nexus")
        .join(name)
}
fn seen_path() -> PathBuf {
    state_file("phone-notifications.json")
}
/// The phone that connected last and when: `[name, ms]`.
fn last_path() -> PathBuf {
    state_file("phone-last.json")
}
fn save_last(s: &PhoneState) {
    let path = last_path();
    let _ = fs::create_dir_all(path.parent().unwrap());
    let _ = fs::write(path, json!([s.last, s.seen]).to_string());
}
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}
/// The newest 500 by post time: removals on the phone keep it short anyway.
fn save_seen(shown: &HashMap<String, (u32, i64)>) {
    let mut entries: Vec<_> = shown.iter().collect();
    entries.sort_by_key(|(_, (_, time))| std::cmp::Reverse(*time));
    entries.truncate(500);
    let path = seen_path();
    let _ = fs::create_dir_all(path.parent().unwrap());
    let _ = fs::write(path, serde_json::to_vec(&entries).unwrap_or_default());
}
fn new_token() -> Result<String> {
    // A new token unpairs every phone: none is waited for any more, and
    // what was kept of its messages goes with it.
    let _ = fs::remove_file(last_path());
    let _ = fs::remove_dir_all(mms_cache());
    let mut bytes = [0u8; 24];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    save_token(&token)?;
    Ok(token)
}
fn save_token(token: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = config();
    fs::create_dir_all(path.parent().unwrap())?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(json!({ "token": token }).to_string().as_bytes())?;
    Ok(())
}
fn load_token() -> Result<String> {
    let saved = fs::read_to_string(config())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| Some(v.get("token")?.as_str()?.to_owned()));
    match saved {
        Some(t) if t.len() >= 32 => Ok(t),
        _ => new_token(),
    }
}
/// Compares without stopping at the first difference.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |d, (x, y)| d | (x ^ y)) == 0
}
/// Where to listen. Lab only: `NEXUS_PHONE_LISTEN` names the address instead,
/// for a stand-in phone on this machine.
fn listen_ip() -> Result<String> {
    #[cfg(feature = "lab")]
    if let Ok(ip) = std::env::var("NEXUS_PHONE_LISTEN") {
        return Ok(ip);
    }
    tailscale_ip()
}
fn tailscale_ip() -> Result<String> {
    let out = Command::new("tailscale")
        .args(["ip", "-4"])
        .output()
        .context(
            "Tailscale is not installed. Install it and sign in to the tailnet your phone uses.",
        )?;
    let ip = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    if !out.status.success() || ip.is_empty() {
        bail!("Tailscale is not connected. Run `tailscale up`; this retries by itself.");
    }
    Ok(ip)
}
/// Whether the screen is locked: `nexus lock` or hyprlock runs. Read from
/// /proc rather than by taking the lock's file lock, which would make a
/// `nexus lock` started at that instant think the screen already locked.
pub fn screen_locked() -> bool {
    let Ok(procs) = fs::read_dir("/proc") else {
        return false;
    };
    // comm first: reading a cmdline takes the lock of that process's memory.
    procs.flatten().any(|p| {
        fs::read(p.path().join("comm")).is_ok_and(|c| matches!(&c[..], b"nexus\n" | b"hyprlock\n"))
            && fs::read(p.path().join("cmdline")).is_ok_and(|c| is_locker(&c))
    })
}
/// A /proc cmdline: NUL-separated arguments.
fn is_locker(cmdline: &[u8]) -> bool {
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    let name = args[0].rsplit(|b| *b == b'/').next().unwrap_or_default();
    name == b"hyprlock"
        || (name == b"nexus"
            && args.get(1) == Some(&&b"lock"[..])
            && !args.contains(&&b"--preview"[..]))
}
/// The file `name` in `dir`, or `name (1).ext` and so on if that is taken, created
/// empty so that nothing else takes it before the caller renames over it.
fn claim_name(dir: &Path, name: &str) -> Result<PathBuf> {
    let (stem, ext) = {
        let path = Path::new(name);
        (
            path.file_stem().unwrap_or_default().to_string_lossy(),
            path.extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_default(),
        )
    };
    for i in 0..10_000 {
        let path = dir.join(match i {
            0 => name.to_owned(),
            _ => format!("{stem} ({i}){ext}"),
        });
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    bail!("No free name left")
}
pub fn hostname() -> String {
    fs::read_to_string("/etc/hostname")
        .map(|h| h.trim().to_owned())
        .unwrap_or_else(|_| "Nexus".into())
}
pub fn apk() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join(".local/share/nexus/nexus-phone.apk")
}
/// Lab only: `NEXUS_PHONE_FAKE` names a JSON file standing in for the phone,
/// `{"state": PhoneState, "replies": {op: data | {"error": text}}}`, read anew
/// each time. No listener, no link: nothing reaches a real phone.
#[cfg(feature = "lab")]
fn fake() -> Option<Value> {
    serde_json::from_slice(&fs::read(std::env::var_os("NEXUS_PHONE_FAKE")?).ok()?).ok()
}
fn read_line(r: &mut impl BufRead, limit: u64) -> Result<Option<String>> {
    let mut line = String::new();
    if r.take(limit).read_line(&mut line)? == 0 {
        return Ok(None);
    }
    if !line.ends_with('\n') {
        bail!("Oversized message");
    }
    Ok(Some(line))
}

impl Phone {
    pub fn start() -> Result<Arc<Self>> {
        let shown = fs::read(seen_path())
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<(String, (u32, i64))>>(&b).ok())
            .unwrap_or_default();
        let phone = Arc::new(Self {
            token: load_token()?,
            shown: Mutex::new(shown.into_iter().collect()),
            ..Self::default()
        });
        if let Ok((last, seen)) = fs::read(last_path())
            .map_err(anyhow::Error::from)
            .and_then(|b| Ok(serde_json::from_slice::<(Option<String>, Option<i64>)>(&b)?))
        {
            let mut s = phone.state.lock().unwrap();
            (s.last, s.seen) = (last, seen);
        }
        let p = phone.clone();
        thread::spawn(move || p.sync_vault());
        #[cfg(feature = "lab")]
        if std::env::var_os("NEXUS_PHONE_FAKE").is_some() {
            let p = phone.clone();
            thread::spawn(move || {
                loop {
                    if let Some(mut s) = fake()
                        .and_then(|f| serde_json::from_value::<PhoneState>(f["state"].clone()).ok())
                    {
                        let mut current = p.state.lock().unwrap();
                        // The sync worker runs against the fake phone and owns this.
                        s.vault = std::mem::take(&mut current.vault);
                        *current = s;
                    }
                    thread::sleep(Duration::from_secs(1));
                }
            });
            return Ok(phone);
        }
        let p = phone.clone();
        thread::spawn(move || {
            // ponytail: binds the address Tailscale had at startup; a new one
            // takes a daemon restart.
            loop {
                let bound = listen_ip().and_then(|ip| {
                    let listener = TcpListener::bind((ip.as_str(), PORT)).map_err(|e| {
                        anyhow::anyhow!(match e.kind() {
                            std::io::ErrorKind::AddrInUse => format!(
                                "Port {PORT} is already in use. Another Nexus daemon probably holds it: stop it, and this retries by itself."
                            ),
                            _ => format!("Can't listen for your phone: {e}. This retries every 10 seconds."),
                        })
                    })?;
                    Ok((listener, ip))
                });
                match bound {
                    Ok((listener, ip)) => {
                        p.set_url(&ip);
                        for stream in listener.incoming().flatten() {
                            if !p.admit() {
                                continue;
                            }
                            let p = p.clone();
                            thread::spawn(move || p.serve(stream));
                        }
                    }
                    Err(e) => p.state.lock().unwrap().error = Some(e.to_string()),
                }
                thread::sleep(Duration::from_secs(10));
            }
        });
        // Locked, the desktop shows no notifications and nothing asks for the
        // phone's data: the link drops, and the phone waits until it unlocks.
        let p = phone.clone();
        thread::spawn(move || {
            loop {
                // Going through every process: only while there is a link to drop.
                if p.link.lock().unwrap().is_some()
                    && screen_locked()
                    && let Some(link) = p.link.lock().unwrap().take()
                {
                    Self::release(&link, "locked");
                }
                thread::sleep(Duration::from_secs(2));
            }
        });
        let p = phone.clone();
        thread::spawn(move || {
            if let Err(e) = p.follow_restarts() {
                eprintln!("Phone notifications: {e}");
            }
        });
        let p = phone.clone();
        thread::spawn(move || {
            if let Err(e) = p.follow_dismissals() {
                eprintln!("Phone notifications: {e}");
            }
        });
        let p = phone.clone();
        thread::spawn(move || p.sync_notes());
        let p = phone.clone();
        thread::spawn(move || p.sync_calendar());
        let p = phone.clone();
        thread::spawn(move || {
            loop {
                if let Err(e) = p.share_clipboard() {
                    eprintln!("Clipboard: {e}");
                }
                thread::sleep(Duration::from_secs(10));
            }
        });
        Ok(phone)
    }
    fn set_url(&self, ip: &str) {
        let token = &self.token;
        let mut s = self.state.lock().unwrap();
        s.pair_url = Some(format!("http://{ip}:{PORT}/pair?t={token}"));
        s.error = None;
    }
    /// Cuts the phone off. The token never changes, so it stays paired and
    /// reconnects when tapped in the app; `reason` tells it not to retry.
    fn release(link: &Link, reason: &str) {
        let _ = link.send(&json!({ "t": "bye", "reason": reason }));
        let _ = link.out.lock().unwrap().shutdown(Shutdown::Both);
    }
    pub fn disconnect(&self) {
        if let Some(link) = self.link.lock().unwrap().take() {
            Self::release(&link, "released");
            self.state.lock().unwrap().released = true;
        }
    }

    /// Whether another connection may wait to say who it is.
    fn admit(&self) -> bool {
        // Only the accepting thread adds, so this cannot overshoot.
        let room = self.greeting.load(Ordering::Relaxed) < MAX_GREETING;
        if room {
            self.greeting.fetch_add(1, Ordering::Relaxed);
        }
        room
    }
    /// The first line of an admitted connection.
    // ponytail: the wait is per read, so a peer trickling bytes keeps its slot for
    // long; a deadline if the tailnet ever holds anything hostile.
    fn greet(stream: &TcpStream) -> Option<(BufReader<TcpStream>, String)> {
        let _ = stream.set_read_timeout(Some(HELLO_WAIT));
        let mut reader = BufReader::new(stream.try_clone().ok()?);
        let first = read_line(&mut reader, MAX_HELLO).ok()??;
        Some((reader, first))
    }
    fn serve(self: &Arc<Self>, stream: TcpStream) {
        let _ = stream.set_write_timeout(Some(Duration::from_secs(20)));
        let greeted = Self::greet(&stream);
        // What follows answers at once, or holds the token.
        self.greeting.fetch_sub(1, Ordering::Relaxed);
        let Some((mut reader, first)) = greeted else {
            return;
        };
        if first.starts_with("GET ") {
            let _ = self.http(&first, stream);
            return;
        }
        let hello: Value = serde_json::from_str(&first).unwrap_or_default();
        let token = hello["token"].as_str().unwrap_or_default();
        if !same(token, &self.token) {
            return;
        }
        // The screen's frames, straight to the viewer that asked for them. The
        // phone repeats one every few seconds, so a dead link times out.
        if hello["t"] == "cast" {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
            if !screen_locked()
                && let Ok(mut viewer) = crate::runtime().and_then(|d| {
                    Ok(std::os::unix::net::UnixStream::connect(
                        d.join(CAST_SOCKET),
                    )?)
                })
            {
                let _ = std::io::copy(&mut reader, &mut viewer);
            }
            return;
        }
        if hello["t"] != "hello" {
            return;
        }
        let _ = stream.set_read_timeout(Some(Duration::from_secs(90)));
        let link = Arc::new(Link {
            out: Mutex::new(stream),
            pending: Mutex::default(),
        });
        if screen_locked() {
            let _ = link.send(&json!({ "t": "bye", "reason": "locked" }));
            return;
        }
        if link
            .send(&json!({ "t": "welcome", "name": hostname() }))
            .is_err()
        {
            return;
        }
        // The phone tapped last wins; the other one waits to be tapped.
        if let Some(old) = self.link.lock().unwrap().replace(link.clone()) {
            Self::release(&old, "replaced");
        }
        {
            let mut s = self.state.lock().unwrap();
            let name = hello["name"].as_str().unwrap_or("Phone").to_owned();
            (s.name, s.last) = (Some(name.clone()), Some(name));
            (s.seen, s.released) = (Some(now_ms()), false);
            save_last(&s);
            let _ = link.send(&json!({ "t": "notes_state", "state": s.notes }));
        }
        self.kick_notes();
        self.kick_calendar();
        self.kick_vault();
        let ping = Arc::downgrade(&link);
        thread::spawn(move || {
            while let Some(link) = ping.upgrade() {
                if link.send(&json!({ "t": "ping" })).is_err() {
                    break;
                }
                drop(link);
                thread::sleep(Duration::from_secs(30));
            }
        });
        while let Ok(Some(line)) = read_line(&mut reader, MAX_LINE) {
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match msg["t"].as_str() {
                Some("notif") => self.notify(&msg),
                Some("notif_gone") => self.close(msg["key"].as_str().unwrap_or_default()),
                Some("clip") => self.copy(msg["text"].as_str().unwrap_or_default()),
                Some("notes_changed") => self.kick_notes(),
                Some("calendar_changed") => self.kick_calendar(),
                Some("vault_changed") => self.kick_vault(),
                // On connecting, then with each answer to the daemon's ping.
                Some("info" | "pong") if msg["info"].is_object() => {
                    self.state.lock().unwrap().info = Some(msg["info"].clone());
                }
                Some("res") => {
                    let id = msg["id"].as_u64().unwrap_or_default();
                    if let Some(tx) = link.pending.lock().unwrap().remove(&id) {
                        let _ = tx.send(if msg["ok"] == true {
                            Ok(msg["data"].clone())
                        } else {
                            Err(msg["error"].as_str().unwrap_or("Phone error").to_owned())
                        });
                    }
                }
                _ => {}
            }
        }
        // Waiting requests fail at once rather than at their timeout.
        link.pending.lock().unwrap().clear();
        let mut current = self.link.lock().unwrap();
        // Unless another connection took its place; locking or disconnecting
        // took it away already.
        if current.as_ref().is_none_or(|l| Arc::ptr_eq(l, &link)) {
            *current = None;
            let mut s = self.state.lock().unwrap();
            s.name = None;
            s.info = None;
            s.notifications.clear();
            s.seen = Some(now_ms());
            save_last(&s);
        }
    }
    /// The pairing page and the APK, for a browser holding the token.
    fn http(&self, first: &str, mut stream: TcpStream) -> Result<()> {
        let target = first.split_whitespace().nth(1).unwrap_or("/");
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let token = &self.token;
        let given = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("t="))
            .unwrap_or_default();
        let reply = |stream: &mut TcpStream, status: &str, kind: &str, body: &[u8]| {
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )?;
            stream.write_all(body)
        };
        if !same(given, token) {
            reply(
                &mut stream,
                "403 Forbidden",
                "text/plain",
                b"Scan the code Nexus shows.",
            )?;
            return Ok(());
        }
        match path {
            "/pair" => {
                let host = stream.local_addr()?.ip();
                let intent = format!(
                    "intent://pair?host={host}&port={PORT}&token={token}&name={}#Intent;scheme=nexus;package=io.github.vitrixxl.nexus;end",
                    hostname()
                );
                let host_name = hostname();
                let page = format!(
                    r#"<!doctype html><meta name=viewport content="width=device-width,initial-scale=1"><meta name=color-scheme content="dark light"><title>Pair with Nexus</title>
<style>:root{{--bg:#141416;--surface:#1f1f22;--fg:#f4f4f5;--muted:#b4b4bb}}@media(prefers-color-scheme:light){{:root{{--bg:#f2f2f5;--surface:#fff;--fg:#1d1d1f;--muted:#62626a}}}}
body{{font:16px system-ui,sans-serif;background:var(--bg);color:var(--fg);display:grid;gap:12px;padding:40px 20px;max-width:420px;margin:auto}}h1{{font-size:28px;margin:0}}p{{color:var(--muted);margin:0 0 12px;line-height:1.45}}
a{{display:block;padding:16px;border-radius:16px;text-align:center;text-decoration:none;font-weight:600}}.p{{background:#0a84ff;color:#fff}}.s{{background:var(--surface);color:var(--fg)}}small{{color:var(--muted);font-size:14px;line-height:1.45}}</style>
<h1>Pair with {host_name}</h1><p>Share this phone's notifications, files, messages and clipboard with your desktop.</p>
<a class=p href="{intent}">Pair this phone</a><a class=s href="/nexus.apk?t={token}">Install the Nexus app first</a>
<small>No app yet? Install it, then come back here and tap Pair. The app then lists what to allow.</small>"#
                );
                reply(
                    &mut stream,
                    "200 OK",
                    "text/html; charset=utf-8",
                    page.as_bytes(),
                )?;
            }
            "/nexus.apk" => match fs::read(apk()) {
                Ok(bytes) => reply(
                    &mut stream,
                    "200 OK",
                    "application/vnd.android.package-archive",
                    &bytes,
                )?,
                Err(_) => reply(
                    &mut stream,
                    "404 Not Found",
                    "text/plain",
                    b"Build the app first.",
                )?,
            },
            // What the phone compares with the app it runs, to offer the update.
            "/nexus.apk.sha256" => match fs::read(apk()) {
                Ok(bytes) => reply(
                    &mut stream,
                    "200 OK",
                    "text/plain",
                    crate::notesync::hash_bytes(&bytes).as_bytes(),
                )?,
                Err(_) => reply(&mut stream, "404 Not Found", "text/plain", b"")?,
            },
            _ => reply(&mut stream, "404 Not Found", "text/plain", b"")?,
        }
        Ok(())
    }

    /// Sends `op` to the phone and waits for its answer.
    pub fn request(&self, op: &str, args: Value) -> Result<Value> {
        #[cfg(feature = "lab")]
        if let Some(f) = fake() {
            let r = &f["replies"][op];
            return match r["error"].as_str() {
                Some(e) => Err(anyhow::anyhow!("{e}")),
                None if r.is_null() => bail!("No phone is connected"),
                None => Ok(r.clone()),
            };
        }
        let link = self
            .link
            .lock()
            .unwrap()
            .clone()
            .context("No phone is connected")?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = mpsc::channel();
        link.pending.lock().unwrap().insert(id, tx);
        let mut msg = json!({ "t": "req", "id": id, "op": op });
        if let (Some(m), Value::Object(args)) = (msg.as_object_mut(), args) {
            m.extend(args);
        }
        link.send(&msg)?;
        let reply = rx.recv_timeout(Duration::from_secs(30));
        link.pending.lock().unwrap().remove(&id);
        match reply {
            Ok(r) => r.map_err(anyhow::Error::msg),
            Err(_) => bail!("The phone did not answer"),
        }
    }
    /// Copies the phone's file at `remote` into `dir`, in the background.
    pub fn pull(self: &Arc<Self>, remote: String, dir: PathBuf) {
        let p = self.clone();
        thread::spawn(move || {
            let name = Path::new(&remote)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".into());
            // Its own: the same file may be pulled twice at once.
            let id = p.next_id.fetch_add(1, Ordering::Relaxed);
            let part = dir.join(format!(".{name}.{id}.part"));
            let result = (|| -> Result<PathBuf> {
                let mut f = fs::File::create(&part)?;
                let mut offset = 0;
                loop {
                    let r = p.request(
                        "read",
                        json!({ "path": remote, "offset": offset, "len": CHUNK }),
                    )?;
                    let bytes = gtk::glib::base64_decode(r["b64"].as_str().unwrap_or_default());
                    f.write_all(&bytes)?;
                    offset += bytes.len() as u64;
                    if bytes.is_empty() || offset >= r["size"].as_u64().unwrap_or(0) {
                        break;
                    }
                }
                let dest = claim_name(&dir, &name)?;
                fs::rename(&part, &dest)?;
                Ok(dest)
            })();
            if result.is_err() {
                let _ = fs::remove_file(&part);
            }
            p.desktop_notice(&match result {
                Ok(dest) => format!("Saved {}", dest.display()),
                Err(e) => format!("Could not copy {name}: {e}"),
            });
        });
    }
    /// Copies the local file `src` into the phone's directory `dir`.
    pub fn push(self: &Arc<Self>, src: PathBuf, dir: String) {
        let p = self.clone();
        thread::spawn(move || {
            let name = src
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let remote = format!("{}/{name}", dir.trim_end_matches('/'));
            let result = (|| -> Result<()> {
                let mut f = fs::File::open(&src)?;
                let mut buf = vec![0; CHUNK as usize];
                let mut offset = 0u64;
                loop {
                    let n = f.read(&mut buf)?;
                    if n == 0 && offset > 0 {
                        break;
                    }
                    let b64 = gtk::glib::base64_encode(&buf[..n]);
                    p.request(
                        "write",
                        json!({ "path": remote, "offset": offset, "b64": b64.as_str() }),
                    )?;
                    offset += n as u64;
                    if n == 0 {
                        break;
                    }
                }
                Ok(())
            })();
            p.desktop_notice(&match result {
                Ok(()) => format!("Sent {name} to the phone"),
                Err(e) => format!("Could not send {name}: {e}"),
            });
        });
    }

    fn kick_calendar(&self) {
        *self.calendar_kick.0.lock().unwrap() = true;
        self.calendar_kick.1.notify_all();
    }
    fn linked(&self) -> bool {
        #[cfg(feature = "lab")]
        if fake().is_some() {
            return true;
        }
        self.link.lock().unwrap().is_some()
    }
    /// The events between two instants, as they stand here, and the calendars.
    /// Far from today only the phone has them.
    pub fn calendar_events(&self, from: i64, to: i64) -> Value {
        let (mut events, calendars, covered) = {
            let c = self.calendar.lock().unwrap();
            (
                c.between(from, to),
                c.calendars.clone(),
                from >= c.from && to <= c.to,
            )
        };
        if !covered
            && let Ok(listing) = self.request("calendar_list", json!({ "from": from, "to": to }))
            && let Ok(mut far) = serde_json::from_value::<calendar::Cache>(listing)
        {
            calendar::sort(&mut far.events);
            events = far.events;
        }
        json!({ "events": events, "calendars": calendars })
    }
    /// A change from the desktop: shown at once, sent as soon as it can be.
    pub fn calendar_change(&self, change: Change) -> Result<()> {
        let pending = {
            let mut c = self.calendar.lock().unwrap();
            c.apply(change);
            Self::save_calendar(&c)?;
            c.pending.len()
        };
        let mut s = self.state.lock().unwrap();
        s.calendar.revision += 1;
        s.calendar.pending = pending;
        drop(s);
        self.kick_calendar();
        Ok(())
    }
    fn save_calendar(cache: &calendar::Cache) -> Result<()> {
        let path = state_file("calendar.json");
        let part = path.with_extension("part");
        fs::create_dir_all(path.parent().context("No state folder")?)?;
        fs::write(&part, serde_json::to_vec(cache)?)?;
        fs::rename(&part, &path)?;
        Ok(())
    }
    /// Follows the phone's calendars, for as long as the daemon runs: when it
    /// connects or says they changed, when the desktop changes one, and every
    /// quarter of an hour so that the days covered keep moving on.
    fn sync_calendar(&self) {
        if let Some(saved) = fs::read(state_file("calendar.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<calendar::Cache>(&b).ok())
        {
            let mut s = self.state.lock().unwrap();
            s.calendar.pending = saved.pending.len();
            s.calendar.synced = !saved.calendars.is_empty();
            drop(s);
            *self.calendar.lock().unwrap() = saved;
        }
        loop {
            {
                let (lock, cond) = &self.calendar_kick;
                let (mut kicked, _) = cond
                    .wait_timeout_while(lock.lock().unwrap(), Duration::from_secs(900), |k| !*k)
                    .unwrap();
                *kicked = false;
            }
            // A burst of changes from the phone's own sync makes one listing.
            thread::sleep(Duration::from_millis(300));
            if self.linked()
                && let Err(e) = self.sync_calendar_once()
            {
                self.state.lock().unwrap().calendar.error = Some(format!("{e:#}"));
            }
        }
    }
    fn calendar_listing(&self) -> Result<calendar::Cache> {
        const DAY: i64 = 86_400_000;
        let (from, to) = (now_ms() - 190 * DAY, now_ms() + 560 * DAY);
        let listing = self.request("calendar_list", json!({ "from": from, "to": to }))?;
        let mut cache: calendar::Cache = serde_json::from_value(listing)?;
        (cache.from, cache.to) = (from, to);
        Ok(cache)
    }
    fn sync_calendar_once(&self) -> Result<()> {
        // First: a phone that refuses the listing has refused the changes too.
        let mut listing = self.calendar_listing()?;
        let changes = self.calendar.lock().unwrap().pending.clone();
        let mut refused = None;
        for change in &changes {
            let (op, event) = match change {
                Change::Save(e) => ("calendar_save", e),
                Change::Delete(e) => ("calendar_delete", e),
            };
            let mut event = event.clone();
            if event.id.starts_with(calendar::LOCAL) {
                event.id.clear();
            }
            match self.request(op, json!({ "event": event })) {
                Ok(_) => {}
                // Gone, or not answering: the change waits for the next time.
                Err(e) if !self.linked() || e.to_string() == "The phone did not answer" => {
                    return Err(e);
                }
                // Refused: it would be refused forever.
                Err(e) => refused = Some(format!("{e:#}")),
            }
            let mut c = self.calendar.lock().unwrap();
            if let Some(i) = c.pending.iter().position(|p| p == change) {
                c.pending.remove(i);
            }
        }
        if !changes.is_empty() {
            listing = self.calendar_listing()?;
        }
        let mut c = self.calendar.lock().unwrap();
        let changed = c.replace(listing);
        let pending = c.pending.len();
        Self::save_calendar(&c)?;
        drop(c);
        let mut s = self.state.lock().unwrap();
        s.calendar.revision += u64::from(changed || !changes.is_empty());
        s.calendar.pending = pending;
        s.calendar.synced = true;
        s.calendar.error = refused;
        Ok(())
    }

    pub fn kick_vault(&self) {
        *self.vault_kick.0.lock().unwrap() = true;
        self.vault_kick.1.notify_all();
    }
    /// `{phone name: "yes" | "no"}`; a phone not in it has not been answered.
    // ponytail: phones are told apart by name: two with the same one share an
    // answer, and the link gives them no other identity.
    fn vault_answers() -> Map<String, Value> {
        fs::read(state_file("vault-sync.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
    fn vault_answer(phone: &str) -> String {
        match Self::vault_answers().get(phone).and_then(Value::as_str) {
            Some(a @ ("yes" | "no")) => a.to_owned(),
            _ => "ask".to_owned(),
        }
    }
    fn vault_peer(&self) -> Option<String> {
        self.linked()
            .then(|| self.state.lock().unwrap().name.clone())?
    }
    /// Answers the question for the connected phone: yes, no, or ask again.
    pub fn set_vault_answer(&self, answer: &str) -> Result<()> {
        let phone = self.vault_peer().context("No phone is connected")?;
        if !matches!(answer, "yes" | "no" | "ask") {
            bail!("Invalid answer");
        }
        let mut answers = Self::vault_answers();
        if answer == "ask" {
            answers.remove(&phone);
        } else {
            answers.insert(phone, answer.into());
        }
        let path = state_file("vault-sync.json");
        let part = path.with_extension("part");
        fs::create_dir_all(path.parent().context("No state folder")?)?;
        fs::write(&part, Value::Object(answers).to_string())?;
        fs::rename(&part, &path)?;
        self.kick_vault();
        Ok(())
    }
    /// The phone's answer, and an error unless both sides have said yes.
    fn vault_both_yes(&self) -> Result<()> {
        let phone = self.vault_peer().context("No phone is connected")?;
        let hello = self.request("vault_hello", json!({}))?;
        if Self::vault_answer(&phone) != "yes" || hello["consent"] != "yes" {
            bail!("Syncing has not been allowed on both sides");
        }
        Ok(())
    }
    /// Leaves the phone's vault file where the Passwords window can read it,
    /// for the "different vault" merge. The file stays off the daemon's IPC.
    pub fn vault_fetch_other(&self) -> Result<Value> {
        self.vault_both_yes()?;
        let pulled = self.request("vault_pull", json!({}))?;
        let file = vault::VaultFile::parse(pulled["vault"].clone())
            .context("The phone has no vault to merge")?;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(crate::runtime()?.join("vault-other.json"))?;
        f.write_all(&serde_json::to_vec(&file)?)?;
        Ok(Value::Null)
    }
    /// Gives the phone this desktop's vault to replace its own, after a merge.
    pub fn vault_adopt(&self) -> Result<Value> {
        self.vault_both_yes()?;
        let file = vault::load()?.context("There is no vault on this desktop")?;
        self.request("vault_push", json!({ "vault": file, "adopt": true }))?;
        self.kick_vault();
        Ok(Value::Null)
    }
    /// Ciphertext only: merging never needs the master password.
    fn sync_vault(&self) {
        let stamp = || {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(vault::path())
                .ok()
                .map(|m| (m.mtime(), m.mtime_nsec(), m.len()))
        };
        // Starts unset, so that the first turn syncs whatever the file does.
        let mut seen = None;
        let mut asked = false;
        loop {
            let kicked = {
                let (lock, cond) = &self.vault_kick;
                let (mut kicked, _) = cond
                    .wait_timeout_while(lock.lock().unwrap(), Duration::from_secs(5), |k| !*k)
                    .unwrap();
                std::mem::replace(&mut *kicked, false)
            };
            let Some(phone) = self.vault_peer() else {
                asked = false;
                seen = None;
                self.state.lock().unwrap().vault = VaultSync::default();
                continue;
            };
            let now = Some(stamp());
            if !kicked && now == seen {
                continue;
            }
            // A burst of writes makes one sync.
            thread::sleep(Duration::from_millis(300));
            if let Err(e) = self.sync_vault_once(&phone, &mut asked) {
                self.state.lock().unwrap().vault.error = Some(format!("{e:#}"));
            }
            seen = Some(stamp());
        }
    }
    fn sync_vault_once(&self, phone: &str, asked: &mut bool) -> Result<()> {
        let here = Self::vault_answer(phone);
        if here == "no" {
            self.state.lock().unwrap().vault = VaultSync {
                here,
                ..VaultSync::default()
            };
            return Ok(());
        }
        let hello = self.request("vault_hello", json!({}))?;
        let there = hello["consent"].as_str().unwrap_or("ask").to_owned();
        let mut sync = VaultSync {
            here: here.clone(),
            there: there.clone(),
            ..VaultSync::default()
        };
        let both_yes = here == "yes" && there == "yes";
        if !both_yes {
            if here == "ask" && !*asked {
                *asked = true;
                self.post(
                    "Passwords",
                    "dialog-password-symbolic",
                    &format!("Sync passwords with {phone}?"),
                    "Open Passwords to answer. Nothing moves until both sides say yes.",
                    0,
                    false,
                );
            }
            sync.status = "waiting".into();
            self.state.lock().unwrap().vault = sync;
            return Ok(());
        }
        let pulled = self.request("vault_pull", json!({}))?;
        let remote = match pulled["vault"].clone() {
            Value::Null => None,
            v => Some(vault::VaultFile::parse(v)?),
        };
        let (to_push, status) = vault::update(|local| match (local, remote) {
            (None, None) => Ok((None, (None, ""))),
            (None, Some(remote)) => Ok((Some(remote), (None, "synced"))),
            (Some(local), None) => Ok((None, (Some(local), "synced"))),
            (Some(local), Some(remote)) if local.id != remote.id => Ok((None, (None, "different"))),
            (Some(mut local), Some(remote)) => {
                let changed = local.merge(&remote)?;
                let push = (local != remote).then(|| local.clone());
                Ok((changed.then_some(local), (push, "synced")))
            }
        })?;
        if let Some(file) = to_push {
            self.request("vault_push", json!({ "vault": file, "adopt": false }))?;
        }
        sync.status = status.into();
        self.state.lock().unwrap().vault = sync;
        Ok(())
    }

    fn kick_notes(&self) {
        *self.kick.0.lock().unwrap() = true;
        self.kick.1.notify_all();
    }
    /// Turns the notes sync on or off; the phone hears of it at once.
    pub fn set_notes_sync(&self, on: bool) {
        let notes = {
            let mut s = self.state.lock().unwrap();
            s.notes.on = on;
            s.notes.clone()
        };
        if let Some(link) = self.link.lock().unwrap().clone() {
            let _ = link.send(&json!({ "t": "notes_state", "state": notes }));
        }
        self.kick_notes();
    }
    /// Shows `f`'s change of the notes sync's state, here and on the phone.
    fn notes_state(&self, f: impl FnOnce(&mut NotesSync)) {
        let notes = {
            let mut s = self.state.lock().unwrap();
            f(&mut s.notes);
            s.notes.clone()
        };
        if let Some(link) = self.link.lock().unwrap().clone() {
            let _ = link.send(&json!({ "t": "notes_state", "state": notes }));
        }
    }
    /// Syncs the notes when the phone connects or says it changed some, and
    /// when the desktop's change, looked at every 15 seconds while linked.
    fn sync_notes(&self) {
        let path = state_file("notes-sync.json");
        let mut saved: NotesSaved = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let save = |saved: &NotesSaved| {
            let _ = fs::create_dir_all(path.parent().unwrap());
            let _ = fs::write(&path, serde_json::to_vec(saved).unwrap_or_default());
        };
        self.notes_state(|n| {
            n.on = !saved.off;
            n.last = saved
                .stores
                .get(&saved.current)
                .map(|st| st.last)
                .filter(|l| *l > 0);
            n.conflicts.clone_from(&saved.conflicts);
        });
        let mut dir = notesync::Dir::new(notesync::notes_dir());
        // The desktop's notes as last synced: unchanged, there is nothing to do.
        let mut synced: Option<notesync::Manifest> = None;
        loop {
            let kicked = {
                let (lock, wake) = &self.kick;
                let flag = lock.lock().unwrap();
                let (mut flag, _) = wake
                    .wait_timeout_while(flag, Duration::from_secs(15), |k| !*k)
                    .unwrap();
                std::mem::take(&mut *flag)
            };
            if kicked {
                // Saves come in bursts while typing: one sync for them all.
                thread::sleep(Duration::from_millis(1500));
                *self.kick.0.lock().unwrap() = false;
            }
            let on = self.state.lock().unwrap().notes.on;
            if saved.off == on {
                saved.off = !on;
                save(&saved);
            }
            if !on || self.link.lock().unwrap().is_none() {
                synced = None;
                continue;
            }
            let dm = match dir.manifest() {
                Ok(m) => m,
                Err(e) => {
                    self.notes_state(|n| n.error = Some(format!("{e:#}")));
                    continue;
                }
            };
            if !kicked && synced.as_ref() == Some(&dm) {
                continue;
            }
            self.notes_state(|n| n.busy = true);
            let result = self.sync_notes_once(&mut dir, &dm, &mut saved);
            save(&saved);
            synced = dir.manifest().ok();
            self.notes_state(|n| {
                n.busy = false;
                n.error = result.err().map(|e| format!("{e:#}"));
                n.last = saved
                    .stores
                    .get(&saved.current)
                    .map(|st| st.last)
                    .filter(|l| *l > 0);
                n.conflicts.clone_from(&saved.conflicts);
            });
        }
    }
    fn sync_notes_once(
        &self,
        dir: &mut notesync::Dir,
        dm: &notesync::Manifest,
        saved: &mut NotesSaved,
    ) -> Result<()> {
        let listing = self.request("notes_list", json!({}))?;
        let id = listing["store"].as_str().unwrap_or_default();
        if id.is_empty()
            || id.len() > 64
            || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            bail!("The phone's notes have no valid id");
        }
        let files = listing["files"].as_array().context("No notes listing")?;
        if files.len() > notesync::MAX_FILES {
            bail!(
                "Over {} notes on the phone: too many to sync",
                notesync::MAX_FILES
            );
        }
        let mut pm = notesync::Manifest::new();
        for f in files {
            if let (Some(path), Some(hash)) = (f[0].as_str(), f[1].as_str())
                && hash.len() == 64
                && hash.bytes().all(|b| b.is_ascii_hexdigit())
            {
                pm.insert(path.to_owned(), hash.to_ascii_lowercase());
            }
        }
        let when = gtk::glib::DateTime::now_local()
            .and_then(|d| d.format("%Y-%m-%d %H.%M"))
            .map(|s| s.to_string())
            .unwrap_or_default();
        let stamp = gtk::glib::real_time().to_string();
        saved.current = id.to_owned();
        // The stores of phones gone for good: a reinstall makes a new one.
        while saved.stores.len() > 4 {
            let Some(old) = saved
                .stores
                .iter()
                .filter(|(k, _)| *k != id)
                .min_by_key(|(_, v)| v.last)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            saved.stores.remove(&old);
        }
        let store = saved.stores.entry(id.to_owned()).or_default();
        let out = notesync::sync(
            dir,
            &mut Remote(self),
            dm,
            pm,
            &mut store.base,
            &when,
            &stamp,
        );
        for c in out.conflicts {
            if !saved.conflicts.contains(&c) {
                saved.conflicts.push(c);
            }
        }
        saved.conflicts.retain(|c| dir.root.join(c).is_file());
        match out.errors.first() {
            None => {
                if let Some(store) = saved.stores.get_mut(id) {
                    store.last = now_ms();
                }
                Ok(())
            }
            Some(e) => bail!("{} changes wait for the next sync: {e}", out.errors.len()),
        }
    }

    /// Sends each text the desktop copies to the phone. wl-paste follows the
    /// clipboard without a window of ours; what password managers mark as
    /// secret stays here. Each copy comes out ended by a NUL.
    fn share_clipboard(&self) -> Result<()> {
        const KEEP_SECRETS: &str = "if wl-paste --list-types | grep -q x-kde-passwordManagerHint; \
            then cat >/dev/null; else cat; fi; printf '\\0'";
        let mut command = Command::new("wl-paste");
        command
            .args(["--type", "text", "--watch", "sh", "-c", KEEP_SECRETS])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        use std::os::unix::process::CommandExt;
        // It would outlive the daemon, watching for good. The signal comes when the
        // thread that spawned it ends, not the process: this one's loop never does.
        let parent = std::process::id() as libc::pid_t;
        // SAFETY: only async-signal-safe calls, in the child before exec.
        unsafe {
            command.pre_exec(move || {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                // The daemon died before that took effect.
                if libc::getppid() != parent {
                    libc::raise(libc::SIGTERM);
                }
                Ok(())
            });
        }
        let mut child = command.spawn().context("wl-clipboard is not installed")?;
        let mut out = BufReader::new(child.stdout.take().context("No wl-paste output")?);
        let mut copied = Vec::new();
        // A read error is the end of it too, wl-paste still running: stopped below.
        while out.read_until(0, &mut copied).unwrap_or(0) > 0 {
            if copied.last() == Some(&0) {
                copied.pop();
            }
            let text = String::from_utf8_lossy(&copied).into_owned();
            copied.clear();
            if text.trim().is_empty() || text.len() > 1 << 20 {
                continue;
            }
            let mut last = self.clip.lock().unwrap();
            if *last == text {
                continue;
            }
            last.clone_from(&text);
            drop(last);
            self.remember(&text, false);
            if let Some(link) = self.link.lock().unwrap().clone() {
                let _ = link.send(&json!({ "t": "clip", "text": text }));
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        bail!("wl-paste stopped")
    }
    /// Puts `text` first in the history; copied again, it keeps where it came from.
    fn remember(&self, text: &str, phone: bool) {
        let mut history = self.history.lock().unwrap();
        let phone = match history.iter().position(|c| c.text == text) {
            Some(i) => history.remove(i).phone || phone,
            None => phone,
        };
        history.insert(
            0,
            Clip {
                text: text.to_owned(),
                time: gtk::glib::real_time() / 1000,
                phone,
            },
        );
        history.truncate(HISTORY);
    }
    pub fn clipboard(&self) -> Vec<Clip> {
        self.history.lock().unwrap().clone()
    }
    /// What the phone copied, onto the desktop's clipboard.
    fn copy(&self, text: &str) {
        {
            let mut last = self.clip.lock().unwrap();
            if text.is_empty() || *last == text {
                return;
            }
            *last = text.to_owned();
        }
        self.remember(text, true);
        let Ok(mut child) = Command::new("wl-copy").stdin(Stdio::piped()).spawn() else {
            return;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        // wl-copy forks to serve the clipboard and its first process returns.
        let _ = child.wait();
    }

    fn bus(&self) -> Option<&zbus::blocking::Connection> {
        self.bus
            .get_or_init(|| zbus::blocking::Connection::session().ok())
            .as_ref()
    }
    fn post(
        &self,
        app: &str,
        icon: &str,
        summary: &str,
        body: &str,
        replaces: u32,
        phone: bool,
    ) -> Option<u32> {
        let mut hints: HashMap<&str, zbus::zvariant::Value> = HashMap::new();
        if phone {
            hints.insert("x-nexus-phone", true.into());
        }
        let reply = self
            .bus()?
            .call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "Notify",
                &(
                    app,
                    replaces,
                    icon,
                    summary,
                    body,
                    Vec::<&str>::new(),
                    hints,
                    -1i32,
                ),
            )
            .ok()?;
        reply.body().deserialize::<u32>().ok()
    }
    fn desktop_notice(&self, text: &str) {
        self.post("Phone", "phone-symbolic", text, "", 0, false);
    }
    fn notify(&self, msg: &Value) {
        let key = msg["key"].as_str().unwrap_or_default().to_owned();
        let time = msg["time"].as_i64().unwrap_or_default();
        let pkg: String = msg["pkg"]
            .as_str()
            .unwrap_or_default()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '_')
            .collect();
        let mut icon = String::from("phone-symbolic");
        if let (Some(b64), false) = (msg["icon"].as_str(), pkg.is_empty())
            && let Ok(dir) = crate::runtime().map(|d| d.join("phone-icons"))
        {
            let path = dir.join(format!("{pkg}.png"));
            if path.exists()
                || (fs::create_dir_all(&dir).is_ok()
                    && fs::write(&path, gtk::glib::base64_decode(b64)).is_ok())
            {
                icon = path.to_string_lossy().into_owned();
            }
        }
        let text = |k: &str| msg[k].as_str().unwrap_or_default();
        {
            let mut state = self.state.lock().unwrap();
            let list = &mut state.notifications;
            list.retain(|n| n.key != key);
            list.push(PhoneNotification {
                key: key.clone(),
                app: text("app").into(),
                title: text("title").into(),
                text: text("text").into(),
                time,
                icon: icon.clone(),
            });
            list.sort_by_key(|n| std::cmp::Reverse(n.time));
        }
        let replaces = match self.shown.lock().unwrap().get(&key) {
            Some((_, shown)) if *shown == time => return,
            Some((id, _)) => *id,
            None => 0,
        };
        let body = gtk::glib::markup_escape_text(text("text"));
        if let Some(id) = self.post(text("app"), &icon, text("title"), &body, replaces, true) {
            let mut shown = self.shown.lock().unwrap();
            shown.insert(key, (id, time));
            save_seen(&shown);
        }
    }
    fn close(&self, key: &str) {
        self.state
            .lock()
            .unwrap()
            .notifications
            .retain(|n| n.key != key);
        let Some((id, _)) = ({
            let mut shown = self.shown.lock().unwrap();
            let removed = shown.remove(key);
            save_seen(&shown);
            removed
        }) else {
            return;
        };
        if id == 0 {
            return;
        }
        if let Some(bus) = self.bus() {
            let _ = bus.call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "CloseNotification",
                &(id,),
            );
        }
    }
    /// A new notification server numbers its notifications afresh: the ids
    /// kept would name others, and closing those would dismiss phone ones.
    fn follow_restarts(&self) -> Result<()> {
        let bus = zbus::blocking::Connection::session()?;
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender("org.freedesktop.DBus")?
            .interface("org.freedesktop.DBus")?
            .member("NameOwnerChanged")?
            .add_arg("org.freedesktop.Notifications")?
            .build();
        for msg in zbus::blocking::MessageIterator::for_match_rule(rule, &bus, None)? {
            msg?;
            let mut shown = self.shown.lock().unwrap();
            // Still known as shown, so not posted again, but by no id.
            shown.values_mut().for_each(|v| v.0 = 0);
            save_seen(&shown);
        }
        Ok(())
    }
    /// A phone notification the user dismisses on the desktop goes on the phone too.
    fn follow_dismissals(self: &Arc<Self>) -> Result<()> {
        let bus = zbus::blocking::Connection::session()?;
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.freedesktop.Notifications")?
            .member("NotificationClosed")?
            .build();
        for msg in zbus::blocking::MessageIterator::for_match_rule(rule, &bus, None)? {
            let Ok((id, reason)) = msg?.body().deserialize::<(u32, u32)>() else {
                continue;
            };
            // 2: dismissed by the user.
            if reason != 2 {
                continue;
            }
            let key = {
                let mut shown = self.shown.lock().unwrap();
                let key = shown
                    .iter()
                    .find(|(_, v)| v.0 == id)
                    .map(|(k, _)| k.clone());
                if let Some(k) = &key {
                    shown.remove(k);
                    save_seen(&shown);
                }
                key
            };
            if let Some(key) = key {
                let p = self.clone();
                thread::spawn(move || {
                    let _ = p.request("dismiss", json!({ "key": key }));
                });
            }
        }
        Ok(())
    }
}

/// The phone's notes, through the link.
struct Remote<'a>(&'a Phone);
impl Side for Remote<'_> {
    fn read(&mut self, path: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
        let r = self.0.request(
            "notes_read",
            json!({ "path": path, "offset": offset, "len": len }),
        )?;
        let bytes = gtk::glib::base64_decode(r["b64"].as_str().unwrap_or_default());
        if bytes.len() > len {
            bail!("The phone sent more than asked");
        }
        Ok(bytes)
    }
    fn write(&mut self, path: &str, offset: u64, bytes: &[u8]) -> Result<()> {
        let b64 = gtk::glib::base64_encode(bytes);
        self.0.request(
            "notes_write",
            json!({ "path": path, "offset": offset, "b64": b64.as_str() }),
        )?;
        Ok(())
    }
    fn commit(&mut self, path: &str, hash: &str, expect: Option<&str>) -> Result<()> {
        self.0.request(
            "notes_commit",
            json!({ "path": path, "hash": hash, "expect": expect.unwrap_or_default() }),
        )?;
        Ok(())
    }
    fn trash(&mut self, path: &str, expect: &str, stamp: &str) -> Result<()> {
        self.0.request(
            "notes_trash",
            json!({ "path": path, "expect": expect, "stamp": stamp }),
        )?;
        Ok(())
    }
    fn rename(&mut self, from: &str, to: &str, expect: &str) -> Result<()> {
        self.0.request(
            "notes_move",
            json!({ "path": from, "to": to, "expect": expect }),
        )?;
        Ok(())
    }
}

/// MMS: attachments go to the phone in chunks, then out as one message; the
/// parts of received ones come back the same way, into the cache.
const MMS_FILES: usize = 10;
const MMS_FILE_MAX: u64 = 20 << 20;
const MMS_PART_MAX: u64 = 50 << 20;
const MMS_CHUNK: u64 = 512 << 10;
/// Letters, digits, `.`, `-` and `_` alone, so that a name is never a path.
fn safe_name(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => c,
            _ => '_',
        })
        .collect::<String>()
        .trim_start_matches('.')
        .chars()
        .take(80)
        .collect();
    if clean.is_empty() {
        "attachment".into()
    } else {
        clean
    }
}
// ponytail: never pruned; a size cap if received media ever adds up.
fn mms_cache() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
        })
        .join("nexus/messages")
}
impl Phone {
    /// Sends `body` and `files` to `to` as one MMS: the phone gets each file in
    /// chunks, then builds and sends the message.
    pub fn send_mms(&self, to: &str, body: &str, files: &[PathBuf]) -> Result<Value> {
        if files.is_empty() || files.len() > MMS_FILES {
            bail!("Attach 1 to {MMS_FILES} files");
        }
        let mut parts = Vec::new();
        for (i, path) in files.iter().enumerate() {
            let shown = path.file_name().unwrap_or_default().to_string_lossy();
            let mut f = fs::File::open(path).with_context(|| format!("Can't read {shown}"))?;
            if f.metadata()?.len() > MMS_FILE_MAX {
                bail!("{shown} is over {} MB", MMS_FILE_MAX >> 20);
            }
            let name = format!("{i}-{}", safe_name(&shown));
            let mut offset = 0u64;
            loop {
                let mut chunk = Vec::new();
                (&mut f).take(MMS_CHUNK).read_to_end(&mut chunk)?;
                let b64 = gtk::glib::base64_encode(&chunk);
                self.request(
                    "mms_put",
                    json!({ "name": name, "offset": offset, "b64": b64.as_str() }),
                )?;
                offset += chunk.len() as u64;
                if (chunk.len() as u64) < MMS_CHUNK {
                    break;
                }
            }
            let mime = gtk::gio::content_type_guess(Some(path), None).0;
            let mime = gtk::gio::content_type_get_mime_type(&mime)
                .map_or_else(|| "application/octet-stream".into(), |m| m.to_string());
            parts.push(json!({ "name": name, "mime": mime }));
        }
        #[cfg(feature = "lab")]
        if let Some(wait) = fake().and_then(|f| f["replies"]["mms_send"]["wait"].as_f64()) {
            thread::sleep(Duration::from_secs_f64(wait.min(30.)));
        }
        self.request(
            "mms_send",
            json!({ "to": to, "body": body, "parts": parts }),
        )
    }
    /// The received attachment `part`, copied from the phone once and kept.
    pub fn mms_part(&self, part: i64, name: &str) -> Result<PathBuf> {
        let dir = mms_cache().join(part.to_string());
        let path = dir.join(safe_name(name));
        if path.exists() {
            return Ok(path);
        }
        fs::create_dir_all(&dir)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".{id}.part"));
        let result = (|| -> Result<()> {
            #[cfg(feature = "lab")]
            if let Some(f) = fake() {
                let src = f["replies"]["mms_read"]["files"][part.to_string()].as_str();
                fs::copy(src.context("No such attachment")?, &tmp)?;
                return Ok(());
            }
            let mut f = fs::File::create(&tmp)?;
            let mut offset = 0u64;
            loop {
                let r = self.request(
                    "mms_read",
                    json!({ "part": part, "offset": offset, "len": CHUNK }),
                )?;
                let bytes = gtk::glib::base64_decode(r["b64"].as_str().unwrap_or_default());
                f.write_all(&bytes)?;
                offset += bytes.len() as u64;
                if offset > MMS_PART_MAX {
                    bail!("The attachment is over {} MB", MMS_PART_MAX >> 20);
                }
                if r["eof"] != false || bytes.is_empty() {
                    return Ok(());
                }
            }
        })();
        match result.and_then(|()| Ok(fs::rename(&tmp, &path)?)) {
            Ok(()) => Ok(path),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn mms_names_stay_in_their_folder() {
        use super::safe_name;
        assert_eq!(safe_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(safe_name("Photo 1.jpg"), "Photo_1.jpg");
        assert_eq!(safe_name(""), "attachment");
        assert_eq!(safe_name(".."), "attachment");
    }
    #[test]
    fn token_comparison() {
        assert!(super::same("abc", "abc"));
        assert!(!super::same("abc", "abd"));
        assert!(!super::same("abc", "abcd"));
    }
    #[test]
    fn hello_lines_are_bounded() {
        use super::{MAX_HELLO, read_line};
        let fits = format!("{}\n", "x".repeat(MAX_HELLO as usize - 1));
        let mut r = std::io::Cursor::new(fits.as_bytes());
        assert_eq!(read_line(&mut r, MAX_HELLO).unwrap().unwrap(), fits);
        let over = format!("{}\n", "x".repeat(MAX_HELLO as usize));
        assert!(read_line(&mut std::io::Cursor::new(over.as_bytes()), MAX_HELLO).is_err());
        assert!(
            read_line(&mut std::io::Cursor::new(b""), MAX_HELLO)
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn greeting_connections_are_capped() {
        use std::sync::atomic::Ordering;
        let phone = super::Phone::default();
        for _ in 0..super::MAX_GREETING {
            assert!(phone.admit());
        }
        assert!(!phone.admit());
        phone.greeting.fetch_sub(1, Ordering::Relaxed);
        assert!(phone.admit());
    }
    #[test]
    fn an_oversized_hello_is_dropped_and_frees_its_slot() {
        use std::{io::Write, sync::atomic::Ordering};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let mut client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let phone = std::sync::Arc::new(super::Phone::default());
        assert!(phone.admit());
        client
            .write_all(&[b'x'; super::MAX_HELLO as usize + 1])
            .unwrap();
        // Returns without waiting for the rest of the line, or for a timeout.
        phone.serve(stream);
        assert_eq!(phone.greeting.load(Ordering::Relaxed), 0);
    }
    #[test]
    fn pulled_files_never_replace_local_ones() {
        let dir = std::env::temp_dir().join(format!("nexus-pull-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "mine").unwrap();
        let claim = |name| {
            let path = super::claim_name(&dir, name).unwrap();
            path.file_name().unwrap().to_str().unwrap().to_owned()
        };
        assert_eq!(claim("a.txt"), "a (1).txt");
        assert_eq!(claim("a.txt"), "a (2).txt");
        assert_eq!(claim("b.txt"), "b.txt");
        assert_eq!(claim("noext"), "noext");
        assert_eq!(claim("noext"), "noext (1)");
        assert_eq!(claim(".hidden"), ".hidden");
        assert_eq!(claim(".hidden"), ".hidden (1)");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "mine");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn lockers() {
        use super::is_locker;
        assert!(is_locker(b"/home/u/.local/bin/nexus\0lock\0"));
        assert!(is_locker(b"hyprlock\0"));
        assert!(!is_locker(b"nexus\0lock\0--preview\0"));
        assert!(!is_locker(b"nexus\0shell\0"));
        assert!(!is_locker(b""));
    }
}
