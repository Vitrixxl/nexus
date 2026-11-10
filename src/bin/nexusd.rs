use nexus_control::{backend::Backend, *};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, MutexGuard, TryLockError,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use zbus::zvariant::Value;
/// Changes made through the daemon. A poll that read the system before one
/// must not store what it read over the change's own read-back.
static CHANGES: AtomicU64 = AtomicU64::new(0);
/// When a client last worked with the radios, in seconds since the daemon started.
static RADIOS_WATCHED: AtomicU64 = AtomicU64::new(0);
/// Every network's properties are D-Bus calls: read them often only while someone
/// is looking, which Control Center's pages say by asking for scans. Otherwise
/// [`watch_bus`] wakes the poll when something happens; the slow read is for what
/// says nothing, like signal strength.
fn radio_pause(started: Instant) -> Duration {
    let since = started
        .elapsed()
        .as_secs()
        .saturating_sub(RADIOS_WATCHED.load(Ordering::Relaxed));
    Duration::from_secs(if since < 30 { 2 } else { 20 })
}
/// Wakes a radio's poll when `rule` matches on the system bus.
fn watch_bus(rule: zbus::MatchRule<'static>, wake: mpsc::Sender<()>) {
    thread::spawn(move || {
        loop {
            let watch = || -> zbus::Result<()> {
                let bus = zbus::blocking::Connection::system()?;
                for message in
                    zbus::blocking::MessageIterator::for_match_rule(rule.clone(), &bus, Some(8))?
                {
                    // Signal strength comes constantly during a scan, and is no event.
                    let body = message?.body();
                    let (_, changed, _): (String, HashMap<String, Value>, Vec<String>) =
                        body.deserialize()?;
                    if !changed.contains_key("RSSI") && wake.send(()).is_err() {
                        break;
                    }
                }
                Ok(())
            };
            if let Err(e) = watch() {
                eprintln!("Could not follow the radios: {e}");
            }
            thread::sleep(Duration::from_secs(30));
        }
    });
}
/// A change on NetworkManager itself, not its access points whose signal changes
/// all the time; or on a Bluetooth device.
fn radio_changes(subsystem: u8) -> zbus::Result<zbus::MatchRule<'static>> {
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.DBus.Properties")?
        .member("PropertiesChanged")?;
    Ok(if subsystem == 0 {
        rule.sender("org.freedesktop.NetworkManager")?
            .path("/org/freedesktop/NetworkManager")?
            .build()
    } else {
        rule.sender("org.bluez")?
            .arg(0, "org.bluez.Device1")?
            .build()
    })
}
fn main() -> anyhow::Result<()> {
    let (listener, _lock) = match listener("daemon") {
        Ok(v) => v,
        Err(e) => {
            if request(&Request::Status).is_ok() {
                return Ok(());
            }
            return Err(e);
        }
    };
    let mut signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
    ])?;
    thread::spawn(move || {
        if signals.forever().next().is_some() {
            theme::stop_wallpaper();
            std::process::exit(0);
        }
    });
    let backend = Arc::new(Backend::new()?);
    // NetworkManager/BlueZ may still be starting with the desktop session. Their D-Bus
    // registration must not delay serving the catalogue and cached state.
    {
        let backend = backend.clone();
        thread::spawn(move || backend.register_agents());
    }
    let phone = phone::Phone::start()?;
    let catalogue = Arc::new(Mutex::new(launcher::Catalogue::load()));
    launcher::watch(catalogue.clone());
    let settings = theme::load();
    theme::write_theme(&settings)?;
    {
        let settings = settings.clone();
        thread::spawn(move || theme::sync_desktop(&settings));
    }
    let state = Arc::new(Mutex::new(Snapshot {
        settings: settings.clone(),
        ..Snapshot::default()
    }));
    if let Some(path) = settings.wallpaper {
        thread::spawn(move || {
            if let Err(e) = theme::apply_wallpaper(&path) {
                eprintln!("{e}");
            }
        });
    }
    // Each subsystem polls independently: a radio timeout never freezes sound or IPC.
    let started = Instant::now();
    for subsystem in 0..3u8 {
        let b = backend.clone();
        let state = state.clone();
        let (wake, woken) = mpsc::channel();
        if subsystem < 2
            && let Ok(rule) = radio_changes(subsystem)
        {
            watch_bus(rule, wake.clone());
        }
        thread::spawn(move || {
            let _awake = wake;
            // NetworkManager forgets its agents when it restarts.
            let mut reachable = true;
            loop {
                match subsystem {
                    0 => {
                        let ok = poll_wifi(&b, &state);
                        if ok && !reachable {
                            b.register_agents();
                        }
                        reachable = ok;
                    }
                    1 => poll_bluetooth(&b, &state),
                    _ => poll_light(&state),
                }
                let _ = woken.recv_timeout(if subsystem == 2 {
                    Duration::from_secs(2)
                } else {
                    radio_pause(started)
                });
                while woken.try_recv().is_ok() {}
            }
        });
    }
    // Application streams come and go with playback; follow them as they change.
    {
        let state = state.clone();
        let (changed, pending) = std::sync::mpsc::channel::<()>();
        // Devices, defaults and volumes are read again when PipeWire reports a
        // change, rather than by starting a dozen helpers every 2 seconds.
        let (audio_changed, audio_pending) = std::sync::mpsc::channel::<()>();
        {
            let state = state.clone();
            thread::spawn(move || {
                loop {
                    let _ = audio_pending.recv_timeout(Duration::from_secs(30));
                    // Coalesce event bursts, such as a volume drag.
                    thread::sleep(Duration::from_millis(50));
                    while audio_pending.try_recv().is_ok() {}
                    poll_audio(&state);
                }
            });
        }
        thread::spawn(move || {
            while pending.recv().is_ok() {
                while pending.try_recv().is_ok() {}
                if let Ok(streams) = backend::app_streams() {
                    state.lock().unwrap().streams = streams;
                }
                // Coalesce event bursts, such as a volume drag.
                thread::sleep(Duration::from_millis(50));
            }
        });
        thread::spawn(move || {
            loop {
                let _ = changed.send(());
                let _ = audio_changed.send(());
                if let Ok(mut child) = Command::new("pactl")
                    .arg("subscribe")
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    if let Some(out) = child.stdout.take() {
                        for line in BufReader::new(out).lines() {
                            let Ok(line) = line else { break };
                            if line.contains("sink-input") {
                                if changed.send(()).is_err() {
                                    return;
                                }
                            } else if ["on sink #", "on source #", "on server", "on card #"]
                                .iter()
                                .any(|e| line.contains(e))
                            {
                                let _ = audio_changed.send(());
                            }
                        }
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                }
                thread::sleep(Duration::from_secs(1));
            }
        });
    }
    let mutations = Arc::new(Mutex::new(()));
    let clients = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        if clients.fetch_add(1, Ordering::Relaxed) >= 32 {
            clients.fetch_sub(1, Ordering::Relaxed);
            continue;
        }
        let catalogue = catalogue.clone();
        let b = backend.clone();
        let state = state.clone();
        let mutations = mutations.clone();
        let clients = clients.clone();
        let phone = phone.clone();
        thread::spawn(move || {
            let reply = (|| -> anyhow::Result<Reply> {
                let req: Request = serde_json::from_str(&read_line(&stream)?)?;
                Ok(match req {
                    Request::Status => {
                        let mut snapshot = state.lock().unwrap().clone();
                        snapshot.prompt = b.prompts.current();
                        snapshot.applications_revision = catalogue.lock().unwrap().revision;
                        snapshot.phone = phone.state.lock().unwrap().clone();
                        Reply {
                            ok: true,
                            error: None,
                            state: Some(snapshot),
                            catalogue: None,
                            data: None,
                        }
                    }
                    Request::Applications => Reply {
                        ok: true,
                        error: None,
                        state: None,
                        catalogue: Some(catalogue.lock().unwrap().clone()),
                        data: None,
                    },
                    Request::Phone { op, args } => {
                        let arg = |k: &str| args[k].as_str().unwrap_or_default().to_owned();
                        let result = match op.as_str() {
                            "disconnect" => {
                                phone.disconnect();
                                Ok(serde_json::Value::Null)
                            }
                            "clipboard" => Ok(serde_json::to_value(phone.clipboard())?),
                            "notes_sync" => {
                                phone.set_notes_sync(args["on"].as_bool().unwrap_or(true));
                                Ok(serde_json::Value::Null)
                            }
                            "calendar" => Ok(phone.calendar_events(
                                args["from"].as_i64().unwrap_or_default(),
                                args["to"].as_i64().unwrap_or_default(),
                            )),
                            "calendar_save" | "calendar_delete" => (|| {
                                let event = serde_json::from_value(args["event"].clone())?;
                                phone.calendar_change(if op == "calendar_save" {
                                    calendar::Change::Save(event)
                                } else {
                                    calendar::Change::Delete(event)
                                })?;
                                Ok(serde_json::Value::Null)
                            })(),
                            "pull" => {
                                let dir = match arg("dest") {
                                    d if !d.is_empty() => d.into(),
                                    _ => gtk::glib::user_special_dir(
                                        gtk::glib::UserDirectory::Downloads,
                                    )
                                    .unwrap_or_else(gtk::glib::home_dir),
                                };
                                phone.pull(arg("path"), dir);
                                Ok(serde_json::Value::Null)
                            }
                            "push" => {
                                phone.push(arg("src").into(), arg("dir"));
                                Ok(serde_json::Value::Null)
                            }
                            "mms_send" => {
                                let files: Vec<std::path::PathBuf> = args["files"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .filter_map(|f| f.as_str().map(Into::into))
                                    .collect();
                                phone.send_mms(&arg("to"), &arg("body"), &files)
                            }
                            "mms_part" => phone
                                .mms_part(args["part"].as_i64().unwrap_or_default(), &arg("name"))
                                .map(|p| serde_json::json!({ "path": p })),
                            "vault_answer" => phone
                                .set_vault_answer(&arg("answer"))
                                .map(|()| serde_json::Value::Null),
                            "vault_sync" => {
                                phone.kick_vault();
                                Ok(serde_json::Value::Null)
                            }
                            "vault_fetch_other" => phone.vault_fetch_other(),
                            "vault_adopt" => phone.vault_adopt(),
                            // The file, ciphertext or not, stays off this socket.
                            op if op.starts_with("vault_") => {
                                Err(anyhow::anyhow!("Not available here"))
                            }
                            _ => phone.request(&op, args),
                        };
                        match result {
                            Ok(data) => Reply {
                                data: Some(data),
                                ..Reply::result(Ok(()))
                            },
                            Err(e) => Reply::result(Err(e)),
                        }
                    }
                    Request::Launch { id } => Reply::result(catalogue.lock().unwrap().launch(&id)),
                    Request::Answer {
                        id,
                        values,
                        accepted,
                    } => Reply::result(b.prompts.answer(id, values, accepted)),
                    Request::Action { op, target, .. } if op == "wifi-password" => {
                        match b.wifi_password(&target) {
                            Ok(password) => Reply {
                                data: Some(password.into()),
                                ..Reply::result(Ok(()))
                            },
                            Err(e) => Reply::result(Err(e)),
                        }
                    }
                    // A scan changes nothing and must not wait for, or block, a change.
                    Request::Action { op, target, value } if op.ends_with("-scan") => {
                        RADIOS_WATCHED.store(started.elapsed().as_secs(), Ordering::Relaxed);
                        Reply::result(b.action(&op, &target, &value))
                    }
                    // The way out of a lock screen: never behind another operation.
                    Request::Action { op, target, value } if is_power(&op) => {
                        Reply::result(b.action(&op, &target, &value))
                    }
                    Request::Action { op, target, value } => match try_mutate(&mutations) {
                        Some(_guard) => {
                            if op.starts_with("wifi-") || op.starts_with("bluetooth-") {
                                RADIOS_WATCHED
                                    .store(started.elapsed().as_secs(), Ordering::Relaxed);
                            }
                            CHANGES.fetch_add(1, Ordering::SeqCst);
                            let result = b.action(&op, &target, &value);
                            // Toggles are read back at once; sliders stream too fast for that.
                            if result.is_ok()
                                && matches!(
                                    op.as_str(),
                                    "mute"
                                        | "mic-mute"
                                        | "audio-output"
                                        | "audio-input"
                                        | "stream-mute"
                                )
                            {
                                poll_audio(&state);
                            }
                            // So that a connection in progress shows at once,
                            // rather than the button flipping back meanwhile.
                            if result.is_ok() && (op.starts_with("wifi-") || op == "network-use") {
                                poll_wifi(&b, &state);
                            }
                            if result.is_ok() && op.starts_with("bluetooth-") {
                                poll_bluetooth(&b, &state);
                            }
                            if result.is_ok() && op == "night-light" {
                                poll_light(&state);
                            }
                            Reply::result(result)
                        }
                        None => Reply::result(Err(anyhow::anyhow!(
                            "Another operation is in progress. Complete it or cancel its prompt first."
                        ))),
                    },
                    Request::Theme { settings } => match try_mutate(&mutations) {
                        Some(_guard) => Reply::result(
                            theme::save(settings)
                                .map(|settings| state.lock().unwrap().settings = settings),
                        ),
                        None => {
                            Reply::result(Err(anyhow::anyhow!("Another operation is in progress")))
                        }
                    },
                })
            })();
            let reply = reply.unwrap_or_else(|e| Reply::result(Err(e)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
            let _ = send_line(&mut stream, &reply);
            clients.fetch_sub(1, Ordering::Relaxed);
        });
    }
    Ok(())
}

fn is_power(op: &str) -> bool {
    matches!(op, "sleep" | "restart" | "shutdown")
}
/// None while another operation runs. One that panicked guards nothing any more,
/// and must not refuse every action until the daemon restarts.
fn try_mutate(mutations: &Mutex<()>) -> Option<MutexGuard<'_, ()>> {
    match mutations.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}
/// Reads the Wi-Fi state into the snapshot; false when NetworkManager is unreachable.
fn poll_wifi(b: &Backend, state: &Mutex<Snapshot>) -> bool {
    let seen = CHANGES.load(Ordering::SeqCst);
    let result = b.wifi();
    let wired = b.wired().ok().flatten();
    let ok = result.is_ok();
    let mut s = state.lock().unwrap();
    if CHANGES.load(Ordering::SeqCst) != seen {
        return ok;
    }
    s.wired = wired;
    match result {
        Ok((on, nets, scanning)) => {
            s.wifi = on;
            s.networks = nets;
            s.wifi_scanning = scanning;
            s.wifi_error = None;
        }
        Err(e) => {
            s.wifi_error = Some(e.to_string());
            s.networks.clear();
            s.wifi_scanning = false;
        }
    }
    ok
}
fn poll_bluetooth(b: &Backend, state: &Mutex<Snapshot>) {
    let seen = CHANGES.load(Ordering::SeqCst);
    let result = b.bluetooth();
    let mut s = state.lock().unwrap();
    if CHANGES.load(Ordering::SeqCst) != seen {
        return;
    }
    match result {
        Ok((on, devices, scanning)) => {
            s.bluetooth = on;
            s.devices = devices;
            s.bluetooth_scanning = scanning;
            s.bluetooth_error = None;
        }
        Err(e) => {
            s.bluetooth_error = Some(e.to_string());
            s.devices.clear();
            s.bluetooth_scanning = false;
        }
    }
}
fn poll_audio(state: &Mutex<Snapshot>) {
    let seen = CHANGES.load(Ordering::SeqCst);
    let volume = backend::volume(false);
    let mic = backend::volume(true);
    let outputs = backend::audio_devices("sinks").unwrap_or_default();
    let inputs = backend::audio_devices("sources").unwrap_or_default();
    let streams = backend::app_streams();
    let default_output = backend::run("pactl", &["get-default-sink"]).unwrap_or_default();
    let default_input = backend::run("pactl", &["get-default-source"]).unwrap_or_default();
    let mut s = state.lock().unwrap();
    if CHANGES.load(Ordering::SeqCst) != seen {
        return;
    }
    s.outputs = outputs;
    s.inputs = inputs;
    s.default_output = default_output;
    s.default_input = default_input;
    if let Ok(streams) = streams {
        s.streams = streams;
    }
    match volume {
        Ok((v, m)) => {
            s.volume = Some(v);
            s.muted = m;
            s.audio_error = None;
        }
        Err(e) => {
            s.volume = None;
            s.audio_error = Some(e.to_string());
        }
    }
    if let Ok((v, m)) = mic {
        s.microphone = Some(v);
        s.mic_muted = m;
    }
}
/// Brightness and night light: a sysfs read, and the night light schedule.
fn poll_light(state: &Mutex<Snapshot>) {
    let seen = CHANGES.load(Ordering::SeqCst);
    let light = backend::brightness();
    let night_light = night::get();
    let mut s = state.lock().unwrap();
    if CHANGES.load(Ordering::SeqCst) != seen {
        return;
    }
    match light {
        Ok(v) => {
            s.brightness = Some(v);
            s.brightness_error = None;
        }
        Err(e) => {
            s.brightness = None;
            s.brightness_error = Some(e.to_string());
        }
    }
    match night_light {
        Ok(v) => {
            s.night_light = Some(v);
            s.night_light_error = None;
        }
        Err(e) => {
            s.night_light = None;
            s.night_light_error = Some(e.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn power_actions_are_told_apart() {
        for op in ["sleep", "restart", "shutdown"] {
            assert!(is_power(op));
        }
        assert!(!is_power("wifi"));
    }
    #[test]
    fn mutations_survive_a_panic_but_not_each_other() {
        let mutations = Arc::new(Mutex::new(()));
        let guard = try_mutate(&mutations).unwrap();
        assert!(try_mutate(&mutations).is_none());
        drop(guard);
        let poison = mutations.clone();
        let _ = thread::spawn(move || {
            let _guard = poison.lock().unwrap();
            panic!("poisons the mutex");
        })
        .join();
        assert!(mutations.is_poisoned());
        assert!(try_mutate(&mutations).is_some());
    }
}
