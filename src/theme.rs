use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Mutex, OnceLock},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub mode: String,
    pub wallpaper: Option<String>,
    pub wallpaper_colors: bool,
    pub accent: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: "dark".into(),
            wallpaper: None,
            wallpaper_colors: false,
            accent: "#91b5a4".into(),
        }
    }
}
pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".config"))
        .join("nexus")
}
pub fn load() -> Settings {
    fs::read(config_dir().join("settings.json"))
        .ok()
        .and_then(|v| serde_json::from_slice(&v).ok())
        .unwrap_or_default()
}
pub fn palette(settings: &Settings) -> (&str, &str, &str, &str) {
    if settings.mode == "light" {
        ("#f3f4f0", "#ffffff", "#202824", "#69736e")
    } else {
        ("#151a18", "#202724", "#edf2ee", "#9ba9a1")
    }
}
pub fn accent_for(path: &str) -> Result<String> {
    let image = image::ImageReader::open(path)?
        .with_guessed_format()?
        .decode()?
        .thumbnail(96, 96)
        .to_rgb8();
    let mut bins = std::collections::BTreeMap::<(u8, u8, u8), (u64, [u64; 3])>::new();
    for p in image.pixels() {
        let [r, g, b] = p.0;
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        if max < 35 || min > 230 {
            continue;
        }
        let item = bins.entry((r / 32, g / 32, b / 32)).or_default();
        item.0 += 1 + u64::from(max - min) / 32;
        for (i, c) in [r, g, b].iter().enumerate() {
            item.1[i] += u64::from(*c);
        }
    }
    // Quantized dominant color with fixed bucket centers; deterministic and inexpensive.
    let (r, g, b) = bins
        .iter()
        .max_by_key(|(_, v)| v.0)
        .map(|(k, _)| *k)
        .unwrap_or((4, 5, 5));
    Ok(format!(
        "#{:02x}{:02x}{:02x}",
        r * 24 + 64,
        g * 24 + 64,
        b * 24 + 64
    ))
}
pub fn css(s: &Settings) -> String {
    let (bg, surface, fg, muted) = palette(s);
    format!(
        r#"
@define-color nexus_bg {bg}; @define-color nexus_surface {surface}; @define-color nexus_fg {fg}; @define-color nexus_muted {muted}; @define-color nexus_accent {accent};
window.nexus {{ background: @nexus_bg; color: @nexus_fg; font-family: 'Geist', 'Inter', sans-serif; }}
.nexus .sidebar {{ background: @nexus_surface; padding: 24px 14px; }}
.nexus .brand {{ font-size: 25px; font-weight: 700; letter-spacing: -1px; }}
.nexus .eyebrow {{ font-size: 10px; font-weight: 600; letter-spacing: 2px; color: @nexus_muted; }}
.nexus .title {{ font-size: 30px; font-weight: 600; letter-spacing: -1px; }}
.nexus .muted {{ color: @nexus_muted; }}
.nexus button {{ border-radius: 10px; padding: 10px 14px; background: @nexus_surface; color: @nexus_fg; border: none; box-shadow: none; }}
.nexus button:hover {{ background: alpha(@nexus_accent, 0.20); }}
.nexus button.suggested-action, .nexus .nav-active {{ background: @nexus_accent; color: #18211c; }}
.nexus .card {{ background: @nexus_surface; border-radius: 14px; padding: 18px; }}
.nexus .row-title {{ font-weight: 600; }}
.nexus .error {{ color: #d7786d; }}
.nexus entry {{ border-radius: 9px; padding: 10px; }}
.nexus scale highlight, .nexus switch:checked {{ background: @nexus_accent; }}
.nexus .power-tile {{ padding: 40px 20px; min-width: 145px; font-size: 20px; }}
.nexus .power-icon {{ font-size: 44px; }}
.nexus .danger {{ color: #e29990; }}
.nexus separator {{ background: alpha(@nexus_muted, 0.15); }}
window.shell-overlay {{ background: transparent; }}
window.power-overlay {{ background: alpha(#080d0a, 0.75); }}
.shell-panel {{ background: @nexus_bg; border-radius: 0 0 22px 22px; border: none; box-shadow: none; }}
.power-overlay .shell-panel {{ border-radius: 22px; }}
.nexus-bar {{ background: @nexus_bg; border-radius: 12px; border: none; box-shadow: none; }}
.nexus-bar button {{ padding: 3px 10px; min-height: 24px; margin: 2px 0; font-size: 12px; background: transparent; border-radius: 8px; }}
.nexus-bar .bar-brand {{ color: @nexus_accent; font-size: 14px; font-weight: 700; padding-left: 15px; }}
.nexus-bar .bar-search {{ background: @nexus_surface; padding: 3px 16px; border-radius: 10px; }}
.nexus-bar .bar-hint {{ color: @nexus_muted; font-size: 10px; }}
.nexus-bar .bar-clock {{ font-size: 12px; }}
.nexus-bar .bar-power {{ color: @nexus_accent; padding-right: 15px; font-size: 17px; }}
.nexus-bar .workspace {{ color: @nexus_muted; padding: 3px 8px; }}
.nexus-bar .workspace.active {{ background: @nexus_surface; color: @nexus_accent; }}
.launcher .search-row {{ padding: 20px 22px 16px; color: @nexus_accent; }}
.nexus .launcher entry, .nexus .launcher entry:focus-within {{ background: transparent; border: none; box-shadow: none; padding: 4px; font-size: 18px; color: @nexus_fg; }}

.launcher .keycap {{ font-size: 10px; color: @nexus_muted; border: none; border-radius: 5px; padding: 4px 6px; }}
.launcher .app-results {{ background: transparent; padding: 4px 10px 6px; }}
.launcher .app-results row {{ border-radius: 12px; background: transparent; color: @nexus_fg; }}
.launcher .app-results row:selected {{ background: alpha(@nexus_accent,0.18); }}
.launcher .app-results row:hover {{ background: alpha(@nexus_accent,0.10); }}
.launcher .app-row {{ padding: 11px 14px; }}
.launcher .launch-arrow {{ color: transparent; font-size: 13px; }}
.launcher row:selected .launch-arrow {{ color: @nexus_accent; }}
.launcher .app-name {{ font-size: 14px; font-weight: 500; }}
.launcher .results-heading {{ padding: 0 24px 6px; font-size: 10px; }}
.launcher .empty-results {{ padding: 24px; }}
.launcher row, .launcher entry, .launcher entry:focus-within {{ border: none; outline: none; }}
.launcher .launcher-footer {{ padding: 10px 22px 16px; font-size: 11px; }}

"#,
        accent = s.accent
    )
}
pub fn write_theme(s: &Settings) -> Result<()> {
    let dir = config_dir();
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("gtk.css"), css(s))?;
    Ok(())
}
static WALLPAPER: OnceLock<Mutex<Option<Child>>> = OnceLock::new();
pub fn stop_wallpaper() {
    if let Some(lock) = WALLPAPER.get()
        && let Some(mut child) = lock.lock().unwrap().take()
    {
        let _ = child.kill();
        let _ = child.wait();
    }
}
pub fn apply_wallpaper(path: &str) -> Result<()> {
    let path = fs::canonicalize(path).context("Wallpaper file does not exist")?;
    let mut child = Command::new("swaybg")
        .args(["-m", "fill", "-i"])
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Install swaybg to apply wallpapers")?;
    std::thread::sleep(std::time::Duration::from_millis(250));
    if let Some(status) = child.try_wait()? {
        bail!("Wallpaper renderer exited: {status}");
    }
    let mut old = WALLPAPER.get_or_init(|| Mutex::new(None)).lock().unwrap();
    if let Some(mut old) = old.take() {
        let _ = old.kill();
        let _ = old.wait();
    }
    // Stop the session's legacy video wallpaper only after the new image is ready.
    let _ = Command::new("pkill")
        .args([
            "-u",
            &std::env::var("USER").unwrap_or_default(),
            "-x",
            "mpvpaper",
        ])
        .status();
    *old = Some(child);
    Ok(())
}
pub fn save(mut settings: Settings) -> Result<Settings> {
    if !["dark", "light"].contains(&settings.mode.as_str()) {
        bail!("Unknown theme mode");
    }
    if settings.accent.len() != 7
        || !settings.accent.starts_with('#')
        || !settings.accent[1..].chars().all(|c| c.is_ascii_hexdigit())
    {
        bail!("Invalid accent color");
    }
    let old = load();
    if let Some(path) = &settings.wallpaper {
        image::ImageReader::open(path)?
            .with_guessed_format()?
            .into_dimensions()
            .context("Choose a PNG, JPEG or WebP image")?;
        if settings.wallpaper_colors {
            settings.accent = accent_for(path)?;
        }
        if old.wallpaper != settings.wallpaper {
            apply_wallpaper(path)?;
        }
    }
    if !settings.wallpaper_colors {
        settings.accent = Settings::default().accent;
    }
    write_theme(&settings)?;
    let dir = config_dir();
    fs::write(
        dir.join("settings.json.tmp"),
        serde_json::to_vec_pretty(&settings)?,
    )?;
    fs::rename(dir.join("settings.json.tmp"), dir.join("settings.json"))?;
    let _ = Command::new("gsettings")
        .args([
            "set",
            "org.gnome.desktop.interface",
            "color-scheme",
            if settings.mode == "dark" {
                "prefer-dark"
            } else {
                "prefer-light"
            },
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    Ok(settings)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_css_injection() {
        let s = Settings {
            accent: "red; }".into(),
            ..Settings::default()
        };
        assert!(save(s).is_err());
    }
    #[test]
    fn generated_accent_is_stable() {
        let p = std::env::temp_dir().join(format!("nexus-color-{}.png", std::process::id()));
        image::RgbImage::from_pixel(4, 4, image::Rgb([60, 130, 180]))
            .save(&p)
            .unwrap();
        assert_eq!(accent_for(p.to_str().unwrap()).unwrap(), "#58a0b8");
        fs::remove_file(p).unwrap();
    }
}
