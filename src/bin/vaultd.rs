//! `nexus-vaultd`: holds the vault key in memory only, for the Nexus binaries
//! allowed by [`peer_allowed`]. Started by its first client; exits once locked
//! and idle for a minute. Nothing it handles is ever logged.
use anyhow::{Context, Result, bail};
use nexus_control::vault::{self, Entry, VaultFile, VaultKey, WRONG_PASSWORD};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use zeroize::{Zeroize, Zeroizing};

const TICK: Duration = Duration::from_secs(2);
const EXIT_AFTER: Duration = Duration::from_secs(60);
const MIN_PASSWORD: usize = 8;
const MAX_LINE: usize = 16 << 20;

/// Lab only: `NEXUS_VAULT_LOCK_FILE` stands in for the lock screen, whose
/// detection otherwise sees every session on the machine.
fn screen_locked() -> bool {
    #[cfg(feature = "lab")]
    if let Some(path) = std::env::var_os("NEXUS_VAULT_LOCK_FILE") {
        return std::path::Path::new(&path).exists();
    }
    nexus_control::phone::screen_locked()
}
fn idle_limit() -> Duration {
    #[cfg(feature = "lab")]
    if let Some(secs) = std::env::var("NEXUS_VAULT_IDLE")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        return Duration::from_secs(secs);
    }
    Duration::from_secs(15 * 60)
}

struct State {
    key: Option<VaultKey>,
    last: Instant,
    /// Password attempts failed in a row, and when the next one is allowed.
    fails: u32,
    retry_at: Option<Instant>,
    /// A password is being checked: one at a time.
    checking: bool,
    /// Entries that would not decrypt, as of the last listing.
    damaged: usize,
}
impl State {
    fn lock_vault(&mut self) {
        self.key = None;
        self.damaged = 0;
    }
}
type Shared = Arc<Mutex<State>>;

fn peer_allowed(stream: &UnixStream, dir: &std::path::Path) -> bool {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are valid for the option being read.
    let got = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &raw mut len,
        )
    };
    // SAFETY: no preconditions.
    if got != 0 || cred.uid != unsafe { libc::getuid() } {
        return false;
    }
    let Ok(exe) = fs::read_link(format!("/proc/{}/exe", cred.pid)) else {
        return false;
    };
    ["nexus", "nexus-vault-host"]
        .iter()
        .any(|name| exe == dir.join(name))
}

/// Takes the next line out of `buffer`, reading more as needed. The caller's
/// `Zeroizing` buffers replace `BufReader`, whose buffer could not be wiped.
fn next_line(
    stream: &mut UnixStream,
    buffer: &mut Zeroizing<Vec<u8>>,
) -> Result<Option<Zeroizing<Vec<u8>>>> {
    loop {
        if let Some(i) = buffer.iter().position(|b| *b == b'\n') {
            let line = Zeroizing::new(buffer[..i].to_vec());
            buffer.drain(..=i);
            return Ok(Some(line));
        }
        if buffer.len() > MAX_LINE {
            bail!("Oversized request");
        }
        let mut chunk = Zeroizing::new([0u8; 8192]);
        let n = stream.read(&mut chunk[..])?;
        if n == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
}

fn delay(fails: u32) -> Duration {
    Duration::from_secs(1u64 << fails.saturating_sub(1).min(5)).min(Duration::from_secs(30))
}
/// One password attempt at a time, none during the delay after a failure.
fn begin_attempt(state: &Shared) -> Result<()> {
    let mut s = state.lock().unwrap();
    if s.checking {
        bail!("Still checking the last attempt");
    }
    if let Some(at) = s.retry_at
        && let Some(wait) = at.checked_duration_since(Instant::now())
    {
        bail!(
            "Too many attempts. Try again in {} s.",
            wait.as_secs_f32().ceil() as u32
        );
    }
    s.checking = true;
    Ok(())
}
fn end_attempt(state: &Shared, wrong: bool) {
    let mut s = state.lock().unwrap();
    s.checking = false;
    if wrong {
        s.fails += 1;
        s.retry_at = Some(Instant::now() + delay(s.fails));
    } else {
        s.fails = 0;
        s.retry_at = None;
    }
}
/// Runs a password check under the rate limit: only "Wrong password" counts
/// against the caller.
fn attempt<R>(state: &Shared, check: impl FnOnce() -> Result<R>) -> Result<R> {
    begin_attempt(state)?;
    let result = check();
    end_attempt(
        state,
        matches!(&result, Err(e) if e.to_string() == WRONG_PASSWORD),
    );
    result
}

fn text<'a>(req: &'a Value, field: &str) -> Result<&'a str> {
    req[field]
        .as_str()
        .with_context(|| format!("Missing {field}"))
}
fn device() -> String {
    nexus_control::phone::hostname()
}
fn summary(id: &str, updated: i64, entry: &Entry) -> Value {
    json!({
        "id": id,
        "title": entry.title,
        "urls": entry.urls,
        "username": entry.username,
        "updated": updated,
    })
}
fn check_new_password(password: &str) -> Result<()> {
    if password.chars().count() < MIN_PASSWORD {
        bail!("Use at least {MIN_PASSWORD} characters");
    }
    Ok(())
}
fn entry_from(req: &Value) -> Result<Entry> {
    let urls = req["urls"]
        .as_array()
        .map(|urls| {
            urls.iter()
                .filter_map(|u| u.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let mut entry = Entry::default();
    entry.title = text(req, "title")?.to_owned();
    entry.urls = urls;
    entry.username = req["username"].as_str().unwrap_or_default().to_owned();
    entry.password = req["password"].as_str().unwrap_or_default().to_owned();
    entry.notes = req["notes"].as_str().unwrap_or_default().to_owned();
    Ok(entry)
}

fn handle(state: &Shared, op: &str, req: &Value) -> Result<Value> {
    if op != "status" {
        state.lock().unwrap().last = Instant::now();
    }
    // What needs the key holds the state's lock for the time of it: none of
    // these derive a key from a password, so that is milliseconds.
    let with_key = |f: &mut dyn FnMut(&mut State) -> Result<Value>| -> Result<Value> {
        let mut s = state.lock().unwrap();
        s.key.as_ref().context("The vault is locked")?;
        f(&mut s)
    };
    match op {
        "status" => {
            let s = state.lock().unwrap();
            let file = vault::load().ok().flatten();
            Ok(json!({
                "exists": vault::path().exists(),
                "unlocked": s.key.is_some(),
                "count": file.as_ref().map_or(0, VaultFile::live),
                "damaged": s.damaged,
            }))
        }
        "create" => {
            let password = text(req, "password")?;
            check_new_password(password)?;
            let key = vault::update(|current| {
                if current.is_some() {
                    bail!("A vault already exists");
                }
                let (file, key) = VaultFile::create(password)?;
                Ok((Some(file), key))
            })?;
            key.pin();
            state.lock().unwrap().key = Some(key);
            Ok(json!({}))
        }
        "unlock" => {
            let password = text(req, "password")?;
            let key = attempt(state, || {
                vault::load()?
                    .context("No vault here yet")?
                    .unlock(password)
            })?;
            key.pin();
            state.lock().unwrap().key = Some(key);
            Ok(json!({}))
        }
        "lock" => {
            state.lock().unwrap().lock_vault();
            Ok(json!({}))
        }
        "change_password" => {
            let (old, new) = (text(req, "old")?, text(req, "new")?);
            check_new_password(new)?;
            let key = attempt(state, || {
                vault::update(|current| {
                    let mut file = current.context("No vault here yet")?;
                    let key = file.change_password(old, new)?;
                    Ok((Some(file), key))
                })
            })?;
            key.pin();
            state.lock().unwrap().key = Some(key);
            Ok(json!({}))
        }
        "list" | "match" => with_key(&mut |s| {
            let file = vault::load()?.context("No vault here yet")?;
            let (mut entries, damaged) = file.entries(s.key.as_ref().unwrap());
            s.damaged = damaged;
            if op == "match" {
                let page = text(req, "url")?;
                entries.retain(|(_, _, e)| vault::entry_matches(&e.urls, page));
            }
            entries.sort_by_cached_key(|(_, _, e)| e.title.to_lowercase());
            Ok(entries
                .iter()
                .map(|(id, up, e)| summary(id, *up, e))
                .collect())
        }),
        "get" => with_key(&mut |s| {
            let id = text(req, "id")?;
            let file = vault::load()?.context("No vault here yet")?;
            let entry = file
                .entry(s.key.as_ref().unwrap(), id)
                .context("That entry no longer exists")?;
            let mut value = serde_json::to_value(&entry)?;
            value["id"] = id.into();
            Ok(value)
        }),
        "fill" => with_key(&mut |s| {
            let (id, page) = (text(req, "id")?, text(req, "url")?);
            let file = vault::load()?.context("No vault here yet")?;
            let entry = file
                .entry(s.key.as_ref().unwrap(), id)
                .context("That entry no longer exists")?;
            if !vault::entry_matches(&entry.urls, page) {
                bail!("This entry is not for that site");
            }
            Ok(json!({ "username": entry.username, "password": entry.password }))
        }),
        "save" => with_key(&mut |s| {
            let id = req["id"].as_str();
            let entry = entry_from(req)?;
            let key = s.key.as_ref().unwrap();
            let id = vault::update(|current| {
                let mut file = current.context("No vault here yet")?;
                let id = file.save(key, id, entry, &device())?;
                Ok((Some(file), id))
            })?;
            Ok(json!({ "id": id }))
        }),
        "delete" => with_key(&mut |s| {
            let id = text(req, "id")?;
            let key = s.key.as_ref().unwrap();
            vault::update(|current| {
                let mut file = current.context("No vault here yet")?;
                file.delete(key, id, &device())?;
                Ok((Some(file), ()))
            })?;
            Ok(json!({}))
        }),
        "generate" => {
            let length = req["length"].as_u64().unwrap_or(20) as usize;
            let symbols = req["symbols"].as_bool().unwrap_or(true);
            Ok(json!({ "password": vault::generate(length, symbols) }))
        }
        "merge_other" => {
            // The other vault's password is checked with the key out of the
            // lock, so a slow KDF does not stop every other request.
            let key = state
                .lock()
                .unwrap()
                .key
                .clone()
                .context("The vault is locked")?;
            let password = text(req, "password")?;
            let other = VaultFile::parse(req["vault"].clone())?;
            // A wrong password burns no time on the file lock: it is checked first.
            attempt(state, || other.unlock(password).map(drop))?;
            let (imported, skipped) = vault::update(|current| {
                let mut file = current.context("No vault here yet")?;
                let counts = file.absorb(&key, &other, password)?;
                Ok((Some(file), counts))
            })?;
            Ok(json!({ "imported": imported, "skipped": skipped }))
        }
        _ => bail!("Unknown request"),
    }
}

fn serve(state: Shared, mut stream: UnixStream) {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let mut buffer = Zeroizing::new(Vec::with_capacity(16 * 1024));
    while let Ok(Some(line)) = next_line(&mut stream, &mut buffer) {
        let mut reply = match serde_json::from_slice::<Value>(&line) {
            Ok(mut req) => {
                let op = req["op"].as_str().unwrap_or_default().to_owned();
                let result = handle(&state, &op, &req);
                vault::scrub(&mut req);
                match result {
                    Ok(data) => json!({ "ok": true, "data": data }),
                    Err(e) => json!({ "ok": false, "error": e.to_string() }),
                }
            }
            Err(_) => json!({ "ok": false, "error": "Bad request" }),
        };
        let mut bytes = serde_json::to_vec(&reply).unwrap_or_default();
        bytes.push(b'\n');
        let written = stream.write_all(&bytes);
        bytes.zeroize();
        vault::scrub(&mut reply);
        if written.is_err() {
            return;
        }
    }
}

/// BOOTTIME counts time asleep and MONOTONIC does not, so their gap grows
/// across a suspend, even one logind's signal never told us of.
fn sleep_gap() -> i64 {
    let clock = |id| {
        let mut t = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `t` is valid for writing.
        unsafe { libc::clock_gettime(id, &raw mut t) };
        t.tv_sec
    };
    clock(libc::CLOCK_BOOTTIME) - clock(libc::CLOCK_MONOTONIC)
}
fn watch(state: Shared) {
    let idle = idle_limit();
    let mut gap = sleep_gap();
    loop {
        std::thread::sleep(TICK);
        let mut s = state.lock().unwrap();
        let now_gap = sleep_gap();
        let slept = now_gap > gap + 2;
        gap = now_gap;
        if s.key.is_some() {
            if slept || s.last.elapsed() >= idle || screen_locked() {
                s.lock_vault();
            }
        } else if s.last.elapsed() >= EXIT_AFTER
            && !s.checking
            && s.retry_at.is_none_or(|at| at <= Instant::now())
        {
            std::process::exit(0);
        }
    }
}
/// Locks before the machine sleeps, when logind can say so.
fn watch_sleep(state: Shared) {
    let _ = (|| -> zbus::Result<()> {
        let bus = zbus::blocking::Connection::system()?;
        let manager = zbus::blocking::Proxy::new(
            &bus,
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
        )?;
        for signal in manager.receive_signal("PrepareForSleep")? {
            if signal.body().deserialize::<bool>().unwrap_or(false) {
                state.lock().unwrap().lock_vault();
            }
        }
        Ok(())
    })();
}

fn serve_forever(listener: UnixListener, state: Shared, dir: PathBuf) -> ! {
    loop {
        let Ok((stream, _)) = listener.accept() else {
            continue;
        };
        if !peer_allowed(&stream, &dir) {
            continue;
        }
        let state = state.clone();
        std::thread::spawn(move || serve(state, stream));
    }
}

fn main() -> Result<()> {
    let dir = std::env::current_exe()?
        .canonicalize()?
        .parent()
        .context("No folder for this binary")?
        .to_path_buf();
    // SAFETY: plain prctl; no core dumps, no ptrace by other processes of this user.
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) };
    let (listener, _lock) = nexus_control::listener("vault")?;
    let state: Shared = Arc::new(Mutex::new(State {
        key: None,
        last: Instant::now(),
        fails: 0,
        retry_at: None,
        checking: false,
        damaged: 0,
    }));
    let s = state.clone();
    std::thread::spawn(move || watch(s));
    let s = state.clone();
    std::thread::spawn(move || watch_sleep(s));
    serve_forever(listener, state, dir)
}
