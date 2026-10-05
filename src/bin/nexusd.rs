use nexus_control::{backend::Backend, *};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
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
    for subsystem in 0..3 {
        let b = backend.clone();
        let state = state.clone();
        thread::spawn(move || {
            // NetworkManager forgets its agents when it restarts.
            let mut reachable = true;
            loop {
                match subsystem {
                    0 => {
                        let result = b.wifi();
                        if result.is_ok() && !reachable {
                            b.register_agents();
                        }
                        reachable = result.is_ok();
                        let mut s = state.lock().unwrap();
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
                    }
                    1 => {
                        let result = b.bluetooth();
                        let mut s = state.lock().unwrap();
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
                    _ => poll_audio(&state),
                }
                thread::sleep(Duration::from_secs(2));
            }
        });
    }
    // Discovery is independent of cached status and user mutations. Each scan
    // rechecks the radio state, and a slow service cannot delay the other radio.
    for op in ["wifi-scan", "bluetooth-scan"] {
        let backend = backend.clone();
        thread::spawn(move || {
            loop {
                let started = Instant::now();
                if let Err(e) = backend.action(op, "", "") {
                    eprintln!("Automatic {op}: {e}");
                }
                thread::sleep(Duration::from_secs(15).saturating_sub(started.elapsed()));
            }
        });
    }
    // Application streams come and go with playback; follow them as they change.
    {
        let state = state.clone();
        let (changed, pending) = std::sync::mpsc::channel::<()>();
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
                            if line.contains("sink-input") && changed.send(()).is_err() {
                                return;
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
        thread::spawn(move || {
            let reply = (|| -> anyhow::Result<Reply> {
                let req: Request = serde_json::from_str(&read_line(&stream)?)?;
                Ok(match req {
                    Request::Status => {
                        let mut snapshot = state.lock().unwrap().clone();
                        snapshot.prompt = b.prompts.current();
                        snapshot.applications_revision = catalogue.lock().unwrap().revision;
                        Reply {
                            ok: true,
                            error: None,
                            state: Some(snapshot),
                            catalogue: None,
                        }
                    }
                    Request::Applications => Reply {
                        ok: true,
                        error: None,
                        state: None,
                        catalogue: Some(catalogue.lock().unwrap().clone()),
                    },
                    Request::Launch { id } => Reply::result(catalogue.lock().unwrap().launch(&id)),
                    Request::Answer {
                        id,
                        values,
                        accepted,
                    } => Reply::result(b.prompts.answer(id, values, accepted)),
                    Request::Action { op, target, value } => match mutations.try_lock() {
                        Ok(_guard) => {
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
                            Reply::result(result)
                        }
                        Err(_) => Reply::result(Err(anyhow::anyhow!(
                            "Another operation is in progress. Complete it or cancel its prompt first."
                        ))),
                    },
                    Request::Theme { settings } => match mutations.try_lock() {
                        Ok(_guard) => Reply::result(
                            theme::save(settings)
                                .map(|settings| state.lock().unwrap().settings = settings),
                        ),
                        Err(_) => {
                            Reply::result(Err(anyhow::anyhow!("Another operation is in progress")))
                        }
                    },
                })
            })();
            let reply = reply.unwrap_or_else(|e| Reply::result(Err(e)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
            let _ = serde_json::to_writer(&mut stream, &reply);
            let _ = stream.write_all(b"\n");
            clients.fetch_sub(1, Ordering::Relaxed);
        });
    }
    Ok(())
}

fn poll_audio(state: &Mutex<Snapshot>) {
    let volume = backend::volume(false);
    let mic = backend::volume(true);
    let light = backend::brightness();
    let night_light = backend::night_light();
    let outputs = backend::audio_devices("sinks").unwrap_or_default();
    let inputs = backend::audio_devices("sources").unwrap_or_default();
    let streams = backend::app_streams();
    let default_output = backend::run("pactl", &["get-default-sink"]).unwrap_or_default();
    let default_input = backend::run("pactl", &["get-default-source"]).unwrap_or_default();
    let mut s = state.lock().unwrap();
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
