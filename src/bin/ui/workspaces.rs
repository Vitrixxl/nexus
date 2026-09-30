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
type State = (Value, Value);
fn snapshot() -> anyhow::Result<State> {
    let ws = nexus_control::backend::run("hyprctl", &["-j", "workspaces"])?;
    let monitors = nexus_control::backend::run("hyprctl", &["-j", "monitors"])?;
    Ok((serde_json::from_str(&ws)?, serde_json::from_str(&monitors)?))
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
        assert!(!relevant("activewindow>>foot,Terminal"));
    }
}
