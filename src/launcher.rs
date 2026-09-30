//! Desktop entries are resolved and launched by GIO, never interpreted as shell input.
use gtk::{gio, prelude::*};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Application {
    pub icon: Option<String>,
    pub id: String,
    pub name: String,
    pub description: String,
    pub keywords: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct History(pub BTreeMap<String, u32>);
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Catalogue {
    pub applications: Vec<Application>,
    pub history: History,
    pub revision: u64,
}
impl Catalogue {
    pub fn load() -> Self {
        Self {
            applications: applications(),
            history: History::load(),
            revision: 1,
        }
    }
    pub fn launch(&mut self, id: &str) -> anyhow::Result<()> {
        use anyhow::Context;
        if !self.applications.iter().any(|a| a.id == id) {
            anyhow::bail!("Unknown application");
        }
        let app = gio::DesktopAppInfo::new(id).context("Application is no longer installed")?;
        app.launch(&[], None::<&gio::AppLaunchContext>)?;
        self.history.record(id);
        self.revision += 1;
        Ok(())
    }
}
/// The catalogue lives in the daemon; GIO invalidates it when desktop files change.
pub fn watch(catalogue: std::sync::Arc<std::sync::Mutex<Catalogue>>) {
    std::thread::spawn(move || {
        let context = gtk::glib::MainContext::new();
        let _ = context.with_thread_default(|| {
            let monitor = gio::AppInfoMonitor::get();
            monitor.connect_changed(move |_| {
                let applications = applications();
                let mut cache = catalogue.lock().unwrap();
                if cache.applications != applications {
                    cache.applications = applications;
                    cache.revision += 1;
                }
            });
            gtk::glib::MainLoop::new(Some(&context), false).run();
        });
    });
}
impl History {
    fn path() -> PathBuf {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local/state")
            })
            .join("nexus/launcher.json")
    }
    pub fn load() -> Self {
        fs::read(Self::path())
            .ok()
            .and_then(|s| serde_json::from_slice(&s).ok())
            .unwrap_or_default()
    }
    pub fn record(&mut self, id: &str) {
        let n = self.0.entry(id.to_owned()).or_default();
        *n = n.saturating_add(1);
        let path = Self::path();
        if fs::create_dir_all(path.parent().unwrap()).is_ok()
            && let Ok(json) = serde_json::to_vec(self)
        {
            let tmp = path.with_extension("tmp");
            if fs::write(&tmp, json).is_ok() {
                let _ = fs::rename(tmp, path);
            }
        }
    }
}
pub fn applications() -> Vec<Application> {
    let mut apps: Vec<_> = gio::AppInfo::all()
        .into_iter()
        .filter(|a| a.should_show())
        .map(|info| {
            let id = info
                .id()
                .map(|s| s.to_string())
                .unwrap_or_else(|| info.executable().to_string_lossy().into());
            let desktop = gio::DesktopAppInfo::new(&id);
            Application {
                name: info.display_name().to_string(),
                description: info
                    .description()
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                keywords: desktop
                    .map(|a| {
                        a.keywords()
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default(),
                id,
                icon: info
                    .icon()
                    .and_then(|i| i.to_string())
                    .map(|i| i.to_string()),
            }
        })
        .collect();
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps.dedup_by(|a, b| a.id == b.id);
    apps
}
/// Name matches take precedence over metadata; all query words must match.
pub fn score(name: &str, metadata: &str, query: &str) -> Option<i64> {
    let name = name.to_lowercase();
    let metadata = metadata.to_lowercase();
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    let mut score = 0;
    for word in query.split_whitespace() {
        score += if name == word {
            1000
        } else if name.starts_with(word) {
            700
        } else if name.split_whitespace().any(|s| s.starts_with(word)) {
            500
        } else if name.contains(word) {
            300
        } else if metadata.contains(word) {
            80
        } else {
            let mut chars = name.chars();
            if word
                .chars()
                .all(|wanted| chars.by_ref().any(|c| c == wanted))
            {
                30
            } else {
                return None;
            }
        };
    }
    Some(score)
}
pub fn ranked(apps: &[Application], history: &History, query: &str) -> Vec<usize> {
    let mut indices: Vec<_> = apps
        .iter()
        .enumerate()
        .filter_map(|(i, a)| {
            score(
                &a.name,
                &format!("{} {} {}", a.description, a.keywords, a.id),
                query,
            )
            .map(|score| {
                (
                    i,
                    score + i64::from(history.0.get(&a.id).copied().unwrap_or(0).min(25)),
                )
            })
        })
        .collect();
    indices.sort_by(|(a, sa), (b, sb)| {
        sb.cmp(sa).then_with(|| {
            apps[*a]
                .name
                .to_lowercase()
                .cmp(&apps[*b].name.to_lowercase())
        })
    });
    indices.into_iter().take(6).map(|(i, _)| i).collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_ranking() {
        assert!(
            score("Firefox", "Browser", "fire").unwrap()
                > score("Files", "Open files in Firefox", "fire").unwrap()
        );
        assert!(score("Visual Studio Code", "Editor", "vsc").is_some());
        assert!(score("Music", "Player", "music browser").is_none());
        assert_eq!(score("Calculator", "Math", "  "), Some(0));
    }
}

#[cfg(test)]
mod desktop_tests {
    use super::*;
    #[test]
    fn desktop_entry_launch_preserves_quoted_arguments() {
        let dir = std::env::temp_dir().join(format!("nexus-launch-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("a file with spaces");
        let desktop = dir.join("test.desktop");
        fs::write(&desktop,format!("[Desktop Entry]\nType=Application\nName=Nexus test\nExec=/usr/bin/touch \"{}\"\nNoDisplay=true\n",marker.display())).unwrap();
        let app = gio::DesktopAppInfo::from_filename(&desktop).unwrap();
        assert!(!app.should_show());
        app.launch(&[], None::<&gio::AppLaunchContext>).unwrap();
        for _ in 0..100 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(marker.exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
