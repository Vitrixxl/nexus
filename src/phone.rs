//! The phone link. The Nexus Android app connects to the daemon over Tailscale
//! and keeps one TCP connection open: newline-delimited JSON both ways. The
//! phone pushes its notifications; the daemon sends requests (`ls`, `read`,
//! `write`, `sms_threads`, `sms_list`, `sms_send`, `dismiss`…) that the phone
//! answers by id. The same port serves the page the pairing QR code opens,
//! which hands the address and token to the app and offers the APK.
//!
//! The token in `$XDG_CONFIG_HOME/nexus/phone.json`, made on the first run and
//! never changed, is the only credential: the QR code stays the same, and any
//! phone that scanned it reconnects with a tap. Deleting the file revokes them
//! all. The listener binds the Tailscale address alone, never the LAN.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

pub const PORT: u16 = 47913;
const CHUNK: u64 = 1 << 20;
const MAX_LINE: u64 = 8 << 20;

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
    locked: std::sync::atomic::AtomicBool,
    pub state: Mutex<PhoneState>,
    /// Desktop notification id of each phone notification, and its post
    /// time, by its key: the phone sends them all again on reconnecting.
    /// Kept on disk, so a restarted daemon does not show them again either.
    shown: Mutex<HashMap<String, (u32, i64)>>,
    bus: OnceLock<Option<zbus::blocking::Connection>>,
}

fn config() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
        .join("nexus/phone.json")
}
fn seen_path() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
        })
        .join("nexus/phone-notifications.json")
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
fn tailscale_ip() -> Result<String> {
    let out = Command::new("tailscale")
        .args(["ip", "-4"])
        .output()
        .context("Tailscale is not installed")?;
    let ip = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    if !out.status.success() || ip.is_empty() {
        bail!("Tailscale is not connected. Run `tailscale up`.");
    }
    Ok(ip)
}
/// Whether the screen is locked: `nexus lock` or hyprlock runs. Read from
/// /proc rather than by taking the lock's file lock, which would make a
/// `nexus lock` started at that instant think the screen already locked.
fn screen_locked() -> bool {
    let Ok(procs) = fs::read_dir("/proc") else {
        return false;
    };
    procs
        .flatten()
        .any(|p| fs::read(p.path().join("cmdline")).is_ok_and(|c| is_locker(&c)))
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
fn hostname() -> String {
    fs::read_to_string("/etc/hostname")
        .map(|h| h.trim().to_owned())
        .unwrap_or_else(|_| "Nexus".into())
}
pub fn apk() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join(".local/share/nexus/nexus-phone.apk")
}
fn read_line(r: &mut impl BufRead) -> Result<Option<String>> {
    let mut line = String::new();
    if r.take(MAX_LINE).read_line(&mut line)? == 0 {
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
        let p = phone.clone();
        thread::spawn(move || {
            // ponytail: binds the address Tailscale had at startup; a new one
            // takes a daemon restart.
            loop {
                let bound =
                    tailscale_ip().and_then(|ip| Ok((TcpListener::bind((ip.as_str(), PORT))?, ip)));
                match bound {
                    Ok((listener, ip)) => {
                        p.set_url(&ip);
                        for stream in listener.incoming().flatten() {
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
                let locked = screen_locked();
                p.locked.store(locked, Ordering::Relaxed);
                if locked && let Some(link) = p.link.lock().unwrap().take() {
                    Self::release(&link, "locked");
                }
                thread::sleep(Duration::from_secs(2));
            }
        });
        let p = phone.clone();
        thread::spawn(move || {
            if let Err(e) = p.follow_dismissals() {
                eprintln!("Phone notifications: {e}");
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
        }
    }

    fn serve(self: &Arc<Self>, stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(90)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(20)));
        let Ok(clone) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(clone);
        let Ok(Some(first)) = read_line(&mut reader) else {
            return;
        };
        if first.starts_with("GET ") {
            let _ = self.http(&first, stream);
            return;
        }
        let hello: Value = serde_json::from_str(&first).unwrap_or_default();
        let token = hello["token"].as_str().unwrap_or_default();
        if hello["t"] != "hello" || !same(token, &self.token) {
            return;
        }
        let link = Arc::new(Link {
            out: Mutex::new(stream),
            pending: Mutex::default(),
        });
        if self.locked.load(Ordering::Relaxed) {
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
        self.state.lock().unwrap().name =
            Some(hello["name"].as_str().unwrap_or("Phone").to_owned());
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
        while let Ok(Some(line)) = read_line(&mut reader) {
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match msg["t"].as_str() {
                Some("notif") => self.notify(&msg),
                Some("notif_gone") => self.close(msg["key"].as_str().unwrap_or_default()),
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
                let page = format!(
                    r#"<!doctype html><meta name=viewport content="width=device-width,initial-scale=1"><title>Nexus</title>
<style>body{{font:18px system-ui;background:#0d1014;color:#e6e8eb;display:grid;gap:16px;padding:32px;max-width:420px;margin:auto}}a{{display:block;padding:16px;border-radius:14px;text-align:center;text-decoration:none;font-weight:600}}.p{{background:#7aa2f7;color:#0d1014}}.s{{background:#1c2128;color:#e6e8eb}}</style>
<h2>Nexus</h2><a class=p href="{intent}">Pair this phone</a><a class=s href="/nexus.apk?t={token}">Install the app first</a>
<p style="opacity:.6;font-size:14px">Installed? Tap Pair, then grant the permissions the app lists.</p>"#
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
            _ => reply(&mut stream, "404 Not Found", "text/plain", b"")?,
        }
        Ok(())
    }

    /// Sends `op` to the phone and waits for its answer.
    pub fn request(&self, op: &str, args: Value) -> Result<Value> {
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
            let dest = dir.join(&name);
            let result = (|| -> Result<()> {
                let part = dir.join(format!(".{name}.part"));
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
                fs::rename(part, &dest)?;
                Ok(())
            })();
            p.desktop_notice(&match result {
                Ok(()) => format!("Saved {}", dest.display()),
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
        let replaces = match self.shown.lock().unwrap().get(&key) {
            Some((_, shown)) if *shown == time => return,
            Some((id, _)) => *id,
            None => 0,
        };
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
        let body = gtk::glib::markup_escape_text(text("text"));
        if let Some(id) = self.post(text("app"), &icon, text("title"), &body, replaces, true) {
            let mut shown = self.shown.lock().unwrap();
            shown.insert(key, (id, time));
            save_seen(&shown);
        }
    }
    fn close(&self, key: &str) {
        let Some((id, _)) = ({
            let mut shown = self.shown.lock().unwrap();
            let removed = shown.remove(key);
            save_seen(&shown);
            removed
        }) else {
            return;
        };
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

#[cfg(test)]
mod tests {
    #[test]
    fn token_comparison() {
        assert!(super::same("abc", "abc"));
        assert!(!super::same("abc", "abd"));
        assert!(!super::same("abc", "abcd"));
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
