//! Power profiles and battery estimates from the system services, without helpers.
use anyhow::{Result, anyhow, bail};
use gtk::{gio, glib, prelude::*};
use std::collections::HashMap;

pub const MODES: [(&str, &str, &str); 3] = [
    (
        "power-saver",
        "Power saver",
        "power-profile-power-saver-symbolic",
    ),
    ("balanced", "Balanced", "power-profile-balanced-symbolic"),
    (
        "performance",
        "Performance",
        "power-profile-performance-symbolic",
    ),
];
const ENDPOINTS: [(&str, &str); 2] = [
    (
        "org.freedesktop.UPower.PowerProfiles",
        "/org/freedesktop/UPower/PowerProfiles",
    ),
    ("net.hadess.PowerProfiles", "/net/hadess/PowerProfiles"),
];
type Properties = HashMap<String, glib::Variant>;

async fn properties(
    bus: &gio::DBusConnection,
    name: &str,
    path: &str,
    interface: &str,
) -> Result<Properties> {
    let reply = bus
        .call_future(
            Some(name),
            path,
            "org.freedesktop.DBus.Properties",
            "GetAll",
            Some(&(interface,).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            3000,
        )
        .await?;
    reply
        .get::<(Properties,)>()
        .map(|(p,)| p)
        .ok_or_else(|| anyhow!("Invalid power service response"))
}

#[derive(Clone, Debug)]
pub struct Profiles {
    endpoint: usize,
    pub active: String,
    pub available: Vec<String>,
    pub inhibited: String,
    pub degraded: String,
}
impl Profiles {
    pub fn supports(&self, mode: &str) -> bool {
        self.available.iter().any(|p| p == mode)
            && (mode != "performance" || self.inhibited.is_empty())
    }
}

pub async fn profiles(bus: &gio::DBusConnection) -> Result<Profiles> {
    let mut error = anyhow!("Power profiles unavailable");
    for (endpoint, (name, path)) in ENDPOINTS.iter().enumerate() {
        match properties(bus, name, path, name).await {
            Ok(p) => {
                let active = p
                    .get("ActiveProfile")
                    .and_then(|v| v.get::<String>())
                    .ok_or_else(|| anyhow!("Missing active power profile"))?;
                let available = p
                    .get("Profiles")
                    .and_then(|v| v.get::<Vec<Properties>>())
                    .ok_or_else(|| anyhow!("Missing power profiles"))?
                    .iter()
                    .filter_map(|p| p.get("Profile").and_then(|v| v.get::<String>()))
                    .collect();
                let reason = |key| {
                    p.get(key)
                        .and_then(|v| v.get::<String>())
                        .unwrap_or_default()
                };
                return Ok(Profiles {
                    endpoint,
                    active,
                    available,
                    inhibited: reason("PerformanceInhibited"),
                    degraded: reason("PerformanceDegraded"),
                });
            }
            Err(e) => error = e,
        }
    }
    Err(error)
}

pub async fn set_profile(bus: &gio::DBusConnection, profiles: &Profiles, mode: &str) -> Result<()> {
    if !MODES.iter().any(|(id, _, _)| *id == mode) || !profiles.supports(mode) {
        bail!("Power profile unavailable: {mode}");
    }
    let (name, path) = ENDPOINTS[profiles.endpoint];
    bus.call_future(
        Some(name),
        path,
        "org.freedesktop.DBus.Properties",
        "Set",
        Some(&(name, "ActiveProfile", mode.to_variant()).to_variant()),
        None,
        gio::DBusCallFlags::ALLOW_INTERACTIVE_AUTHORIZATION,
        15000,
    )
    .await?;
    Ok(())
}

pub async fn battery_status(bus: &gio::DBusConnection) -> Result<String> {
    let p = properties(
        bus,
        "org.freedesktop.UPower",
        "/org/freedesktop/UPower/devices/DisplayDevice",
        "org.freedesktop.UPower.Device",
    )
    .await?;
    if p.get("IsPresent").and_then(|v| v.get::<bool>()) != Some(true) {
        return Ok("No battery".into());
    }
    let state = p
        .get("State")
        .and_then(|v| v.get::<u32>())
        .unwrap_or_default();
    let seconds = |key| p.get(key).and_then(|v| v.get::<i64>()).unwrap_or_default();
    Ok(estimate(
        state,
        seconds("TimeToFull"),
        seconds("TimeToEmpty"),
    ))
}

pub fn estimate(state: u32, to_full: i64, to_empty: i64) -> String {
    let (seconds, suffix) = match state {
        1 => (to_full, "until fully charged"),
        2 => (to_empty, "remaining"),
        3 => return "Battery empty".into(),
        4 => return "Fully charged".into(),
        5 => return "Plugged in · Not charging".into(),
        6 => return "Discharge pending".into(),
        _ => return "Battery estimate unavailable".into(),
    };
    if seconds <= 0 {
        return format!(
            "{} · Estimating…",
            if state == 1 { "Charging" } else { "On battery" }
        );
    }
    let minutes = seconds / 60;
    let duration = if minutes == 0 {
        "Less than a minute".into()
    } else if minutes < 60 {
        format!("{minutes} min")
    } else {
        format!("{} h {:02} min", minutes / 60, minutes % 60)
    };
    format!("{duration} {suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn charge_and_discharge_estimates() {
        assert_eq!(estimate(1, 5460, 9999), "1 h 31 min until fully charged");
        assert_eq!(estimate(2, 9999, 780), "13 min remaining");
        assert_eq!(estimate(2, 0, 20), "Less than a minute remaining");
        assert_eq!(estimate(1, 0, 0), "Charging · Estimating…");
        assert_eq!(estimate(2, 0, -1), "On battery · Estimating…");
        assert_eq!(estimate(4, 500, 500), "Fully charged");
        assert_eq!(estimate(5, 500, 500), "Plugged in · Not charging");
        assert_eq!(estimate(0, 500, 500), "Battery estimate unavailable");
    }
}
