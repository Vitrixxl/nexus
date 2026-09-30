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
window.nexus {{ background: @nexus_bg; color: @nexus_fg; font-family: 'Geist', 'Inter', sans-serif; font-size: 14px; }}
.nexus .muted {{ color: @nexus_muted; }}
.nexus .caption {{ font-size: 12px; }}
.nexus .title {{ font-size: 26px; font-weight: 650; letter-spacing: -0.5px; }}
.nexus .row-title {{ font-weight: 600; }}
.nexus .eyebrow {{ font-size: 10px; font-weight: 600; letter-spacing: 2px; color: @nexus_muted; }}
.nexus .error {{ color: #d7786d; }}
.nexus .banner {{ background: alpha(#d7786d, 0.12); border-radius: 10px; padding: 10px 14px; }}
.nexus .empty-state {{ color: @nexus_muted; padding: 36px 0; }}

.nexus button {{ min-height: 0; min-width: 0; border-radius: 10px; padding: 8px 14px; background: alpha(@nexus_fg, 0.07); color: @nexus_fg; border: none; box-shadow: none; text-shadow: none; outline: none; }}
.nexus button:hover {{ background: alpha(@nexus_fg, 0.12); }}
.nexus button:active {{ background: alpha(@nexus_fg, 0.17); }}
.nexus button:disabled {{ opacity: 0.45; }}
.nexus button:focus-visible {{ outline: 2px solid alpha(@nexus_accent, 0.7); outline-offset: -2px; }}
.nexus button.flat {{ background: transparent; color: @nexus_muted; }}
.nexus button.flat:hover {{ background: alpha(@nexus_fg, 0.07); color: @nexus_fg; }}
.nexus button.suggested-action {{ background: @nexus_accent; color: #16201b; font-weight: 600; }}
.nexus button.suggested-action:hover {{ background: shade(@nexus_accent, 1.08); }}
.nexus button.destructive-action {{ background: #c4655a; color: #ffffff; font-weight: 600; }}
.nexus button.destructive-action:hover {{ background: #d27468; }}

.control-center .sidebar {{ background: @nexus_surface; padding: 10px 12px 12px; border-right: 1px solid alpha(@nexus_fg, 0.05); }}
.control-center .sidebar-section {{ font-size: 12px; font-weight: 600; color: @nexus_muted; padding: 18px 12px 8px; }}
.control-center separator {{ margin: 0 8px; min-height: 1px; background: alpha(@nexus_fg, 0.07); }}
.nexus button.nav {{ background: transparent; padding: 9px 10px; border-radius: 12px; color: alpha(@nexus_fg, 0.85); font-size: 15px; font-weight: 500; }}
.nexus button.nav:hover {{ background: alpha(@nexus_fg, 0.05); color: @nexus_fg; }}
.nexus button.nav:focus-visible {{ outline: none; background: alpha(@nexus_fg, 0.06); }}
.nexus button.nav .nav-icon {{ background: alpha(@nexus_fg, 0.07); color: alpha(@nexus_fg, 0.85); border-radius: 10px; min-width: 36px; min-height: 36px; }}
.nexus button.nav .nav-detail {{ color: @nexus_muted; font-size: 13px; font-weight: 400; }}
.nexus button.nav.nav-active {{ background: alpha(@nexus_fg, 0.08); color: @nexus_fg; font-weight: 600; }}
.nexus button.nav.nav-active .nav-icon {{ background: @nexus_accent; color: #16201b; }}
.nexus button.nav.power-nav:hover .nav-icon {{ background: alpha(#d7786d, 0.2); color: #e29990; }}
.control-center .status {{ font-size: 12px; padding: 0 32px 16px; }}

.nexus .card {{ background: @nexus_surface; border-radius: 14px; padding: 14px 16px; }}
.nexus .row-icon {{ color: @nexus_muted; }}
.nexus button.stream-mute {{ padding: 6px; }}
.nexus button.stream-mute:checked {{ background: alpha(#d7786d, 0.16); color: #e29990; }}
.nexus .list-row.active .row-icon {{ color: @nexus_accent; }}
.nexus .segmented {{ background: alpha(@nexus_fg, 0.07); border-radius: 10px; padding: 3px; }}
.nexus .segmented button {{ background: transparent; padding: 6px 16px; border-radius: 8px; color: @nexus_muted; }}
.nexus .segmented button:hover {{ color: @nexus_fg; }}
.nexus .segmented button:checked {{ background: @nexus_accent; color: #16201b; font-weight: 600; }}
.nexus .wallpaper-frame {{ border-radius: 10px; background: alpha(@nexus_fg, 0.05); }}

.nexus switch {{ background: alpha(@nexus_fg, 0.16); border: none; border-radius: 999px; padding: 3px; box-shadow: none; outline: none; }}
.nexus switch:checked {{ background: @nexus_accent; }}
.nexus switch image {{ color: transparent; }}
.nexus switch > slider {{ background: #ffffff; border: none; border-radius: 999px; min-width: 18px; min-height: 18px; margin: 0; box-shadow: 0 1px 2px alpha(black, 0.3); }}
.nexus switch:disabled {{ opacity: 0.45; }}

.nexus scale {{ padding: 8px 0; }}
.nexus scale > trough {{ background: alpha(@nexus_fg, 0.12); border: none; border-radius: 999px; min-height: 6px; outline: none; }}
.nexus scale > trough > highlight {{ background: @nexus_accent; border: none; border-radius: 999px; }}
.nexus scale > trough > slider {{ background: #ffffff; border: none; border-radius: 999px; min-width: 18px; min-height: 18px; margin: -6px; box-shadow: 0 1px 3px alpha(black, 0.35); }}
.nexus scale > value {{ color: @nexus_muted; font-size: 12px; min-width: 40px; margin-left: 12px; font-feature-settings: "tnum"; }}
.nexus scale:disabled {{ opacity: 0.45; }}

.nexus dropdown > button {{ background: alpha(@nexus_fg, 0.06); padding: 8px 12px; }}
.nexus popover > contents {{ background: @nexus_surface; color: @nexus_fg; border-radius: 12px; padding: 6px; border: 1px solid alpha(@nexus_fg, 0.08); box-shadow: 0 8px 24px alpha(black, 0.35); }}
.nexus popover > arrow {{ background: @nexus_surface; border: 1px solid alpha(@nexus_fg, 0.08); }}
.nexus popover listview {{ background: transparent; }}
.nexus popover listview > row {{ border-radius: 8px; padding: 8px 10px; }}
.nexus popover listview > row:hover {{ background: alpha(@nexus_fg, 0.07); }}
.nexus popover listview > row:selected {{ background: alpha(@nexus_accent, 0.18); color: @nexus_fg; }}

.nexus entry {{ background: alpha(@nexus_fg, 0.07); color: @nexus_fg; border: 1px solid transparent; border-radius: 10px; padding: 8px 10px; box-shadow: none; outline: none; }}
.nexus entry:focus-within {{ border-color: alpha(@nexus_accent, 0.8); }}
.nexus scrollbar {{ background: transparent; border: none; }}
.nexus scrollbar slider {{ background: alpha(@nexus_fg, 0.2); border: none; border-radius: 999px; min-width: 4px; min-height: 24px; }}
.nexus separator {{ background: alpha(@nexus_muted, 0.15); }}

window.shell-overlay {{ background: transparent; }}
.panel-fillet {{ color: @nexus_bg; }}
window.power-overlay {{ background: alpha(#080d0a, 0.72); }}
.shell-panel {{ background: @nexus_bg; border-radius: 0 0 22px 22px; border: none; box-shadow: 0 24px 44px -12px alpha(black, 0.7), 0 8px 16px -8px alpha(black, 0.45); }}
.power-overlay .shell-panel {{ border-radius: 22px; border: 1px solid alpha(@nexus_fg, 0.06); box-shadow: 0 24px 48px -12px alpha(black, 0.6); }}
.nexus .power-page {{ padding: 36px 40px 24px; min-width: 520px; }}
.nexus button.power-tile {{ padding: 28px 18px 22px; font-size: 15px; font-weight: 500; border-radius: 16px; background: alpha(@nexus_fg, 0.06); }}
.nexus button.power-tile:hover {{ background: alpha(@nexus_fg, 0.1); }}
.nexus button.power-tile.selected {{ background: alpha(@nexus_accent, 0.2); }}
.nexus button.power-tile.danger {{ color: #e29990; }}
.nexus button.power-tile.danger.selected {{ background: alpha(#d7786d, 0.18); }}

window.prompt-window {{ background: transparent; }}
.nexus .prompt {{ background: @nexus_bg; border-radius: 18px; padding: 24px; border: 1px solid alpha(@nexus_fg, 0.1); }}
.nexus .prompt .title {{ font-size: 20px; }}

.nexus-bar {{ background: @nexus_bg; border-radius: 12px; border: none; box-shadow: none; }}
.nexus-bar .bar-content {{ padding: 4px; min-height: 26px; }}
.nexus-bar button {{ padding: 0 10px; min-height: 26px; margin: 0 1px; font-size: 12px; background: transparent; border-radius: 8px; }}
.nexus-bar button:hover {{ background: alpha(@nexus_fg, 0.08); }}
.nexus-bar button.bar-static:hover {{ background: transparent; }}
.nexus-bar .bar-brand {{ color: @nexus_accent; font-size: 14px; font-weight: 700; padding: 0 12px; }}
.nexus-bar .bar-search {{ background: @nexus_surface; padding: 0 16px; border-radius: 8px; }}
.nexus-bar .bar-search:hover {{ background: shade(@nexus_surface, 1.15); }}
.nexus-bar .bar-hint {{ color: @nexus_muted; font-size: 10px; }}
.nexus-bar .bar-clock {{ font-size: 12px; }}
.nexus-bar .bar-power {{ color: @nexus_accent; padding: 0 12px; }}
.nexus-bar .workspace {{ color: @nexus_muted; padding: 0; min-width: 26px; }}
.nexus-bar .workspace.active {{ background: @nexus_surface; color: @nexus_accent; }}
.launcher .search-row {{ padding: 20px 22px 16px; color: @nexus_accent; }}
.nexus .launcher entry, .nexus .launcher entry:focus-within {{ background: transparent; border: none; box-shadow: none; padding: 4px; font-size: 18px; color: @nexus_fg; }}
.launcher .keycap {{ font-size: 10px; color: @nexus_muted; border: none; border-radius: 5px; padding: 4px 6px; }}
.launcher .app-results {{ background: transparent; padding: 4px 10px 6px; }}
.launcher .app-results row {{ border-radius: 12px; background: transparent; color: @nexus_fg; outline: none; }}
.launcher, .launcher *, .nexus button.nav, .nexus button.nav * {{ transition: none; }}
.launcher .app-results row:hover {{ background: alpha(@nexus_accent,0.10); }}
.launcher .app-results row:selected {{ background: alpha(@nexus_accent,0.18); }}
.launcher .app-row {{ padding: 8px 12px; }}
.launcher .launch-arrow {{ color: transparent; font-size: 13px; }}
.launcher row:selected .launch-arrow {{ color: @nexus_accent; }}
.launcher .app-name {{ font-size: 15px; font-weight: 600; }}
.launcher .app-description {{ font-size: 12px; color: @nexus_muted; }}
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
