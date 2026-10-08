//! Night light: warms every output through wlr-gamma-control, from the daemon,
//! in place of hyprsunset. It follows a fixed schedule; a manual change holds
//! until the schedule next turns, or until it is asked to follow it again.
use crate::NightLight;
use anyhow::{Context, Result, bail};
use std::{
    io::{Seek, Write},
    os::fd::AsFd,
    sync::{Arc, Mutex, OnceLock},
};
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    protocol::{wl_output::WlOutput, wl_registry},
};
use wayland_protocols_wlr::gamma_control::v1::client::{
    zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1 as Manager,
    zwlr_gamma_control_v1::{self, ZwlrGammaControlV1 as Control},
};

// ponytail: fixed schedule, move it to the settings if it ever needs changing.
/// Minutes after midnight the night light turns on, then off.
const EVENING: u32 = 21 * 60;
const MORNING: u32 = 7 * 60 + 30;
const WARM: u16 = 4000;

fn is_night(minutes: u32) -> bool {
    !(MORNING..EVENING).contains(&minutes)
}
fn night_now() -> bool {
    gtk::glib::DateTime::now_local()
        .map(|t| is_night((t.hour() * 60 + t.minute()) as u32))
        .unwrap_or(false)
}
/// Red, green and blue gains of a black body at `kelvin` (Tanner Helland's fit).
fn gains(kelvin: u16) -> [f64; 3] {
    let t = f64::from(kelvin) / 100.;
    let red = if t <= 66. {
        255.
    } else {
        329.698_727_446 * (t - 60.).powf(-0.133_204_759_2)
    };
    let green = if t <= 66. {
        99.470_802_586_1 * t.ln() - 161.119_568_166_1
    } else {
        288.122_169_528_3 * (t - 60.).powf(-0.075_514_849_2)
    };
    let blue = if t >= 66. {
        255.
    } else if t <= 19. {
        0.
    } else {
        138.517_731_223_1 * (t - 10.).ln() - 305.044_792_730_7
    };
    [red, green, blue].map(|c| (c / 255.).clamp(0., 1.))
}
/// Red, then green, then blue ramps of `size` native-endian u16 entries.
fn ramps(size: u32, light: &NightLight) -> Vec<u8> {
    let gains = if light.enabled {
        gains(light.temperature)
    } else {
        [1.; 3]
    };
    let last = f64::from(size.max(2) - 1);
    gains
        .iter()
        .flat_map(|gain| {
            (0..size).flat_map(move |i| {
                ((f64::from(i) / last * gain * 65535.).round() as u16).to_ne_bytes()
            })
        })
        .collect()
}

struct Output {
    name: u32,
    control: Control,
    /// Entries per ramp; 0 until the compositor tells.
    size: u32,
}
struct Shared {
    light: NightLight,
    /// Whether the schedule wanted the night light when it was last changed by
    /// hand; `None` while it follows the schedule.
    manual: Option<bool>,
    outputs: Vec<Output>,
    error: Option<String>,
}
impl Shared {
    fn apply(&mut self, name: Option<u32>) {
        for output in &self.outputs {
            if output.size == 0 || name.is_some_and(|n| n != output.name) {
                continue;
            }
            if let Err(e) = send(&output.control, &ramps(output.size, &self.light)) {
                self.error = Some(format!("Could not set the screen colours: {e}"));
            }
        }
    }
}
/// The compositor reads the ramps from a file descriptor.
fn send(control: &Control, ramps: &[u8]) -> Result<()> {
    let path = crate::runtime()?.join("gamma");
    let mut file = std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)?;
    let _ = std::fs::remove_file(&path);
    file.write_all(ramps)?;
    file.rewind()?;
    control.set_gamma(file.as_fd());
    Ok(())
}

struct State {
    shared: Arc<Mutex<Shared>>,
    manager: Option<Manager>,
    /// Outputs announced before the manager.
    waiting: Vec<(u32, WlOutput)>,
}
impl State {
    fn control(&mut self, name: u32, output: &WlOutput, qh: &QueueHandle<Self>) {
        if let Some(manager) = &self.manager {
            let control = manager.get_gamma_control(output, qh, name);
            self.shared.lock().unwrap().outputs.push(Output {
                name,
                control,
                size: 0,
            });
        } else {
            self.waiting.push((name, output.clone()));
        }
    }
}
impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name, interface, ..
            } => match interface.as_str() {
                "zwlr_gamma_control_manager_v1" => {
                    state.manager = Some(registry.bind(name, 1, qh, ()));
                    for (name, output) in std::mem::take(&mut state.waiting) {
                        state.control(name, &output, qh);
                    }
                }
                "wl_output" => {
                    let output = registry.bind(name, 1, qh, ());
                    state.control(name, &output, qh);
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                state.waiting.retain(|(n, _)| *n != name);
                state.shared.lock().unwrap().outputs.retain(|o| {
                    if o.name == name {
                        o.control.destroy();
                    }
                    o.name != name
                });
            }
            _ => {}
        }
    }
}
impl Dispatch<Control, u32> for State {
    fn event(
        state: &mut Self,
        control: &Control,
        event: zwlr_gamma_control_v1::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mut shared = state.shared.lock().unwrap();
        match event {
            zwlr_gamma_control_v1::Event::GammaSize { size } => {
                if let Some(output) = shared.outputs.iter_mut().find(|o| o.name == *name) {
                    output.size = size;
                }
                shared.apply(Some(*name));
            }
            zwlr_gamma_control_v1::Event::Failed => {
                control.destroy();
                shared.outputs.retain(|o| o.name != *name);
                shared.error =
                    Some("Another program controls the screen colours (hyprsunset?)".into());
            }
            _ => {}
        }
    }
}
wayland_client::delegate_noop!(State: ignore WlOutput);
wayland_client::delegate_noop!(State: ignore Manager);

struct Gamma {
    connection: Connection,
    shared: Arc<Mutex<Shared>>,
}
fn gamma() -> Result<&'static Gamma> {
    static GAMMA: OnceLock<Result<Gamma, String>> = OnceLock::new();
    GAMMA
        .get_or_init(|| start().map_err(|e| format!("Night light unavailable: {e}")))
        .as_ref()
        .map_err(|e| anyhow::anyhow!("{e}"))
}
fn start() -> Result<Gamma> {
    let connection = Connection::connect_to_env()?;
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let night = night_now();
    let shared = Arc::new(Mutex::new(Shared {
        light: NightLight {
            enabled: night,
            temperature: WARM,
        },
        manual: None,
        outputs: vec![],
        error: None,
    }));
    let mut state = State {
        shared: shared.clone(),
        manager: None,
        waiting: vec![],
    };
    connection.display().get_registry(&qh, ());
    queue.roundtrip(&mut state)?;
    if state.manager.is_none() {
        bail!("the compositor does not offer gamma control");
    }
    std::thread::spawn(move || while queue.blocking_dispatch(&mut state).is_ok() {});
    Ok(Gamma { connection, shared })
}
/// Changes the light and puts it on screen.
fn change(f: impl FnOnce(&mut Shared)) -> Result<()> {
    let gamma = gamma()?;
    let mut shared = gamma.shared.lock().unwrap();
    let before = shared.light.clone();
    f(&mut shared);
    if shared.light != before {
        shared.apply(None);
        gamma.connection.flush().context("Lost the compositor")?;
    }
    Ok(())
}
/// The light on screen, after catching up with the schedule.
pub fn get() -> Result<NightLight> {
    let night = night_now();
    change(|s| {
        if s.manual.is_some_and(|m| m != night) {
            s.manual = None;
        }
        if s.manual.is_none() {
            s.light.enabled = night;
        }
    })?;
    let shared = gamma()?.shared.lock().unwrap();
    match &shared.error {
        Some(e) => bail!("{e}"),
        None => Ok(shared.light.clone()),
    }
}
pub fn set(enabled: bool, temperature: Option<u16>) -> Result<()> {
    let night = night_now();
    change(|s| {
        s.manual = Some(night);
        s.light.enabled = enabled;
        if let Some(k) = temperature {
            s.light.temperature = k;
        }
    })
}
pub fn follow_schedule() -> Result<()> {
    let night = night_now();
    change(|s| {
        s.manual = None;
        s.light.enabled = night;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schedule_wraps_midnight() {
        assert!(is_night(EVENING) && is_night(0) && is_night(MORNING - 1));
        assert!(!is_night(MORNING) && !is_night(12 * 60) && !is_night(EVENING - 1));
    }
    #[test]
    fn warm_light_cuts_blue_and_off_is_identity() {
        let [r, g, b] = gains(WARM);
        assert!(r == 1. && g < 1. && b < g);
        let off = NightLight {
            enabled: false,
            temperature: WARM,
        };
        let ramp = ramps(256, &off);
        assert_eq!(ramp.len(), 256 * 3 * 2);
        assert_eq!(u16::from_ne_bytes([ramp[510], ramp[511]]), 65535);
    }
}
