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
    pub style: String,
    /// Opacity of the surfaces' background alone, in percent; their text and icons
    /// stay solid. By default, what the style says.
    pub opacity: Option<u8>,
    /// Which of [`LOCK_STYLES`] the lock screen uses.
    pub lock_style: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: "dark".into(),
            wallpaper: None,
            wallpaper_colors: false,
            accent: "#0a84ff".into(),
            style: "blur".into(),
            opacity: None,
            lock_style: LOCK_STYLES[0].0.into(),
        }
    }
}
impl Settings {
    pub fn dark(&self) -> bool {
        self.mode != "light"
    }
}
/// `$XDG_CONFIG_HOME`, where other applications keep their configuration.
pub(crate) fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".config"))
}
pub fn config_dir() -> PathBuf {
    config_home().join("nexus")
}
pub fn load() -> Settings {
    parse(&fs::read(config_dir().join("settings.json")).unwrap_or_default())
}
/// A style from another version becomes what it is shown as, or [`check`] would
/// refuse every later save.
fn parse(file: &[u8]) -> Settings {
    let mut settings: Settings = serde_json::from_slice(file).unwrap_or_default();
    settings.style = settings.style().id.into();
    if !LOCK_STYLES.iter().any(|l| l.0 == settings.lock_style) {
        settings.lock_style = Settings::default().lock_style;
    }
    settings
}
pub fn palette(settings: &Settings) -> (&str, &str, &str, &str) {
    palette_for(settings.dark())
}
/// Background, surface, text and secondary text colours.
fn palette_for(dark: bool) -> (&'static str, &'static str, &'static str, &'static str) {
    if dark {
        ("#141416", "#1f1f22", "#f4f4f5", "#b4b4bb")
    } else {
        ("#f2f2f5", "#ffffff", "#1d1d1f", "#62626a")
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
    Ok(hex(vivid([r, g, b].map(|c| f64::from(c * 24 + 64) / 255.))))
}
/// The same hue with enough colour to carry a selection or a slider: a dominant
/// colour off a photograph is usually much too muddy for that. A hue too grey to
/// tell gets the system blue.
fn vivid([r, g, b]: [f64; 3]) -> [f64; 3] {
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let chroma = max - min;
    if chroma < 0.04 {
        return rgb(&Settings::default().accent);
    }
    let hue = if max == r {
        ((g - b) / chroma).rem_euclid(6.)
    } else if max == g {
        (b - r) / chroma + 2.
    } else {
        (r - g) / chroma + 4.
    };
    let mut lightness = ((max + min) / 2.).clamp(0.44, 0.56);
    let mut saturation =
        ((chroma / (1. - (2. * ((max + min) / 2.) - 1.).abs())) * 1.25).clamp(0.62, 0.9);
    // Red through yellow: at these values, brown and mustard. They need all their
    // colour and some light to read as orange or amber.
    if (0.2..1.2).contains(&hue) {
        lightness = lightness.max(0.54);
        saturation = saturation.max(0.88);
    }
    let c = (1. - (2. * lightness - 1.).abs()) * saturation;
    let x = c * (1. - (hue.rem_euclid(2.) - 1.).abs());
    let m = lightness - c / 2.;
    let (r, g, b) = match hue as u8 {
        0 => (c, x, 0.),
        1 => (x, c, 0.),
        2 => (0., c, x),
        3 => (0., x, c),
        4 => (x, 0., c),
        _ => (c, 0., x),
    };
    [r + m, g + m, b + m]
}
pub(crate) fn rgb(hex: &str) -> [f64; 3] {
    let hex = hex.trim_start_matches('#');
    let channel = |i: usize| {
        u8::from_str_radix(hex.get(i..i + 2).unwrap_or("80"), 16).unwrap_or(128) as f64 / 255.
    };
    [channel(0), channel(2), channel(4)]
}
pub(crate) fn hex([r, g, b]: [f64; 3]) -> String {
    let c = |v: f64| (v.clamp(0., 1.) * 255.).round() as u8;
    format!("#{:02x}{:02x}{:02x}", c(r), c(g), c(b))
}
/// WCAG relative luminance.
fn luminance(c: [f64; 3]) -> f64 {
    let lin = |v: f64| {
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
}
fn contrast(a: [f64; 3], b: [f64; 3]) -> f64 {
    let (x, y) = (luminance(a), luminance(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}
/// The accent shifted toward black or white until it reads as text on `bg`.
fn readable_on(accent: [f64; 3], bg: [f64; 3]) -> [f64; 3] {
    let target = if luminance(bg) > 0.5 { 0. } else { 1. };
    let mut c = accent;
    for _ in 0..10 {
        if contrast(c, bg) >= 3.5 {
            break;
        }
        c = c.map(|v| v + (target - v) * 0.15);
    }
    c
}
pub(crate) fn mix(a: [f64; 3], b: [f64; 3], t: f64) -> [f64; 3] {
    [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * t)
}
/// Text and icons drawn on an accent fill.
fn on_accent(accent: [f64; 3]) -> &'static str {
    if contrast(accent, rgb("#16201b")) >= contrast(accent, [1.; 3]) {
        "#16201b"
    } else {
        "#ffffff"
    }
}
/// The palette of one mode with the accent and the colours derived from it.
pub(crate) struct Palette {
    pub dark: bool,
    pub bg: [f64; 3],
    pub surface: [f64; 3],
    pub fg: [f64; 3],
    pub muted: [f64; 3],
    pub accent: [f64; 3],
    /// The accent adjusted to read as text on the background.
    pub accent_fg: [f64; 3],
    pub on_accent: &'static str,
}
impl Palette {
    pub fn new(accent: &str, dark: bool) -> Self {
        let (bg, surface, fg, muted) = palette_for(dark);
        let (bg, accent) = (rgb(bg), rgb(accent));
        Self {
            dark,
            bg,
            surface: rgb(surface),
            fg: rgb(fg),
            muted: rgb(muted),
            accent,
            accent_fg: readable_on(accent, bg),
            on_accent: on_accent(accent),
        }
    }
}
/// The accents to choose from when the wallpaper does not provide one.
pub const ACCENTS: [(&str, &str); 8] = [
    ("Blue", "#0a84ff"),
    ("Purple", "#bf5af2"),
    ("Pink", "#ff375f"),
    ("Red", "#ff453a"),
    ("Orange", "#ff9f0a"),
    ("Yellow", "#ffd60a"),
    ("Green", "#30d158"),
    ("Teal", "#40c8e0"),
];
/// The lock screen's compositions: id, name and what sets it apart.
pub const LOCK_STYLES: [(&str, &str, &str); 3] = [
    (
        "panel",
        "Side panel",
        "A solid column with everything in one place",
    ),
    (
        "clock",
        "Large clock",
        "Hours over minutes as large as the screen allows, the rest beside them",
    ),
    (
        "bar",
        "Bottom bar",
        "The time above, one strip along the bottom",
    ),
];
/// A material for the shell: how much of the desktop shows through what floats, and
/// what the compositor does behind it. Windows stay solid in both: they are read.
pub struct Style {
    pub id: &'static str,
    pub name: &'static str,
    /// Opacity of what floats, dark then light.
    panel: (f64, f64),
    /// `decoration.blur` for this style in Hyprland's Lua, dark then light.
    blur: (&'static str, &'static str),
}
pub const STYLES: [Style; 2] = [
    Style {
        id: "opaque",
        name: "Opaque",
        panel: (1.0, 1.0),
        // Nothing shows through: blurring behind it would be work for nothing.
        blur: ("enabled = false", "enabled = false"),
    },
    Style {
        id: "blur",
        name: "Blur",
        panel: (0.88, 0.97),
        // Two passes: one leaves text behind readable as ghosts, a third is not seen
        // through this much opacity, and each costs a draw of the whole surface on
        // every frame under a floating layer. Vibrancy is not seen through it either.
        // Dimmed under dark panels; under light ones that would turn them grey.
        blur: (
            "enabled = true, variant = 0, size = 10, passes = 2, vibrancy = 0, vibrancy_darkness = 0, contrast = 0.9, brightness = 0.85, noise = 0",
            "enabled = true, variant = 0, size = 10, passes = 2, vibrancy = 0, vibrancy_darkness = 0, contrast = 1.0, brightness = 1.0, noise = 0",
        ),
    },
];
impl Settings {
    pub fn opacity(&self) -> u8 {
        let style = self.style().panel;
        let default = if self.dark() { style.0 } else { style.1 };
        self.opacity
            .unwrap_or((default * 100.).round() as u8)
            .min(100)
    }
    pub fn style(&self) -> &'static Style {
        STYLES
            .iter()
            .find(|style| style.id == self.style)
            // Styles from earlier versions, or none: the middle ground.
            .unwrap_or(&STYLES[1])
    }
}
/// Tells the compositor how to blur behind this style. The rest of its look is
/// in the stylesheet; this part cannot be.
pub fn apply_blur(s: &Settings) {
    let blur = s.style().blur;
    let blur = if s.dark() { blur.0 } else { blur.1 };
    let lua = format!("hl.config({{ decoration = {{ blur = {{ {blur} }} }} }})");
    if let Err(e) = crate::backend::hypr_socket(".socket.sock", &format!("eval {lua}")) {
        eprintln!("Could not set the blur: {e}");
    }
}
fn rgba(c: [f64; 3], a: f64) -> String {
    let c = c.map(|v| (v.clamp(0., 1.) * 255.).round() as u8);
    format!("rgba({}, {}, {}, {a:.3})", c[0], c[1], c[2])
}
pub fn css(s: &Settings) -> String {
    let (bg, surface, fg, muted) = palette(s);
    let light = s.mode == "light";
    let p = Palette::new(&s.accent, s.dark());
    // Text and icons drawn on an accent fill, and the accent used as text colour.
    let (on_accent, accent_fg) = (p.on_accent, hex(p.accent_fg));
    let (error, danger, shadow) = if light {
        ("#d70015", "#d70015", "rgba(0, 0, 0, 0.22)")
    } else {
        ("#ff453a", "#ff6961", "rgba(0, 0, 0, 0.55)")
    };
    let opacity = f64::from(s.opacity()) / 100.;
    let glass = rgba(p.bg, opacity);
    // What separates a surface from what is behind it: never a light outline.
    let edge = rgba([0.; 3], if light { 0.12 } else { 0.5 });
    let surface_shadow = "0 0 0 1px @nexus_edge, 0 14px 32px -12px @nexus_shadow";
    let surface_shadow_small = "0 0 0 1px @nexus_edge, 0 6px 14px -8px @nexus_shadow";
    // Asks the compositor for blur behind the surface: GTK turns this into the
    // background-effect request that Hyprland honours. Nothing to blur when opaque.
    let backdrop = if opacity < 1. { "blur(20px)" } else { "none" };
    // In light, what sits on a surface rises from it; in dark, it is a lighter veil.
    let (raised, tile, checked, checked_shadow, center_muted) = if light {
        (
            "@nexus_surface",
            "@nexus_surface",
            "@nexus_surface",
            "0 1px 2px alpha(black, 0.14)",
            "#55555c",
        )
    } else {
        (
            "alpha(@nexus_fg, 0.045)",
            "alpha(@nexus_fg, 0.06)",
            "alpha(@nexus_fg, 0.16)",
            "none",
            "@nexus_muted",
        )
    };
    // Light over a dark desktop turns grey unless it nearly covers it.
    let lock_scrim = if light { 0.82 } else { 0.62 };
    let power_entrances = power_entrances();
    let accent_dots: String = ACCENTS
        .iter()
        .enumerate()
        .map(|(i, (_, color))| {
            format!(".nexus button.accent-dot.accent-{i}, .nexus button.accent-dot.accent-{i}:hover {{ background: {color}; }}\n")
        })
        .collect();
    format!(
        r#"
@define-color nexus_bg {bg}; @define-color nexus_surface {surface}; @define-color nexus_fg {fg}; @define-color nexus_muted {muted}; @define-color nexus_accent {accent};
@define-color nexus_on_accent {on_accent}; @define-color nexus_accent_fg {accent_fg}; @define-color nexus_error {error}; @define-color nexus_danger {danger}; @define-color nexus_shadow {shadow};
@define-color nexus_glass {glass}; @define-color nexus_edge {edge};

/* Radii: floating surfaces 16, what sits in them 12, controls 8. Nothing eases
   but what says so below: the toolkit's theme animates every row and field. */
.nexus * {{ transition: none; }}
/* The base theme has no spinner animation of its own: spinners stood still. */
@keyframes nexus-spin {{ to {{ -gtk-icon-transform: rotate(1turn); }} }}
.nexus spinner:checked {{ animation: nexus-spin 0.9s linear infinite; }}
window.nexus {{ background: @nexus_bg; color: @nexus_fg; font-family: 'Geist', 'Inter', sans-serif; font-size: 14px; font-feature-settings: "cv11"; }}
selection {{ background: alpha(@nexus_accent, 0.42); color: @nexus_fg; }}
.nexus .muted, .nexus .dim-label {{ color: @nexus_muted; opacity: 1; }}
.nexus .caption {{ font-size: 12px; }}
.nexus .title {{ font-size: 22px; font-weight: 650; letter-spacing: -0.3px; }}
.nexus .section-title {{ font-size: 15px; font-weight: 650; }}
.nexus .row-title {{ font-weight: 550; color: @nexus_fg; }}
.nexus .eyebrow {{ font-size: 13px; font-weight: 600; color: @nexus_muted; }}
.nexus .error {{ color: @nexus_error; }}
.nexus .banner {{ background: alpha(@nexus_error, 0.12); color: @nexus_fg; border-radius: 8px; padding: 8px 12px; }}
.nexus .empty-state {{ color: @nexus_muted; padding: 28px 0; }}

/* The material: what every floating surface is made of. */
.shell-panel, .drawer-panel, .notification-popups .notification, .nexus .clipboard-card, .nexus .prompt, .osd-card, .dock-plate, .nexus button.power-tile {{
  background-color: @nexus_glass;
  box-shadow: {surface_shadow};
  backdrop-filter: {backdrop};
}}

.nexus button {{ min-height: 0; min-width: 0; font-weight: 500; border-radius: 8px; padding: 6px 12px; background: alpha(@nexus_fg, 0.08); color: @nexus_fg; border: none; box-shadow: none; text-shadow: none; outline: none; transition: background-color 80ms ease, color 80ms ease; }}
.nexus button:hover {{ background: alpha(@nexus_fg, 0.13); }}
.nexus button:active {{ background: alpha(@nexus_fg, 0.18); transition-duration: 30ms; }}
.nexus button:disabled {{ opacity: 0.45; }}
.nexus button:focus-visible {{ outline: 2px solid alpha(@nexus_accent, 0.8); outline-offset: 1px; }}
.nexus button.flat {{ background: transparent; color: @nexus_muted; }}
.nexus button.flat:hover {{ background: alpha(@nexus_fg, 0.08); color: @nexus_fg; }}
.nexus button.flat.dismiss, .nexus button.flat.dismiss:hover {{ background: transparent; padding: 4px; }}
.nexus .wifi-password .secret {{ font-family: 'Geist Mono', monospace; font-size: 13px; }}
.nexus .wifi-password button.copy {{ min-width: 28px; min-height: 28px; padding: 0; }}
.nexus button.suggested-action {{ background: @nexus_accent; color: @nexus_on_accent; font-weight: 600; }}
.nexus button.suggested-action:hover {{ background: mix(@nexus_accent, @nexus_fg, 0.12); }}
.nexus button.destructive-action {{ background: @nexus_error; color: #ffffff; font-weight: 600; }}
.nexus button.destructive-action:hover {{ background: shade(@nexus_error, 1.1); }}
.nexus button.icon {{ min-width: 32px; min-height: 32px; padding: 0; }}

/* The settings window: a quiet column, then one page. */
.control-center .sidebar {{ background: mix(@nexus_bg, @nexus_fg, 0.025); padding: 10px 8px 10px; }}
.control-center .sidebar-head {{ padding: 2px 2px 12px 10px; }}
.control-center .sidebar-title {{ font-size: 15px; font-weight: 650; }}
.nexus button.control-close {{ min-width: 28px; min-height: 28px; padding: 0; }}
.nexus button.nav {{ background: transparent; padding: 6px 10px; color: @nexus_fg; font-weight: 500; }}
.nexus button.nav:hover {{ background: alpha(@nexus_fg, 0.06); }}
.nexus button.nav:focus-visible {{ outline-offset: -2px; }}
.nexus button.nav .nav-icon {{ color: @nexus_muted; }}
.nexus button.nav.nav-active {{ background: alpha(@nexus_fg, 0.1); font-weight: 600; }}
.nexus button.nav.nav-active .nav-icon {{ color: @nexus_fg; }}
.control-center .status {{ font-size: 12px; padding: 0 32px 12px; }}

/* Settings come in groups: a heading, then rows sharing one surface. */
.nexus .group-title {{ font-size: 13px; font-weight: 600; color: @nexus_muted; padding: 0 2px 6px; margin-top: 20px; }}
.nexus button.group-action {{ padding: 2px 8px; margin-top: 16px; margin-bottom: 4px; font-size: 13px; }}
.nexus .group {{ background: {raised}; border-radius: 12px; }}
.nexus .row {{ padding: 8px 14px; min-height: 32px; border-bottom: 1px solid alpha(@nexus_fg, 0.06); }}
.nexus .row:last-child {{ border-bottom: none; }}
.nexus .card {{ background: {raised}; color: @nexus_fg; border: none; box-shadow: none; border-radius: 12px; padding: 12px 14px; }}
.nexus .wallpaper-thumb {{ border-radius: 8px; background: alpha(@nexus_fg, 0.08); }}
.nexus button.lock-choice {{ padding: 8px 14px; border-radius: 0; background: transparent; border-bottom: 1px solid alpha(@nexus_fg, 0.06); }}
.nexus button.lock-choice:last-child {{ border-bottom: none; }}
.nexus button.lock-choice:hover {{ background: alpha(@nexus_fg, 0.05); }}
.nexus button.lock-choice:focus-visible {{ outline-offset: -2px; }}
.nexus button.lock-choice .lock-tick {{ opacity: 0; color: @nexus_accent_fg; }}
.nexus button.lock-choice:checked .lock-tick {{ opacity: 1; }}
.nexus .lock-name {{ font-weight: 550; }}
.nexus button.stream-mute {{ padding: 6px; }}
.nexus button.stream-mute:checked {{ background: alpha(@nexus_error, 0.16); color: @nexus_danger; }}
.nexus .segmented {{ background: alpha(@nexus_fg, 0.08); border-radius: 8px; padding: 2px; }}
.nexus .segmented button {{ background: transparent; padding: 4px 12px; border-radius: 6px; color: @nexus_muted; font-size: 13px; }}
.nexus .segmented button:hover {{ color: @nexus_fg; }}
.nexus .segmented button:checked {{ background: {checked}; box-shadow: {checked_shadow}; color: @nexus_fg; font-weight: 600; }}

/* Appearance: the accents. */
.nexus button.accent-dot {{ min-width: 18px; min-height: 18px; padding: 0; border-radius: 999px; }}
.nexus button.accent-dot.selected {{ box-shadow: 0 0 0 2px @nexus_bg, 0 0 0 4px @nexus_fg; }}

{accent_dots}
.nexus switch {{ background: alpha(@nexus_fg, 0.2); border: none; border-radius: 999px; padding: 2px; box-shadow: none; outline: none; transition: background-color 120ms ease; }}
.nexus switch:checked {{ background: @nexus_accent; }}
.nexus switch image {{ color: transparent; }}
.nexus switch > slider {{ background: #ffffff; border: none; border-radius: 999px; min-width: 20px; min-height: 20px; margin: 0; box-shadow: 0 1px 2px alpha(black, 0.3); }}
.nexus switch:disabled {{ opacity: 0.45; }}
.nexus switch:focus-visible {{ outline: 2px solid alpha(@nexus_accent, 0.8); outline-offset: 1px; }}

.nexus scale {{ padding: 8px 0; }}
.nexus scale > trough {{ background: alpha(@nexus_fg, 0.14); border: none; border-radius: 999px; min-height: 6px; outline: none; }}
.nexus scale > trough > highlight {{ background: @nexus_accent; border: none; border-radius: 999px; margin: 0; min-height: 0; min-width: 0; }}
.nexus scale > trough > slider {{ background: #ffffff; border: 1px solid alpha(@nexus_fg, 0.5); border-radius: 999px; min-width: 16px; min-height: 16px; margin: -6px; box-shadow: 0 1px 3px alpha(black, 0.4); }}
.nexus scale > value {{ color: alpha(@nexus_fg, 0.8); opacity: 1; font-size: 12px; min-width: 40px; margin-left: 10px; font-feature-settings: "tnum"; }}
.nexus scale:disabled {{ opacity: 0.45; }}

.nexus dropdown > button {{ background: alpha(@nexus_fg, 0.07); padding: 6px 10px; }}
.nexus popover > contents {{ background-color: mix(@nexus_bg, @nexus_surface, 0.5); color: @nexus_fg; border-radius: 12px; padding: 4px; box-shadow: 0 0 0 1px @nexus_edge, 0 10px 24px -8px @nexus_shadow; }}
.nexus popover > arrow {{ background: transparent; border: none; }}
.nexus popover listview {{ background: transparent; }}
.nexus popover listview > row {{ border-radius: 8px; padding: 6px 10px; }}
.nexus popover listview > row:hover {{ background: alpha(@nexus_fg, 0.08); }}
.nexus popover listview > row:selected {{ background: alpha(@nexus_fg, 0.13); color: @nexus_fg; }}
tooltip.background {{ background: mix(@nexus_bg, @nexus_surface, 0.5); color: @nexus_fg; border-radius: 8px; padding: 0; box-shadow: 0 0 0 1px @nexus_edge; }}
tooltip > box {{ padding: 4px 8px; }}

.nexus entry {{ background: alpha(@nexus_fg, 0.07); color: @nexus_fg; caret-color: @nexus_accent; border: none; border-radius: 8px; padding: 6px 10px; box-shadow: none; outline: none; }}
.nexus entry:focus-within {{ outline: 2px solid alpha(@nexus_accent, 0.8); outline-offset: -2px; }}
.nexus scrollbar {{ background: transparent; border: none; }}
.nexus scrollbar slider {{ background: alpha(@nexus_fg, 0.18); border: none; border-radius: 999px; min-width: 5px; min-height: 28px; }}
.nexus separator {{ background: alpha(@nexus_fg, 0.1); }}

window.shell-overlay, window.clipboard-layer, window.prompt-window, window.osd, window.drawer, window.notification-popups, window.screenshot {{ background: transparent; }}
.nexus .clipboard-card {{ color: @nexus_fg; border-radius: 16px; padding: 12px; }}
.nexus .clipboard-list {{ background: transparent; }}
.nexus .clipboard-list row {{ border-radius: 8px; padding: 6px 8px; margin: 1px 0; }}
.nexus .clipboard-list row:hover, .nexus .clipboard-list row:focus {{ background: alpha(@nexus_fg, 0.08); }}
.nexus .clipboard-list row:selected {{ background: alpha(@nexus_fg, 0.13); color: @nexus_fg; }}
.nexus .clipboard-row image.accent {{ color: @nexus_accent_fg; }}
window.power-overlay {{ background: alpha(#05060a, 0.5); transition: background-color 100ms ease; }}
window.power-overlay.leaving {{ background: transparent; }}
.click-away-seed {{ min-width: 1px; min-height: 1px; background: rgba(0, 0, 0, 0.004); }}
.shell-panel {{ border-radius: 16px; }}
.power-overlay .shell-panel {{ background: transparent; background-image: none; box-shadow: none; backdrop-filter: none; }}
.nexus .power-page {{ padding: 24px; }}
.nexus .power-page .card {{ background: @nexus_bg; }}
.nexus button.power-tile {{ min-width: 112px; padding: 16px 12px 12px; font-size: 14px; font-weight: 500; border-radius: 16px; }}
.nexus button.power-tile:hover {{ background-color: mix(@nexus_glass, @nexus_fg, 0.12); }}
.nexus button.power-tile.selected {{ background-color: mix(@nexus_glass, @nexus_fg, 0.2); }}
.nexus button.power-tile.danger {{ color: @nexus_danger; }}
.nexus button.power-tile.danger.selected {{ background-color: @nexus_error; color: #ffffff; }}
{power_entrances}
/* The selection lets the screen through; its spread shadow dims everything else. */
.screenshot-selection {{ border: 2px solid @nexus_accent; border-radius: 0; box-shadow: 0 0 0 10000px alpha(#05060a, 0.48); }}
.nexus .prompt {{ border-radius: 16px; padding: 20px; }}
.nexus .prompt .title {{ font-size: 18px; }}

/* Notifications: separate cards. */
.notification-popups .popup-panel {{ padding: 2px 0; }}
.notification-popups .notification {{ padding: 10px 12px 12px; margin: 4px 8px 10px 8px; border-radius: 16px; box-shadow: {surface_shadow_small}; }}
.drawer-panel {{ border-radius: 16px; }}
.notification-center {{ padding: 12px; }}
.notification-center .center-title {{ font-size: 15px; font-weight: 650; }}
.notification-center .notification {{ background: {tile}; border-radius: 12px; padding: 8px 10px 10px; }}
.notification-center .empty-state {{ padding: 18px 0 14px; }}
.notification-center .muted, .notification-center .dim-label, .notification-center .notification .app-name {{ color: {center_muted}; }}
.notification-center .center-calendar {{ background: {raised}; border-radius: 12px; padding: 6px 6px 6px; }}
.notification-center .calendar-head {{ padding: 0 2px 2px 8px; }}
.notification-center .calendar-month {{ font-size: 13px; font-weight: 600; }}
.nexus button.calendar-step {{ min-width: 28px; min-height: 28px; padding: 0; background: transparent; color: {center_muted}; }}
.nexus button.calendar-step:hover {{ background: alpha(@nexus_fg, 0.08); color: @nexus_fg; }}
.notification-center calendar {{ background: transparent; color: @nexus_fg; border: none; padding: 0; font-size: 12px; font-feature-settings: "tnum"; }}
.notification-center calendar > grid {{ padding: 0; }}
.notification-center calendar > grid > label {{ min-width: 24px; min-height: 24px; margin: 0; padding: 0; border-radius: 999px; }}
.notification-center calendar > grid > label:selected {{ background: none; color: @nexus_fg; }}
.notification-center calendar > grid > label.day-name {{ color: {center_muted}; font-size: 11px; font-weight: 600; }}
.notification-center calendar > grid > label.week-number {{ color: transparent; }}
.notification-center calendar > grid > label.other-month {{ color: alpha(@nexus_fg, 0.45); }}
.notification-center calendar > grid > label.today, .notification-center calendar > grid > label.today:selected {{ background: radial-gradient(circle closest-side, @nexus_accent 92%, transparent 100%); color: @nexus_on_accent; font-weight: 600; }}
.nexus .notification .app-name {{ font-size: 12px; font-weight: 600; color: @nexus_muted; }}
.nexus .notification .summary {{ font-weight: 600; }}
.nexus .notification .body {{ color: alpha(@nexus_fg, 0.84); }}
.nexus .notification.critical .summary, .nexus .notification.critical .app-name {{ color: @nexus_danger; }}
.notification-popups .notification.critical {{ background-color: mix(@nexus_glass, @nexus_error, 0.1); }}
.notification-center .notification.critical {{ background-color: alpha(@nexus_error, 0.1); }}
.nexus .notification label link {{ color: @nexus_accent_fg; }}
.nexus .notification-image {{ border-radius: 8px; }}
.nexus button.notification-close {{ min-width: 24px; min-height: 24px; padding: 0; border-radius: 8px; background: transparent; color: @nexus_muted; }}
.nexus button.notification-close:hover {{ background: alpha(@nexus_fg, 0.1); color: @nexus_fg; }}
.nexus .notification .actions button {{ padding: 5px 10px; font-size: 13px; }}
.nexus .notification progressbar > trough {{ background: alpha(@nexus_fg, 0.14); border: none; border-radius: 999px; min-height: 6px; }}
.nexus .notification progressbar > trough > progress {{ background: @nexus_accent; border: none; border-radius: 999px; min-width: 0; min-height: 6px; margin: 0; }}

/* The lock: the blurred desktop under a scrim of the theme's own background,
   strong enough for text on a pure white or black capture to read. What needs
   more contrast than that (fields, messages) is drawn solid. */
window.lock-screen {{ background: #000000; color: @nexus_fg; }}
.lock-screen .lock-shot {{ animation: lock-zoom 200ms cubic-bezier(0.22, 1, 0.36, 1) both; }}
.lock-screen .lock-shot.leaving {{ animation: lock-unzoom 140ms cubic-bezier(0.7, 0, 0.84, 0) both; }}
.lock-screen .lock-blurred {{ animation: lock-fade-in 140ms cubic-bezier(0.22, 1, 0.36, 1) both; }}
.lock-screen .lock-shot.leaving .lock-blurred {{ animation: lock-fade-out 140ms cubic-bezier(0.7, 0, 0.84, 0) both; }}
.lock-screen .lock-scrim {{ background: alpha(@nexus_bg, {lock_scrim}); }}
window.lock-screen.panel .lock-scrim {{ background: alpha(@nexus_bg, 0.2); }}
.lock-screen .lock-ui {{ animation: lock-fade-in 140ms cubic-bezier(0.2, 0.9, 0.25, 1) both; }}
.lock-screen .lock-ui.leaving {{ animation: lock-fade-out 120ms cubic-bezier(0.7, 0, 0.84, 0) both; }}
.lock-screen .lock-time {{ font-feature-settings: "tnum"; }}
.lock-screen .lock-date {{ font-weight: 450; }}
.lock-screen .lock-name {{ font-weight: 600; font-size: 16px; }}
.lock-screen .lock-identity {{ margin-bottom: 16px; }}
.lock-screen .lock-avatar {{ border-radius: 999px; background: @nexus_accent; color: @nexus_on_accent; font-weight: 600; font-size: 17px; }}
.lock-screen .lock-field {{ border-radius: 12px; padding: 5px; background: @nexus_surface; color: @nexus_fg; box-shadow: 0 1px 3px alpha(black, 0.18); transition: box-shadow 80ms ease; }}
.lock-screen .lock-field:focus-within {{ box-shadow: 0 0 0 2px @nexus_accent; }}
.lock-screen .lock-field.error, .lock-screen .lock-field.error:focus-within {{ box-shadow: 0 0 0 2px @nexus_error; }}
.lock-screen .lock-field.shake-a {{ animation: field-shake-a 300ms ease-out; }}
.lock-screen .lock-field.shake-b {{ animation: field-shake-b 300ms ease-out; }}
.lock-screen entry.lock-password, .lock-screen entry.lock-password:focus-within {{ background: transparent; border: none; box-shadow: none; outline: none; min-height: 38px; color: @nexus_fg; caret-color: @nexus_accent; padding: 0 10px; font-size: 16px; }}
.lock-screen .lock-password image {{ color: @nexus_muted; }}
.lock-screen button.lock-submit {{ min-width: 38px; min-height: 38px; padding: 0; border-radius: 8px; background: @nexus_accent; color: @nexus_on_accent; }}
.lock-screen button.lock-submit:hover {{ background: mix(@nexus_accent, @nexus_fg, 0.12); }}
.lock-screen button.lock-submit:disabled {{ background: alpha(@nexus_fg, 0.08); color: @nexus_muted; }}
.lock-screen .lock-notes {{ margin-top: 8px; min-height: 34px; }}
.lock-screen .lock-message, .lock-screen .lock-caps {{ font-size: 13px; font-weight: 550; color: @nexus_fg; }}
.lock-screen .poster-side .lock-message, .lock-screen .poster-side .lock-caps {{ padding: 6px 10px; border-radius: 8px; background: @nexus_bg; }}
.lock-screen .lock-preview {{ margin-top: 16px; padding: 6px 12px; border-radius: 8px; font-size: 13px; font-weight: 550; background: @nexus_bg; color: @nexus_fg; }}
.lock-screen .lock-message.error {{ color: @nexus_error; }}
.lock-screen .lock-message.warning {{ color: @nexus_error; }}
.lock-screen .lock-status {{ font-size: 13px; font-weight: 500; font-feature-settings: "tnum"; }}
.lock-screen button.lock-power {{ min-height: 38px; padding: 0 12px; font-size: 13px; font-weight: 500; background: alpha(@nexus_fg, 0.1); color: @nexus_fg; }}
.lock-screen button.lock-power:hover {{ background: alpha(@nexus_fg, 0.18); }}
.lock-screen button.lock-power:active {{ background: alpha(@nexus_fg, 0.24); }}
.lock-screen button:focus-visible {{ box-shadow: 0 0 0 2px @nexus_accent; }}
.lock-screen .lock-question {{ font-size: 13px; font-weight: 550; }}
.lock-screen button.lock-answer {{ min-height: 38px; padding: 0 14px; font-size: 13px; font-weight: 500; background: alpha(@nexus_fg, 0.1); color: @nexus_fg; }}
.lock-screen button.lock-answer:hover {{ background: alpha(@nexus_fg, 0.18); }}
.lock-screen button.lock-answer.destructive-action {{ background: @nexus_error; color: #ffffff; }}
.lock-screen button.lock-answer.destructive-action:hover {{ background: mix(@nexus_error, #000000, 0.12); }}
/* Side panel: one solid column. */
.lock-screen .lock-column {{ min-width: 356px; padding: 44px 36px 32px; background: @nexus_bg; box-shadow: 12px 0 40px -16px @nexus_shadow; }}
.lock-screen .lock-column .lock-time {{ font-size: 88px; font-weight: 300; letter-spacing: -3px; }}
.lock-screen .lock-column .lock-date {{ font-size: 17px; color: @nexus_muted; margin-top: 2px; }}
.lock-screen .lock-column .lock-avatar {{ font-size: 24px; }}
.lock-screen .lock-column .lock-name {{ font-size: 19px; }}
.lock-screen .lock-column .lock-field {{ background: mix(@nexus_bg, @nexus_fg, 0.07); }}
.lock-screen .lock-column .lock-status {{ color: @nexus_muted; margin-bottom: 14px; }}
.lock-screen .lock-column .lock-confirm-row {{ margin-top: 0; }}
/* Large clock: hours over minutes, sized from the monitor in code. */
.lock-screen .lock-poster {{ margin: 44px 56px; }}
.lock-screen .poster-time {{ line-height: 0.62; font-weight: 300; }}
.lock-screen .lock-poster .lock-date {{ font-size: 30px; margin-top: 18px; }}
.lock-screen .poster-side {{ min-width: 344px; }}
.lock-screen .poster-side .lock-power-row, .lock-screen .poster-side .lock-confirm-row {{ margin-top: 10px; }}
.lock-screen.compact .lock-poster {{ margin: 28px 32px; }}
.lock-screen.compact .lock-poster .lock-date {{ font-size: 20px; margin-top: 8px; }}
.lock-screen.compact .poster-side {{ min-width: 300px; }}
/* Bottom bar: the time on the backdrop, the rest on one strip. */
.lock-screen .bar-above {{ margin: 0 40px 28px; }}
.lock-screen .bar-above .lock-time {{ font-size: 136px; font-weight: 300; letter-spacing: -5px; }}
.lock-screen .bar-above .lock-date {{ font-size: 24px; margin-left: 4px; }}
.lock-screen .lock-strip {{ padding: 16px 40px 8px; background: @nexus_bg; box-shadow: 0 -12px 40px -16px @nexus_shadow; }}
.lock-screen .lock-strip .lock-identity {{ margin: 2px 20px 0 0; }}
.lock-screen .lock-strip .lock-field {{ min-width: 400px; background: mix(@nexus_bg, @nexus_fg, 0.07); }}
.lock-screen .lock-strip .lock-confirm-row {{ margin-left: 8px; }}
.lock-screen .lock-strip .lock-question {{ margin-right: 16px; }}
.lock-screen .lock-strip .lock-status {{ color: @nexus_muted; margin-left: 16px; }}
.lock-screen .lock-strip .lock-notes {{ margin-top: 6px; min-height: 20px; }}
.lock-screen.compact .bar-above {{ margin: 0 24px 16px; }}
.lock-screen.compact .bar-above .lock-time {{ font-size: 96px; letter-spacing: -3px; }}
.lock-screen.compact .lock-strip {{ padding: 12px 24px 6px; }}
.lock-screen.compact .lock-strip .lock-field {{ min-width: 300px; }}
@keyframes lock-zoom {{ from {{ transform: scale(1); }} to {{ transform: scale(1.05); }} }}
@keyframes lock-unzoom {{ from {{ transform: scale(1.05); }} to {{ transform: scale(1); }} }}
@keyframes lock-fade-in {{ from {{ opacity: 0; }} to {{ opacity: 1; }} }}
@keyframes lock-fade-out {{ from {{ opacity: 1; }} to {{ opacity: 0; }} }}
@keyframes field-shake-a {{ 0%, 100% {{ transform: none; }} 20% {{ transform: translateX(-10px); }} 45% {{ transform: translateX(8px); }} 70% {{ transform: translateX(-4px); }} }}
@keyframes field-shake-b {{ 0%, 100% {{ transform: none; }} 20% {{ transform: translateX(-10px); }} 45% {{ transform: translateX(8px); }} 70% {{ transform: translateX(-4px); }} }}

/* The menu bar: along the top of the screen rather than a slab floating on it. */
window.nexus.nexus-bar {{ background: transparent; }}
.nexus-bar .bar-body {{ background: transparent; min-height: 32px; }}
.nexus-bar .bar-content {{ padding: 0 6px; background-color: @nexus_glass; backdrop-filter: {backdrop}; box-shadow: inset 0 -1px 0 @nexus_edge; }}
.nexus-bar .bar-island {{ min-height: 32px; }}
.nexus-bar button {{ padding: 0 8px; min-height: 24px; min-width: 16px; margin: 0; font-size: 13px; background: transparent; border-radius: 8px; }}
.nexus-bar .bar-island > button, .nexus-bar .bar-island > box > button {{ margin-top: 4px; margin-bottom: 4px; }}
.nexus-bar button:hover {{ background: alpha(@nexus_fg, 0.12); }}
.nexus-bar button:active {{ background: alpha(@nexus_fg, 0.18); }}
.nexus-bar .bar-brand {{ color: @nexus_fg; }}
.nexus-bar .bar-app {{ font-weight: 650; font-size: 13px; margin: 0 10px 0 4px; }}
.nexus-bar button.bar-phone.away image {{ opacity: 0.45; }}
.nexus-bar button.bar-phone.low label {{ color: @nexus_danger; }}
.nexus-bar .bar-value {{ font-size: 12px; font-weight: 500; font-feature-settings: "tnum"; }}
.nexus-bar .bar-clock {{ font-size: 13px; font-weight: 550; font-feature-settings: "tnum"; }}
.nexus-bar button.error {{ color: @nexus_danger; }}
.nexus-bar button.bar-recording, .nexus-bar button.bar-recording label {{ color: #ffffff; }}
.nexus-bar button.bar-recording {{ background: @nexus_error; }}
.nexus-bar button.workspace {{ color: @nexus_muted; padding: 0; margin: 0; min-width: 26px; min-height: 24px; font-size: 12px; font-weight: 600; font-feature-settings: "tnum"; background: transparent; }}
.nexus-bar button.workspace:hover {{ color: @nexus_fg; background: transparent; }}
.nexus-bar button.workspace.active {{ color: @nexus_fg; }}
.nexus-bar .workspace-pill {{ background: alpha(@nexus_fg, 0.14); border-radius: 8px; }}

/* The dock. While it is down the window has nothing to draw, and a surface that
   draws nothing is never mapped: this much is invisible but counts. */
window.dock {{ background: rgba(0, 0, 0, 0.005); }}
.dock-plate {{ border-radius: 16px; }}
.dock button.dock-tile {{ background: transparent; padding: 0; margin: 0; border-radius: 12px; transition: none; }}
.dock button.dock-tile:hover, .dock button.dock-tile:active {{ background: transparent; }}
.dock button.dock-tile:focus-visible {{ outline: none; }}
.dock .dock-letter {{ background: mix(@nexus_bg, @nexus_fg, 0.22); color: @nexus_fg; border-radius: 14px; font-size: 26px; font-weight: 600; min-width: 50px; min-height: 50px; }}
.dock .dock-dot {{ background: alpha(@nexus_fg, 0.5); border-radius: 999px; min-width: 4px; min-height: 4px; }}
.dock .dock-dot.focused {{ background: @nexus_fg; }}
.dock .dock-divider {{ background: alpha(@nexus_fg, 0.2); min-width: 1px; }}

/* Volume and brightness as they change. */
.osd-card {{ border-radius: 16px; padding: 10px 14px; color: @nexus_fg; }}
.osd-card progressbar > trough {{ background: alpha(@nexus_fg, 0.18); border: none; border-radius: 999px; min-height: 6px; }}
.osd-card progressbar > trough > progress {{ background: @nexus_fg; border: none; border-radius: 999px; min-height: 6px; min-width: 0; margin: 0; }}
.osd-card .osd-value {{ font-size: 12px; font-weight: 600; min-width: 28px; font-feature-settings: "tnum"; }}

/* The system menu and Control Center, under the bar. */
.system-menu {{ padding: 6px; }}
.system-menu button {{ background: transparent; padding: 7px 10px; }}
.system-menu button:hover {{ background: alpha(@nexus_fg, 0.1); }}
.system-menu separator {{ margin: 5px 8px; }}
.quick-panel {{ padding: 10px; }}
.quick-head {{ padding: 0 2px 2px 0; min-height: 30px; }}
.quick-tile {{ background: {tile}; border-radius: 12px; padding: 7px; }}
.nexus button.quick-toggle {{ min-width: 34px; min-height: 34px; padding: 0; border-radius: 999px; background: alpha(@nexus_fg, 0.1); color: @nexus_fg; }}
.nexus button.quick-toggle:hover {{ background: alpha(@nexus_fg, 0.18); }}
.nexus button.quick-toggle.on {{ background: @nexus_accent; color: @nexus_on_accent; }}
.nexus button.quick-toggle.on:hover {{ background: mix(@nexus_accent, @nexus_fg, 0.12); }}
.nexus button.quick-label {{ background: transparent; padding: 0 2px; }}
.nexus button.quick-label:hover {{ background: transparent; }}
.quick-row {{ border-radius: 8px; }}
.quick-row.on {{ background: alpha(@nexus_accent, 0.16); }}
.nexus button.quick-entry {{ background: transparent; padding: 7px 8px; font-size: 13px; font-weight: 500; }}
.nexus button.quick-entry:hover {{ background: alpha(@nexus_fg, 0.08); }}
.nexus button.quick-entry:disabled {{ opacity: 1; }}
.quick-row.on button.quick-entry {{ font-weight: 600; }}
.quick-row.on button.quick-entry image {{ color: @nexus_accent_fg; }}
.nexus button.quick-more {{ min-width: 30px; min-height: 30px; padding: 0; background: transparent; color: @nexus_muted; }}
.nexus button.quick-more:hover {{ background: alpha(@nexus_fg, 0.08); color: @nexus_fg; }}
.quick-unfolded {{ padding: 4px 8px 10px 34px; }}
.quick-unfolded button {{ padding: 5px 10px; font-size: 13px; }}
.quick-heading {{ font-size: 12px; font-weight: 600; color: @nexus_muted; padding: 12px 8px 4px; }}
.quick-panel .error {{ font-size: 12px; padding: 4px 8px; }}
.quick-panel .quick-title {{ font-size: 13px; font-weight: 600; }}
.quick-detail {{ font-size: 11px; color: @nexus_muted; }}
button.quick-entry .quick-detail {{ font-weight: 400; font-size: 12px; }}
.quick-empty {{ padding: 18px 8px; font-size: 12px; }}
.quick-slider {{ background: {tile}; border-radius: 12px; padding: 4px 0 4px 6px; }}
.quick-slider button.quick-more image {{ -gtk-icon-size: 12px; }}
.quick-slider scale {{ padding: 4px 0; }}
.quick-card {{ background: {tile}; border-radius: 12px; padding: 8px 10px; margin-bottom: 6px; }}
.quick-card scale {{ padding: 4px 0; }}
.nexus button.quick-mute {{ background: transparent; padding: 0; min-width: 30px; min-height: 30px; }}
.nexus button.quick-mute:hover {{ background: alpha(@nexus_fg, 0.08); }}
.nexus button.quick-mute.muted-now {{ color: @nexus_danger; }}
.nexus button.quick-action {{ min-height: 40px; padding: 0; border-radius: 12px; background: {tile}; }}
.nexus button.quick-action:hover {{ background: alpha(@nexus_fg, 0.13); }}
.quick-stream {{ padding: 4px 8px 0; }}
.quick-stream .row-title {{ font-size: 13px; }}
.quick-stream scale {{ padding: 2px 0 4px; }}

.monitor-page {{ padding: 0; }}
.monitor .monitor-number, .monitor .monitor-stat, .monitor .monitor-nav-value {{ font-feature-settings: "tnum"; }}
.monitor .monitor-graph {{ color: @nexus_accent_fg; }}
.monitor .monitor-nav {{ background: transparent; }}
.monitor .monitor-nav > row {{ padding: 8px 10px 6px; border-radius: 10px; margin-bottom: 2px; }}
.monitor .monitor-nav > row:hover {{ background: alpha(@nexus_fg, 0.05); }}
.monitor .monitor-nav > row:selected {{ background: {raised}; color: @nexus_fg; }}
.monitor .monitor-nav-name {{ font-size: 13px; font-weight: 600; }}
.monitor .monitor-nav-value {{ font-size: 12px; color: @nexus_muted; }}
.monitor .monitor-detail {{ padding: 0; }}
.monitor .monitor-pane {{ padding: 14px 16px 16px; }}
.monitor .monitor-title {{ font-size: 15px; font-weight: 650; }}
.monitor .monitor-section {{ font-size: 13px; font-weight: 600; }}
.monitor .monitor-stat {{ font-size: 14px; font-weight: 600; }}
.monitor .monitor-line-name {{ font-size: 13px; }}
.monitor .monitor-swatch {{ min-width: 8px; min-height: 8px; border-radius: 2px; }}
.monitor .monitor-swatch.used {{ background: @nexus_accent_fg; }}
.monitor .monitor-swatch.cache {{ background: alpha(@nexus_accent_fg, 0.4); }}
.monitor .monitor-swatch.free {{ background: alpha(@nexus_accent_fg, 0.12); }}
.monitor .monitor-value {{ font-size: 20px; font-weight: 650; font-feature-settings: "tnum"; }}
.monitor .monitor-bar > trough {{ background: alpha(@nexus_fg, 0.14); border: none; border-radius: 999px; min-height: 6px; }}
.monitor .monitor-bar > trough > progress {{ background: @nexus_accent; border: none; border-radius: 999px; min-height: 6px; margin: 0; min-width: 0; }}
.monitor-page .monitor-bar > trough, .monitor-page .monitor-bar > trough > progress {{ min-height: 4px; }}
.monitor-page .monitor-bar > trough > progress {{ background: @nexus_accent_fg; }}
.monitor-page .monitor-bar.hot > trough > progress {{ background: @nexus_error; }}
.monitor .monitor-search {{ padding: 4px 10px; min-height: 0; }}
.monitor button.monitor-tree, .monitor button.monitor-end {{ padding: 4px 12px; min-height: 0; font-size: 13px; }}
.monitor button.monitor-tree:checked {{ background: alpha(@nexus_accent, 0.18); color: @nexus_accent_fg; }}
.monitor .monitor-footer {{ border-top: 1px solid alpha(@nexus_fg, 0.08); padding-top: 8px; }}
.monitor .monitor-question {{ font-size: 13px; font-weight: 600; }}
.monitor .monitor-columns button {{ padding: 4px 0; min-height: 0; font-size: 12px; font-weight: 600; color: @nexus_muted; background: transparent; border: none; outline: none; box-shadow: none; }}
.monitor .monitor-columns button:hover {{ color: @nexus_fg; background: alpha(@nexus_fg, 0.07); }}
.monitor .monitor-columns button.active {{ color: @nexus_accent_fg; background: alpha(@nexus_accent, 0.16); }}
.monitor .monitor-list {{ background: transparent; }}
.monitor .monitor-list row {{ padding: 4px 8px; border-radius: 6px; font-size: 13px; font-feature-settings: "tnum"; }}
.monitor .monitor-list row:hover {{ background: alpha(@nexus_fg, 0.06); }}
.monitor .monitor-list row:selected {{ background: alpha(@nexus_fg, 0.13); color: @nexus_fg; }}

/* Phone: one header for every state, then what that state needs. */
.phone-page .phone-avatar {{ min-width: 48px; min-height: 48px; border-radius: 12px; background: alpha(@nexus_fg, 0.07); color: @nexus_muted; }}
.phone-page .phone-avatar.on {{ background: alpha(@nexus_accent, 0.16); color: @nexus_accent_fg; }}
.nexus .status-dot {{ min-width: 8px; min-height: 8px; border-radius: 999px; background: @nexus_muted; }}
.nexus .status-dot.on {{ background: @nexus_accent_fg; }}
.nexus .status-dot.bad {{ background: @nexus_error; }}
.phone-page .phone-notice .notice-icon {{ color: @nexus_muted; margin-top: 1px; }}
.phone-page .phone-notice.problem {{ background: alpha(@nexus_error, 0.1); }}
.phone-page .phone-notice.problem .notice-icon {{ color: @nexus_error; }}
.phone-page .phone-pair {{ padding: 20px; }}
.phone-page .qr-frame {{ border-radius: 12px; background: #ffffff; }}
.phone-page .step-number {{ min-width: 22px; min-height: 22px; border-radius: 999px; background: alpha(@nexus_fg, 0.08); font-size: 12px; font-weight: 650; font-feature-settings: "tnum"; }}
.phone-page .pair-manual {{ border-top: 1px solid alpha(@nexus_fg, 0.08); padding-top: 14px; margin-top: 4px; }}
.phone-page .phone-stat .monitor-value {{ font-size: 18px; }}
.phone-page .phone-stat progressbar, .phone-page .phone-stat progressbar > trough {{ min-width: 40px; }}
.monitor .monitor-bar.low > trough > progress {{ background: @nexus_error; }}
.nexus .phone-list > row {{ padding: 0 4px; border-radius: 0; border-bottom: 1px solid alpha(@nexus_fg, 0.06); }}
/* Rows fill their card edge to edge: the outer ones take its corners, so a hover follows its shape. */
.nexus .phone-list > row:first-child {{ border-top-left-radius: 12px; border-top-right-radius: 12px; }}
.nexus .phone-list > row:last-child {{ border-bottom-left-radius: 12px; border-bottom-right-radius: 12px; }}
.nexus .phone-list > row:last-child {{ border-bottom: none; }}
.nexus .phone-list > row:hover {{ background: alpha(@nexus_fg, 0.04); }}
.nexus .notification-from {{ opacity: 0.6; }}

/* Messages: conversations in a quiet column, the open one beside them. */
window.messages .messages-side {{ background: mix(@nexus_bg, @nexus_fg, 0.025); border-right: 1px solid alpha(@nexus_fg, 0.06); }}
window.messages .messages-top {{ padding: 18px 14px 12px 20px; }}
window.messages .threads {{ background: transparent; padding: 0 8px 8px; }}
window.messages .threads > row {{ border-radius: 10px; padding: 8px 10px; margin: 1px 0; }}
window.messages .threads > row:hover {{ background: alpha(@nexus_fg, 0.05); }}
window.messages .threads > row:selected {{ background: alpha(@nexus_fg, 0.1); color: @nexus_fg; }}
window.messages .avatar {{ border-radius: 999px; background: alpha(@nexus_accent, 0.18); color: @nexus_accent_fg; font-weight: 650; font-size: 13px; }}
window.messages .conversation-head {{ min-height: 40px; padding: 12px 24px; border-bottom: 1px solid alpha(@nexus_fg, 0.06); }}
window.messages .conversation-to {{ margin: 12px 24px 0; }}
window.messages .bubbles {{ padding: 8px 24px 16px; }}
window.messages .bubble {{ padding: 8px 13px; border-radius: 18px; background: {tile}; font-size: 14.5px; }}
window.messages .bubble.out {{ background: @nexus_accent; color: @nexus_on_accent; }}
window.messages .bubble.out selection {{ background: alpha(@nexus_on_accent, 0.3); color: @nexus_on_accent; }}
window.messages .bubble-time {{ margin: 22px 0 0; font-weight: 500; }}
window.messages .bubble-note {{ font-size: 11.5px; margin: 1px 6px 0; }}
window.messages .failed .bubble, window.messages .failed .voice, window.messages .failed fixed, window.messages .failed .attachment-file {{ opacity: 0.6; }}
window.messages button.note-action {{ padding: 2px 8px; font-size: 12px; }}
/* What messages carry: pictures, voice, files. */
window.messages .attachment-image {{ border-radius: 14px; background: alpha(@nexus_fg, 0.06); }}
window.messages .attachment-loading {{ padding: 8px 2px; }}
window.messages button.attachment-file {{ padding: 8px 14px 8px 12px; border-radius: 14px; background: {tile}; }}
window.messages button.attachment-file:hover {{ background: mix(@nexus_bg, @nexus_fg, 0.14); }}
window.messages button.attachment-file.out {{ background: @nexus_accent; color: @nexus_on_accent; }}
window.messages button.attachment-file.out .muted, window.messages button.attachment-file.out .row-title {{ color: @nexus_on_accent; }}
window.messages .voice {{ min-width: 240px; padding: 6px 14px 6px 6px; border-radius: 22px; background: {tile}; }}
window.messages .voice.out {{ background: @nexus_accent; color: @nexus_on_accent; }}
window.messages .voice button.voice-play {{ min-width: 32px; min-height: 32px; padding: 0; border-radius: 999px; background: alpha(@nexus_fg, 0.1); }}
window.messages .voice.out button.voice-play {{ background: alpha(@nexus_on_accent, 0.2); color: @nexus_on_accent; }}
window.messages .voice-bar > trough {{ min-height: 4px; border-radius: 999px; background: alpha(@nexus_fg, 0.16); border: none; }}
window.messages .voice-bar > trough > progress {{ min-height: 4px; border-radius: 999px; background: @nexus_fg; border: none; }}
window.messages .voice.out .voice-bar > trough {{ background: alpha(@nexus_on_accent, 0.3); }}
window.messages .voice.out .voice-bar > trough > progress {{ background: @nexus_on_accent; }}
window.messages .voice-time {{ font-size: 12px; font-feature-settings: "tnum"; min-width: 30px; }}
/* Dropping files: the conversation says it takes them. */
window.messages .conversation.drop .bubbles-scroll {{ background: alpha(@nexus_accent, 0.07); box-shadow: inset 0 0 0 2px alpha(@nexus_accent, 0.55); border-radius: 12px; }}
window.messages .compose-status {{ padding: 0 24px 6px 72px; }}
/* The composer: round buttons either side of one field that grows with the text. */
window.messages .composer {{ padding: 6px 20px 18px; }}
window.messages .compose-field {{ background: alpha(@nexus_fg, 0.07); border-radius: 20px; min-height: 40px; }}
window.messages .compose-field:focus-within {{ box-shadow: inset 0 0 0 1px alpha(@nexus_accent, 0.5); }}
window.messages textview.compose-text, window.messages textview.compose-text > text {{ background: transparent; color: @nexus_fg; caret-color: @nexus_accent; font-size: 14.5px; }}
window.messages .compose-placeholder {{ color: @nexus_muted; font-size: 14.5px; }}
window.messages .compose-chips {{ padding: 10px 10px 2px; }}
window.messages .chip-image {{ border-radius: 12px; }}
window.messages button.chip-file {{ background: alpha(@nexus_fg, 0.08); border-radius: 12px; padding: 8px 30px 8px 10px; }}
window.messages button.chip-remove {{ min-width: 20px; min-height: 20px; padding: 0; margin: 4px; border-radius: 999px; background: alpha(#000000, 0.62); color: #ffffff; -gtk-icon-size: 12px; }}
window.messages button.chip-remove:hover {{ background: alpha(#000000, 0.8); }}
window.messages button.compose-round {{ min-width: 40px; min-height: 40px; padding: 0; border-radius: 999px; background: transparent; color: @nexus_muted; }}
window.messages button.compose-round:hover {{ background: alpha(@nexus_fg, 0.08); color: @nexus_fg; }}
window.messages button.compose-round.send {{ background: @nexus_accent; color: @nexus_on_accent; }}
window.messages button.compose-round.send:hover {{ background: mix(@nexus_accent, @nexus_fg, 0.12); }}
window.call entry.call-number {{ font-size: 24px; font-weight: 600; background: transparent; border: none; box-shadow: none; outline: none; }}
window.call button.compose-round {{ min-width: 56px; min-height: 56px; background: @nexus_accent; color: @nexus_on_accent; -gtk-icon-size: 22px; }}
window.call button.compose-round:hover {{ background: mix(@nexus_accent, @nexus_fg, 0.12); color: @nexus_on_accent; }}
window.call button.compose-round:disabled {{ opacity: 0.4; }}
window.call .error {{ color: @nexus_error; }}
window.call button.compose-round.hang-up {{ background: @nexus_error; color: #ffffff; }}
window.call button.compose-round.hang-up:hover {{ background: mix(@nexus_error, #000000, 0.12); }}
window.messages button.compose-round.hang-up {{ background: @nexus_error; color: #ffffff; }}
window.messages button.compose-round.hang-up:hover {{ background: mix(@nexus_error, #000000, 0.12); }}
window.messages .compose-recording {{ min-height: 40px; padding: 0 16px; border-radius: 20px; background: alpha(@nexus_error, 0.1); }}
@keyframes rec-pulse {{ 50% {{ opacity: 0.35; }} }}
window.messages .rec-dot {{ min-width: 10px; min-height: 10px; border-radius: 999px; background: @nexus_danger; animation: rec-pulse 1.2s ease-in-out infinite; }}
window.messages .rec-time {{ font-feature-settings: "tnum"; font-weight: 600; min-width: 36px; }}
window.messages levelbar.rec-level > trough {{ padding: 0; background: none; border: none; }}
window.messages levelbar.rec-level > trough > block {{ min-width: 3px; min-height: 14px; margin: 0 1px; border-radius: 999px; border: none; background: @nexus_danger; }}
window.messages levelbar.rec-level > trough > block.empty {{ background: alpha(@nexus_fg, 0.14); }}
window.messages .messages-state {{ padding: 32px; }}
.nexus .package-list {{ background: transparent; }}
.nexus .package-list row {{ background: transparent; padding: 0; }}
.nexus .package-row {{ padding: 9px 14px 9px 2px; border-bottom: 1px solid alpha(@nexus_fg, 0.06); }}
.nexus .package-badge {{ font-size: 11px; font-weight: 600; color: @nexus_muted; background: alpha(@nexus_fg, 0.08); border-radius: 5px; padding: 0 6px; }}
.nexus .package-message {{ padding: 0 24px 48px; }}
.nexus .package-done {{ background: alpha(@nexus_accent, 0.14); border-radius: 8px; padding: 8px 12px; }}
.nexus .package-sheet {{ background: {tile}; border-radius: 12px; padding: 14px; }}
.nexus button.flat.package-details, .nexus button.flat.package-details:hover {{ background: transparent; padding: 4px 0; }}
.nexus button.flat.package-details:focus-visible {{ outline-offset: 4px; }}
.nexus .package-log-frame {{ background: {raised}; border-radius: 12px; }}
.nexus .package-log {{ background: transparent; font-family: 'Geist Mono', monospace; font-size: 12px; color: @nexus_fg; }}
.nexus .package-log text {{ background: transparent; color: @nexus_fg; }}
.nexus progressbar.package-progress > trough {{ background: alpha(@nexus_fg, 0.14); border: none; border-radius: 999px; min-height: 6px; }}
.nexus progressbar.package-progress > trough > progress {{ background: @nexus_accent; border: none; border-radius: 999px; min-height: 6px; min-width: 0; margin: 0; }}
.tray-panel {{ padding: 10px; }}
.battery-panel {{ padding: 14px; }}
.battery-panel .power-profile {{ padding: 7px 10px; background: transparent; }}
.battery-panel .power-profile:hover {{ background: alpha(@nexus_fg, 0.08); }}
.battery-panel .power-profile.active {{ background: alpha(@nexus_accent, 0.16); color: @nexus_fg; }}
.battery-panel .power-profile .profile-check {{ opacity: 0; }}
.battery-panel .power-profile.active .profile-check {{ opacity: 1; color: @nexus_accent_fg; }}
.tray-item {{ padding: 8px 10px; }}
.tray-item.needs-attention {{ background: alpha(@nexus_fg, 0.1); color: @nexus_accent_fg; }}
.tray-menu button {{ background: transparent; padding: 7px 10px; }}
.tray-menu button:hover {{ background: alpha(@nexus_fg, 0.08); }}

/* The launcher: one large field, its results below. */
.launcher .search-row {{ padding: 14px 16px 14px 20px; color: @nexus_muted; }}
.nexus .launcher entry, .nexus .launcher entry:focus-within {{ background: transparent; border: none; outline: none; box-shadow: none; padding: 2px 2px 2px 2px; font-size: 20px; font-weight: 400; color: @nexus_fg; }}
.launcher separator {{ margin: 0; background: alpha(@nexus_fg, 0.08); }}
.launcher .app-results {{ background: transparent; padding: 6px; }}
.launcher .app-results row {{ border-radius: 10px; background: transparent; color: @nexus_fg; outline: none; }}
.nexus button.nav {{ transition: none; }}
.launcher .app-results row:selected {{ background: alpha(@nexus_fg, 0.12); color: @nexus_fg; }}
.launcher .app-row {{ padding: 6px 10px; min-height: 40px; }}
.launcher .app-name {{ font-size: 14px; font-weight: 600; }}
.launcher .app-description {{ font-size: 12px; color: @nexus_muted; }}
.launcher .app-results row:selected .app-description {{ color: alpha(@nexus_fg, 0.82); }}
.launcher .app-letter {{ background: mix(@nexus_bg, @nexus_fg, 0.22); color: @nexus_fg; border-radius: 8px; font-size: 16px; font-weight: 600; min-width: 32px; min-height: 32px; }}
.launcher .empty-results {{ padding: 20px; }}
.launcher row, .launcher entry, .launcher entry:focus-within {{ border: none; outline: none; }}
.launcher .error {{ padding: 0 20px 14px; }}
"#,
        accent = s.accent
    )
}
/// Entrances of the power tiles: the outer ones from their side, the middle one
/// from below. Each comes in two identical copies so that it can be replayed.
fn power_entrances() -> String {
    let mut css = String::new();
    for (side, axis, offset, delay) in [
        ("left", 'X', -16, 0),
        ("bottom", 'Y', 12, 0),
        ("right", 'X', 16, 0),
    ] {
        for copy in ["a", "b"] {
            css += &format!(
                "@keyframes power-{side}-{copy} {{ from {{ opacity: 0; transform: translate{axis}({offset}px); }} to {{ opacity: 1; transform: none; }} }}\n\
                 .nexus button.power-tile.from-{side}.enter-{copy} {{ animation: power-{side}-{copy} 140ms cubic-bezier(0.2, 0.9, 0.25, 1) {delay}ms both; }}\n"
            );
        }
        // Back the way they came, quicker, as the overlay fades.
        let away = offset * 2 / 3;
        css += &format!(
            "@keyframes power-{side}-out {{ from {{ opacity: 1; transform: none; }} to {{ opacity: 0; transform: translate{axis}({away}px); }} }}\n\
             window.power-overlay.leaving button.power-tile.from-{side} {{ animation: power-{side}-out 100ms cubic-bezier(0.55, 0, 1, 0.45) both; }}\n"
        );
    }
    css
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
    pkill_own(None, "mpvpaper");
    *old = Some(child);
    Ok(())
}
/// Signals the user's processes named `name`, terminating them by default.
pub(crate) fn pkill_own(signal: Option<&str>, name: &str) {
    let user = std::env::var("USER").unwrap_or_default();
    let _ = Command::new("pkill")
        .args(signal)
        .args(["-u", &user, "-x", name])
        .status();
}
/// Recolourable GTK theme the desktop theme builds on: every colour it draws
/// comes from libadwaita's named colours.
const GTK_BASE: &str = "adw-gtk3";
/// Generated GTK theme; `Nexus-dark` is its dark sibling.
const GTK_THEME: &str = "Nexus";
/// adw-gtk3 tints text selections with a translucent accent; Chromium flattens
/// it to a near-black selection on web pages. The selectors repeat adw-gtk3's
/// so that, coming after its import, they win at equal specificity.
const GTK_SELECTION: &str = "selection, selection:focus, entry selection, headerbar entry selection, .titlebar entry selection, \
.view text selection, .view text selection:focus, textview text selection, textview text selection:focus, \
iconview text selection, iconview text selection:focus { background-color: @accent_bg_color; color: @accent_fg_color; }\n";
const BLOCK_BEGIN: &str = "/* nexus:begin — generated by Nexus, replaced on theme changes */";
const BLOCK_END: &str = "/* nexus:end */";
/// Header of generated files, in the comment syntax of each.
pub(crate) const GENERATED: &str =
    "Generated by Nexus from its theme settings; changes are overwritten.";
fn gtk_theme_name(base: &str, dark: bool) -> String {
    if dark {
        format!("{base}-dark")
    } else {
        base.into()
    }
}
/// The Nexus palette under libadwaita's colour names, for desktop applications.
fn desktop_colors(accent: &str, dark: bool) -> Vec<(&'static str, String)> {
    let p = Palette::new(accent, dark);
    let (bg, surface, fg) = (p.bg, p.surface, p.fg);
    // Content views recede and chrome stands out: darker and lighter in dark mode,
    // white content on a tinted window in light mode.
    let (view, chrome, overlay) = if dark {
        (mix(bg, [0.; 3], 0.2), surface, mix(surface, fg, 0.05))
    } else {
        (surface, mix(bg, fg, 0.04), surface)
    };
    let mut colors = vec![
        ("accent_bg_color", accent.to_string()),
        ("accent_fg_color", p.on_accent.to_string()),
        ("accent_color", hex(p.accent_fg)),
        ("headerbar_backdrop_color", hex(bg)),
        ("sidebar_backdrop_color", hex(mix(chrome, bg, 0.5))),
    ];
    for (bg_name, fg_name, background) in [
        ("window_bg_color", "window_fg_color", bg),
        ("view_bg_color", "view_fg_color", view),
        ("headerbar_bg_color", "headerbar_fg_color", chrome),
        ("sidebar_bg_color", "sidebar_fg_color", chrome),
        (
            "secondary_sidebar_bg_color",
            "secondary_sidebar_fg_color",
            mix(chrome, bg, 0.5),
        ),
        ("card_bg_color", "card_fg_color", surface),
        ("dialog_bg_color", "dialog_fg_color", overlay),
        ("popover_bg_color", "popover_fg_color", overlay),
        ("thumbnail_bg_color", "thumbnail_fg_color", overlay),
    ] {
        colors.push((bg_name, hex(background)));
        colors.push((fg_name, hex(fg)));
    }
    colors
}
fn define_colors(colors: &[(&str, String)]) -> String {
    colors
        .iter()
        .map(|(name, value)| format!("@define-color {name} {value};\n"))
        .collect()
}
/// libadwaita 1.6+ reads CSS variables rather than named colours.
pub(crate) fn color_vars(colors: &[(&str, String)]) -> String {
    let vars: Vec<String> = colors
        .iter()
        .map(|(name, value)| format!("--{}: {value};", name.replace('_', "-")))
        .collect();
    format!(":root {{ {} }}\n", vars.join(" "))
}
pub(crate) fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local/share"))
}
/// Replaces `path` in one step, leaving it untouched when nothing changed so
/// that applications watching it do not reload.
pub(crate) fn write_atomic(path: &std::path::Path, contents: impl AsRef<[u8]>) -> Result<()> {
    let contents = contents.as_ref();
    if fs::read(path).is_ok_and(|old| old == contents) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_added_extension("tmp");
    fs::write(&tmp, contents)?;
    fs::rename(tmp, path)?;
    Ok(())
}
/// Replaces the Nexus block of a user stylesheet, keeping everything around it.
/// A new block goes at the end, or at the start when it holds an @import.
pub(crate) fn replace_block(text: &str, block: &str) -> String {
    let at_start = block.starts_with("@import");
    let block = format!("{BLOCK_BEGIN}\n{block}{BLOCK_END}\n");
    match (text.find(BLOCK_BEGIN), text.find(BLOCK_END)) {
        (Some(start), Some(end)) if start < end => {
            let rest = text[end + BLOCK_END.len()..].trim_start_matches('\n');
            format!("{}{block}{rest}", &text[..start])
        }
        _ if text.trim().is_empty() => block,
        _ if at_start => format!("{block}\n{text}"),
        _ => format!("{}\n\n{block}", text.trim_end()),
    }
}
/// Writes the Nexus GTK 3/4 themes on top of adw-gtk3, and the libadwaita
/// colours in the user's GTK 4 stylesheet. Chromium-based browsers using the
/// GTK theme follow the GTK 3 one. Returns false when adw-gtk3 is not installed.
fn write_desktop_theme(s: &Settings) -> Result<bool> {
    let (Some(light_base), Some(dark_base)) = (
        theme_dir(GTK_BASE),
        theme_dir(&gtk_theme_name(GTK_BASE, true)),
    ) else {
        return Ok(false);
    };
    let light = desktop_colors(&s.accent, false);
    let dark = desktop_colors(&s.accent, true);
    let themes = data_home().join("themes");
    let header = format!("/* {GENERATED} */\n");
    for dark_theme in [false, true] {
        let name = gtk_theme_name(GTK_THEME, dark_theme);
        let dir = themes.join(&name);
        let base = if dark_theme { &dark_base } else { &light_base };
        let mut files = vec![("gtk.css", if dark_theme { &dark } else { &light })];
        if !dark_theme {
            // Applications that request the dark variant of the light theme.
            files.push(("gtk-dark.css", &dark));
        }
        for version in ["gtk-3.0", "gtk-4.0"] {
            for (file, colors) in &files {
                let mut css = format!(
                    "{header}@import url(\"file://{}\");\n{}",
                    base.join(version).join(file).display(),
                    define_colors(colors)
                );
                css.push_str(GTK_SELECTION);
                if version == "gtk-4.0" {
                    css.push_str(&color_vars(colors));
                }
                write_atomic(&dir.join(version).join(file), &css)?;
            }
        }
        fs::write(
            dir.join("index.theme"),
            format!(
                "[Desktop Entry]\nType=X-GNOME-Metatheme\nName={name}\nComment=Nexus colours on {GTK_BASE}\n\n[X-GNOME-Metatheme]\nGtkTheme={name}\n"
            ),
        )?;
    }
    // libadwaita ignores GTK themes and only reads the user stylesheet.
    let user_css = config_home().join("gtk-4.0/gtk.css");
    let block = format!(
        "{}@media (prefers-color-scheme: dark) {{\n{}}}\n",
        color_vars(&light),
        color_vars(&dark)
    );
    let text = fs::read_to_string(&user_css).unwrap_or_default();
    write_atomic(&user_css, replace_block(&text, &block))?;
    Ok(true)
}
/// Writes the desktop and application themes for the current settings and
/// selects them. Running applications keep their colours: nothing has changed
/// since they started.
pub fn sync_desktop(s: &Settings) {
    match write_desktop_theme(s) {
        Ok(true) => apply_desktop_mode(s.dark(), true, false),
        Ok(false) => {}
        Err(e) => eprintln!("Could not write the desktop theme: {e}"),
    }
    write_apps(s);
}
fn write_apps(s: &Settings) {
    if let Err(e) = crate::apps::write(s) {
        eprintln!("Could not write the application colours: {e}");
    }
}
fn gsettings(args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("gsettings");
    cmd.args(args).stdin(Stdio::null()).stderr(Stdio::null());
    // A disabled bus address would make dconf drop the write; without one, GLib
    // finds the session bus the desktop's applications share.
    if std::env::var("DBUS_SESSION_BUS_ADDRESS").is_ok_and(|a| a.starts_with("disabled:")) {
        cmd.env_remove("DBUS_SESSION_BUS_ADDRESS");
    }
    let out = cmd.output().ok()?;
    out.status.success().then(|| {
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .trim_matches('\'')
            .to_string()
    })
}
fn theme_exists(name: &str) -> bool {
    theme_dir(name).is_some()
}
fn theme_dir(name: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let data =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    [home.join(".themes"), home.join(".local/share/themes")]
        .into_iter()
        .chain(data.split(':').map(|d| PathBuf::from(d).join("themes")))
        .map(|dir| dir.join(name))
        .find(|dir| dir.is_dir())
}
/// Light or dark sibling of a GTK theme, e.g. Arc ↔ Arc-Dark, Adwaita ↔ Adwaita-dark.
fn paired_theme(current: &str, dark: bool, exists: impl Fn(&str) -> bool) -> String {
    let base = current
        .strip_suffix("-Dark")
        .or_else(|| current.strip_suffix("-dark"))
        .unwrap_or(current);
    if !dark {
        return base.to_string();
    }
    if base == "Adwaita" {
        return "Adwaita-dark".into();
    }
    [format!("{base}-Dark"), format!("{base}-dark")]
        .into_iter()
        .find(|t| exists(t))
        .unwrap_or_else(|| current.to_string())
}
/// Sets `key=value` under [Settings], replacing an existing entry.
fn set_ini(text: &str, key: &str, value: &str) -> String {
    let line = format!("{key}={value}");
    let mut out: Vec<String> = text.lines().map(str::to_string).collect();
    if let Some(l) = out
        .iter_mut()
        .find(|l| l.split('=').next().is_some_and(|k| k.trim() == key))
    {
        *l = line;
    } else if let Some(i) = out.iter().position(|l| l.trim() == "[Settings]") {
        out.insert(i + 1, line);
    } else {
        out.insert(0, "[Settings]".into());
        out.insert(1, line);
    }
    out.join("\n") + "\n"
}
/// Switches the whole desktop: GSettings is relayed live by the settings portal to
/// GTK, libadwaita, Chromium, Firefox and Electron; settings.ini covers the rest.
/// `themed` selects the generated Nexus theme; `reload` makes running applications
/// pick up its new colours.
fn apply_desktop_mode(dark: bool, themed: bool, reload: bool) {
    let current = gsettings(&["get", "org.gnome.desktop.interface", "gtk-theme"])
        .unwrap_or_else(|| "Adwaita".into());
    let theme = if themed {
        gtk_theme_name(GTK_THEME, dark)
    } else {
        paired_theme(&current, dark, theme_exists)
    };
    let scheme = if dark { "prefer-dark" } else { "prefer-light" };
    gsettings(&["set", "org.gnome.desktop.interface", "color-scheme", scheme]);
    if reload && current == theme {
        // GTK only reloads a theme when its name changes: pass through the base.
        let base = gtk_theme_name(GTK_BASE, dark);
        gsettings(&["set", "org.gnome.desktop.interface", "gtk-theme", &base]);
    }
    gsettings(&["set", "org.gnome.desktop.interface", "gtk-theme", &theme]);
    let config = config_home();
    for version in ["gtk-3.0", "gtk-4.0"] {
        let path = config.join(version).join("settings.ini");
        let text = fs::read_to_string(&path).unwrap_or_default();
        let text = set_ini(&text, "gtk-theme-name", &theme);
        let text = set_ini(
            &text,
            "gtk-application-prefer-dark-theme",
            if dark { "1" } else { "0" },
        );
        if fs::create_dir_all(config.join(version)).is_ok() {
            let _ = fs::write(&path, text);
        }
    }
}
/// Under this, text stops being readable over a busy desktop.
pub const MIN_OPACITY: u8 = 60;
fn check(settings: &Settings) -> Result<()> {
    if !["dark", "light"].contains(&settings.mode.as_str()) {
        bail!("Unknown theme mode");
    }
    if !STYLES.iter().any(|style| style.id == settings.style) {
        bail!("Unknown style");
    }
    if !LOCK_STYLES.iter().any(|l| l.0 == settings.lock_style) {
        bail!("Unknown lock screen style");
    }
    if settings.accent.len() != 7
        || !settings.accent.starts_with('#')
        || !settings.accent[1..].chars().all(|c| c.is_ascii_hexdigit())
    {
        bail!("Invalid accent color");
    }
    Ok(())
}
pub fn save(mut settings: Settings) -> Result<Settings> {
    settings.opacity = settings.opacity.map(|v| v.clamp(MIN_OPACITY, 100));
    check(&settings)?;
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
    // Turned off without picking one of the accents: back to the default.
    if !settings.wallpaper_colors && old.wallpaper_colors && settings.accent == old.accent {
        settings.accent = Settings::default().accent;
    }
    write_theme(&settings)?;
    write_atomic(
        &config_dir().join("settings.json"),
        serde_json::to_vec_pretty(&settings)?,
    )?;
    if old.style != settings.style || old.mode != settings.mode {
        apply_blur(&settings);
    }
    if old.mode != settings.mode || old.accent != settings.accent {
        let themed = write_desktop_theme(&settings).unwrap_or_else(|e| {
            eprintln!("Could not write the desktop theme: {e}");
            false
        });
        apply_desktop_mode(settings.mode == "dark", themed, true);
        write_apps(&settings);
        if old.mode != settings.mode {
            crate::apps::signal_foot(settings.dark());
        }
    }
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
    fn gtk_theme_follows_the_mode() {
        let exists = |t: &str| t == "Arc-Dark";
        assert_eq!(paired_theme("Arc", true, exists), "Arc-Dark");
        assert_eq!(paired_theme("Arc-Dark", false, exists), "Arc");
        assert_eq!(paired_theme("Adwaita", true, exists), "Adwaita-dark");
        assert_eq!(paired_theme("Adwaita-dark", false, exists), "Adwaita");
        // Without a dark sibling the theme is kept; color-scheme still switches.
        assert_eq!(paired_theme("Breeze", true, exists), "Breeze");
    }
    #[test]
    fn settings_ini_keys_are_replaced_or_added() {
        let ini = "[Settings]\ngtk-theme-name=Arc-Dark\ngtk-font-name=Sans 11\n";
        let ini = set_ini(ini, "gtk-theme-name", "Arc");
        let ini = set_ini(&ini, "gtk-application-prefer-dark-theme", "0");
        assert_eq!(
            ini,
            "[Settings]\ngtk-application-prefer-dark-theme=0\ngtk-theme-name=Arc\ngtk-font-name=Sans 11\n"
        );
        assert_eq!(set_ini("", "a", "1"), "[Settings]\na=1\n");
    }
    #[test]
    fn accent_text_stays_readable_in_both_modes() {
        for accent in ["#58a0b8", "#40585a", "#e8e0a0", "#91b5a4"] {
            for bg in ["#f2f2f5", "#141416"] {
                let c = readable_on(rgb(accent), rgb(bg));
                assert!(contrast(c, rgb(bg)) >= 3.5, "{accent} on {bg}");
            }
        }
        // Dark accents get white text, light accents dark text.
        assert!(
            css(&Settings {
                accent: "#40585a".into(),
                ..Settings::default()
            })
            .contains("nexus_on_accent #ffffff")
        );
        assert!(
            css(&Settings {
                accent: "#e8e0a0".into(),
                ..Settings::default()
            })
            .contains("nexus_on_accent #16201b")
        );
    }
    #[test]
    fn desktop_colors_cover_both_syntaxes() {
        let colors = desktop_colors("#40585a", true);
        let named = define_colors(&colors);
        assert!(named.contains("@define-color accent_bg_color #40585a;"));
        assert!(named.contains("@define-color window_bg_color #141416;"));
        assert!(named.contains("@define-color accent_fg_color #ffffff;"));
        let vars = color_vars(&colors);
        assert!(vars.contains("--secondary-sidebar-bg-color: "));
        assert!(!vars.contains('_'));
    }
    #[test]
    fn user_stylesheet_keeps_its_own_rules() {
        let once = replace_block("label { color: red; }\n", "a\n");
        assert_eq!(
            once,
            format!("label {{ color: red; }}\n\n{BLOCK_BEGIN}\na\n{BLOCK_END}\n")
        );
        let twice = replace_block(&(once + "button {}\n"), "b\n");
        assert_eq!(
            twice,
            format!("label {{ color: red; }}\n\n{BLOCK_BEGIN}\nb\n{BLOCK_END}\nbutton {{}}\n")
        );
        assert_eq!(
            replace_block("p {}\n", "@import x;\n"),
            format!("{BLOCK_BEGIN}\n@import x;\n{BLOCK_END}\n\np {{}}\n")
        );
        assert_eq!(
            replace_block("", "a\n"),
            format!("{BLOCK_BEGIN}\na\n{BLOCK_END}\n")
        );
    }
    #[test]
    fn wallpaper_accents_carry_colour() {
        let chroma = |c: [f64; 3]| {
            c.iter().cloned().fold(0., f64::max) - c.iter().cloned().fold(1., f64::min)
        };
        // A forest's dominant colour, muddy before.
        assert!(chroma(vivid(rgb("#405858"))) > 0.4);
        // Bark and dry grass come out orange and amber, not brown and mustard.
        for c in ["#5a4030", "#807040"] {
            let v = vivid(rgb(c));
            assert!(chroma(v) > 0.8 && v[0] > 0.9, "{}", hex(v));
        }
        // Grey has no hue to keep.
        assert_eq!(hex(vivid(rgb("#808080"))), Settings::default().accent);
        // Stays between too dark and too light whatever comes in.
        for c in ["#0a1420", "#f0e0d0", "#ff0000"] {
            let v = vivid(rgb(c));
            let l =
                (v.iter().cloned().fold(0., f64::max) + v.iter().cloned().fold(1., f64::min)) / 2.;
            assert!((0.43..=0.57).contains(&l), "{c} -> {l}");
        }
    }
    #[test]
    fn every_style_has_a_stylesheet() {
        for style in &STYLES {
            for mode in ["dark", "light"] {
                let s = Settings {
                    style: style.id.into(),
                    mode: mode.into(),
                    ..Settings::default()
                };
                assert_eq!(s.style().id, style.id);
                assert!(css(&s).contains("nexus_glass rgba("));
            }
        }
        // Settings saved by another version fall back rather than break the shell.
        let s = Settings {
            style: "gone".into(),
            ..Settings::default()
        };
        assert_eq!(s.style().id, "blur");
    }
    #[test]
    fn legacy_settings_load_into_something_saveable() {
        let s = parse(br##"{"style": "liquid", "lock_style": "gone", "gloss": 1}"##);
        assert_eq!(s.style, "blur");
        assert_eq!(s.lock_style, LOCK_STYLES[0].0);
        check(&s).unwrap();
        // What is still current stays, and an unreadable file gives the defaults.
        let s = parse(br##"{"style": "opaque", "lock_style": "bar"}"##);
        assert_eq!((s.style.as_str(), s.lock_style.as_str()), ("opaque", "bar"));
        check(&parse(b"")).unwrap();
    }
    #[test]
    fn generated_accent_is_stable() {
        let p = std::env::temp_dir().join(format!("nexus-color-{}.png", std::process::id()));
        image::RgbImage::from_pixel(4, 4, image::Rgb([60, 130, 180]))
            .save(&p)
            .unwrap();
        assert_eq!(accent_for(p.to_str().unwrap()).unwrap(), "#3eadd2");
        fs::remove_file(p).unwrap();
    }
}
