use nexus_control::theme;
use serde_json::Value;
use std::{
    io::{BufRead, BufReader},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
/// Workspaces, monitors, windows and the focused window, as Hyprland lists them.
pub type State = (Value, Value, Value, Value);
fn snapshot() -> anyhow::Result<State> {
    use nexus_control::backend::hypr_socket;
    let read = |what: &str| -> anyhow::Result<Value> {
        Ok(serde_json::from_str(&hypr_socket(
            ".socket.sock",
            &format!("j/{what}"),
        )?)?)
    };
    Ok((
        read("workspaces")?,
        read("monitors")?,
        read("clients")?,
        read("activewindow")?,
    ))
}
fn relevant(line: &str) -> bool {
    let event = line.split_once(">>").map(|(kind, _)| kind).unwrap_or("");
    matches!(
        event,
        "workspacev2"
            | "focusedmon"
            | "focusedmonv2"
            | "createworkspacev2"
            | "destroyworkspacev2"
            | "moveworkspacev2"
            | "renameworkspace"
            | "monitoraddedv2"
            | "monitorremoved"
            | "activewindowv2"
            | "openwindow"
            | "closewindow"
            | "movewindowv2"
            | "configreloaded"
    )
}
pub fn watch(tx: async_channel::Sender<State>, running: Arc<AtomicBool>) {
    thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            let path = std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .zip(std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE"))
                .map(|(p, id)| p.join("hypr").join(id).join(".socket2.sock"));
            if let Some(path) = path
                && let Ok(stream) = UnixStream::connect(path)
            {
                // A compositor just started, or its configuration reloaded, is back
                // to the blur in its configuration file.
                theme::apply_blur(&theme::load());
                if let Ok(state) = snapshot()
                    && tx.send_blocking(state).is_err()
                {
                    break;
                }
                for line in BufReader::new(stream).lines() {
                    let Ok(line) = line else {
                        break;
                    };
                    if !running.load(Ordering::Relaxed) {
                        return;
                    }
                    if line.starts_with("configreloaded") {
                        theme::apply_blur(&theme::load());
                    }
                    if relevant(&line)
                        && let Ok(state) = snapshot()
                        && tx.send_blocking(state).is_err()
                    {
                        return;
                    }
                }
            }
            // Retry only a disconnected compositor; normal workspace updates are event-driven.
            thread::sleep(Duration::from_secs(1));
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn workspace_events_are_selected() {
        assert!(relevant("workspacev2>>2,2"));
        assert!(relevant("focusedmon>>eDP-1,1"));
        assert!(relevant("destroyworkspacev2>>3,3"));
        assert!(relevant("openwindow>>a,1,foot,Terminal"));
        assert!(!relevant("activewindow>>foot,Terminal"));
    }
}
