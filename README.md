# Nexus

A complete, quiet shell for Hyprland. The bar, application launcher, settings panels and power menu share one native Rust + GTK 4 interface, backed by a separate Rust daemon. No Waybar or external launcher is required.

- **Bar** — workspaces, clock and stateful Wi-Fi, Bluetooth, sound, brightness, battery and power icons. One bar per monitor, with hotplug support.
- **System tray** — when an application publishes a StatusNotifier/AppIndicator icon, a small applications button appears immediately to the right of Search. Click it to open a compact panel directly below that button, with application icons in one horizontal row and names shown on hover. Click an application to activate it; right-click for its own menu, including checkboxes and submenus. Icons update immediately on application events, with an additional refresh every second for clients that omit signals. Menus update while applications run, and the button disappears when the tray is empty. Applications without a tray icon are not listed; legacy XEmbed-only icons are not supported.
- **Launcher** — click the center of the bar or press Super+Space. A search panel slides down from the bar; search desktop applications, move with ↑/↓ or Ctrl+N / Ctrl+P, open with Enter, dismiss with Escape or an outside click. Frequently used apps rise in the list.
- **Notifications** — Nexus is the desktop's notification server (`org.freedesktop.Notifications`). Popups stack in the top-right corner from the very top of the screen, above the bar and fullscreen windows, with images, body markup, links, progress and action buttons; clicking one runs its default action, hovering holds it on screen. The bell in the bar counts live notifications and drops a list with Do not disturb (only critical notifications pop up) and Clear all.
- **Wi-Fi** — NetworkManager networks, signal, radio, scanning, connect/disconnect and forget. Passwords are asked in a Nexus prompt (and through a NetworkManager secret agent when a saved one is rejected), handed to NetworkManager over D-Bus and kept out of command-line arguments, logs and Nexus settings. Enterprise networks use PEAP/MSCHAPv2.
- **Bluetooth** — BlueZ discovery, pairing with PIN/passkey confirmation, connect/disconnect and forget.
- **Sound** — PipeWire output and microphone levels, mute and device selection, plus a level and mute switch for each application playing sound.
- **Display** — backlight brightness with `brightnessctl`, and a night light (on/off, colour temperature, return to schedule) set by `nexusd` itself through wlr-gamma-control.
- **Appearance** — light/dark, PNG/JPEG/WebP wallpaper picker, optional wallpaper-derived accent shared by the bar, launcher and settings.
- **Screenshots** — `nexus screenshot` dims every monitor and highlights the window under the pointer; click to capture it, drag to capture an area, Escape, a right click or the same shortcut again to cancel. The screen is frozen at the moment of the shortcut, or stays live with `--live` and is captured on release. The PNG goes to the clipboard, and with `--save` also to `~/Pictures/screenshots`; a notification shows a thumbnail. `screen` and `window` capture the focused monitor or the active window at once. Outputs are copied in memory through wlr-screencopy and the picker's windows stay ready in the shell, so it opens within a few frames and animates at the display's refresh rate in the Nexus colours.
- **Lock screen** — `nexus lock` locks every monitor with the Wayland session-lock protocol: a capture of the screen blurs in and a card in the theme's colours rises from the bottom with the time and the password field. Checking, refusals (the field shakes) and PAM messages (such as a faillock lockout) are shown; on success the card drops and the blur clears before unlocking. `nexus lock --preview` shows it in a window without locking anything.
- **Power** — full-screen Sleep / Restart / Shutdown chooser, with confirmation. Uses login1 (elogind or systemd-logind), without shelling out to sudo.

The UI is entirely in English. A singleton daemon keeps state and the application catalogue in memory before panels open. GIO monitors desktop entry changes; the shell caches rows and icons. Workspace changes arrive through Hyprland’s event socket rather than a polling timer. The shell stays running when a panel is dismissed. Opening the same page again toggles it closed. Escape dismisses the launcher, power chooser or control center without stopping the bar.

## Install

Build dependencies: a current stable Rust toolchain, a C compiler, `pkgconf`, GTK 4.10 or later, `gtk4-layer-shell` (1.1 or later, which also provides the session lock library) and the PAM headers (`pam`; `libpam0g-dev` on Debian and Ubuntu).
Runtime dependencies: `networkmanager`, `bluez`, `pipewire`, `wireplumber`, `libpulse` (`pactl`), `brightnessctl`, `swaybg`, `wl-clipboard`, `coreutils`, `procps-ng`, `polkit`, plus **elogind on Artix** or **systemd-logind on Arch**. Optional: `adw-gtk-theme` (adw-gtk3) to carry the Nexus colours into GTK applications and browsers. The shell requires Wayland with the layer-shell protocol (Hyprland supports it).

```sh
git clone https://github.com/Vitrixxl/nexus.git
cd nexus
./install.sh
```

The installer builds with the committed lockfile and installs into `~/.local` without root. Add `~/.local/bin` to your PATH. It installs the desktop launcher, daemon, session script and an optional systemd user service. It also installs the bundled, separately GPL-licensed `xdg-terminal-exec` helper if missing, so terminal applications open in an installed terminal such as Foot. It does not switch your network manager or enable system services.

The [dotfiles installer](https://github.com/Vitrixxl/dotfiles) installs Nexus automatically, enables NetworkManager and starts the complete shell. **NetworkManager is the Wi-Fi backend**: make sure it, BlueZ and your audio services are running, and do not run a second network manager (ConnMan, iwd on its own, systemd-networkd, dhcpcd) against the same interface. Networks joined from Nexus are system-wide profiles, so they connect at boot; NetworkManager lets members of `wheel` create them without a password prompt (as packaged on Arch), other users need a polkit agent or rule for `org.freedesktop.NetworkManager.settings.modify.system`.

## Start / shortcuts

```sh
nexus-session             # daemon + complete shell; works with runit/OpenRC/systemd
nexus shell               # persistent bar, with panels initially closed
nexus launcher            # toggle the integrated application launcher
nexus launcher firefox    # open with a search query
nexus tray                # toggle the tray panel
nexus battery             # battery estimate and power profiles
nexus control             # control center on the last page shown
nexus wifi
nexus bluetooth
nexus sound
nexus display
nexus appearance
nexus power               # full-screen power chooser
nexus notifications       # toggle the notification list
nexus screenshot          # pick a window or an area on the frozen screen
nexus screenshot --live --save   # on the live screen, also saved to ~/Pictures/screenshots
nexus screenshot screen   # focused monitor at once (window: active window)
nexus lock                # lock the session
nexus status             # JSON diagnostics, without passwords
```

Hyprland Lua startup: `hl.exec_cmd("~/.local/bin/nexus-session")` as the first command inside the `hyprland.start` handler. The bar maps without waiting for the daemon; application discovery and hardware state load in the background. Later shortcuts activate the resident shell directly, without a daemon round trip. The dotfiles replace Fuzzel with the integrated launcher on `Super+D` / `Super+Space`, and add `Super+N` for the control center, `Super+W` for Wi-Fi, `Super+Alt+N/B/A/P` for Nexus / Bluetooth / Sound / Power and `Ctrl+Alt+Delete` for Power. Hyprland's live and repository configs remain separate files. Screenshots: bind `nexus screenshot` (e.g. `Super+Shift+S`, and `Print` for `--live --save`) and turn off Hyprland's layer animation for the picker, which fades itself and whose fade-out would otherwise end up in live captures: `hl.layer_rule({ name = "nexus-screenshot-no-anim", match = { namespace = "^nexus-screenshot$" }, no_anim = true })`.

On systemd desktops you can instead enable the optional service with `systemctl --user enable --now nexusd`. Do not enable both startup methods unnecessarily; a runtime file lock prevents duplicate daemons. Restart the daemon after updating binaries. On runit/OpenRC it is a desktop-session process, not a root service.

## Lock screen

`nexus lock` runs as its own process, so the lock holds whatever happens to the shell; if it ever dies, the compositor keeps the session locked. A second `nexus lock` while locked does nothing, so it can serve both a shortcut and hypridle (`lock_cmd = nexus lock`). The password is checked with the PAM service `/etc/pam.d/nexus` when it exists, otherwise `login` (the stack hyprlock uses); with `pam_faillock`, repeated failures lock the account for a while, as with any locker.

## Battery and power profiles

Click the battery in the bar to open a compact panel below it, aligned to the right edge of the battery button. The three buttons select Power saver, Balanced or Performance; the active profile is highlighted and unsupported modes are disabled. Changes made with `powerprofilesctl` appear while the panel is open. The estimate shows time until fully charged or remaining battery life, with explicit charging/full/unavailable states when no estimate is available.

Install and run `power-profiles-daemon` for profile switching and `upower` for battery estimates. Nexus talks directly to their system D-Bus APIs, so the menu does not require `powerprofilesctl` or Python bindings. Profile changes use the service's normal polkit permissions; failures are shown in the panel.

## Power without sudo

First try `loginctl poweroff` (this immediately shuts down the machine). Nexus uses the equivalent D-Bus method. `loginctl reboot` and `loginctl suspend` work similarly.

If your local session is denied permission, install the included narrowly scoped polkit rule once:

```sh
sudo install -Dm644 ~/.local/share/nexus/49-nexus-power.rules /etc/polkit-1/rules.d/49-nexus-power.rules
```

It grants only shutdown, restart and suspend to active local sessions, including when multiple sessions exist. It does not grant remote access or ignore inhibitors. No sudoers wildcard is needed. You can check without shutting down:

```sh
busctl --system call org.freedesktop.login1 /org/freedesktop/login1 org.freedesktop.login1.Manager CanPowerOff
```

## Themes / wallpapers

Settings live in `$XDG_CONFIG_HOME/nexus/settings.json` (default `~/.config/nexus`). The daemon generates `gtk.css`; the persistent shell applies theme changes live across the bar and all panels. Nexus also sets the desktop light/dark preference when a session bus is available.

When adw-gtk3 is installed (system-wide or in `~/.local/share/themes`), Nexus generates the `Nexus` / `Nexus-dark` GTK 3/4 themes on top of it with the same surfaces and accent, selects them, and writes the matching libadwaita colours inside a marked block of `~/.config/gtk-4.0/gtk.css` (the rest of that file is kept). Chromium-based browsers set to the GTK theme (Brave: Settings › Appearance › Theme › GTK) use the same colours. Running GTK 3 applications follow accent changes live; libadwaita applications pick up new colours when restarted. Nexus also writes colours for applications outside GTK:

- **foot**: `~/.config/nexus/foot.ini` with `[colors-dark]` and `[colors-light]`; add `include=~/.config/nexus/foot.ini` to `foot.ini`. Open windows switch mode live (SIGUSR1/SIGUSR2); accent changes apply to new windows.
- **Neovim**: the `nexus` colorscheme in `~/.local/share/nvim/site/colors/nexus.lua`. Reload it when the file changes, e.g. with a `vim.uv.new_fs_event()` watcher on that directory.
- **Equibop / Vesktop**: a marked block at the top of their QuickCSS, overriding the colour variables of the [midnight](https://github.com/refact0r/midnight-discord) theme it imports. The clients reload QuickCSS live.

Other third-party applications are not recolored.

Wallpaper colors are opt-in. Images are quantized into color buckets to obtain a deterministic accent, with light/dark surfaces selected independently for legibility. Wallpaper rendering uses `swaybg` across outputs. Selecting an image replaces the legacy `mpvpaper` wallpaper; until then the existing wallpaper is retained. A selected image is restored at the next session. Video wallpaper selection is not yet supported.

## Architecture and limits

`nexusd` talks to NetworkManager, BlueZ and login1 over the system bus via `zbus`. Audio and backlight helpers run with argument arrays and bounded execution time. Independent background pollers isolate each subsystem. The shell uses GTK4 layer-shell surfaces for the reserved top bar, the launcher panel that drops from its center and the full-screen power chooser. The Wi-Fi, Bluetooth, Sound, Display and Appearance pages live in a separate control center window (class `io.github.vitrixxl.Nexus`, title `Nexus`); on Hyprland, float it with a window rule such as `hl.window_rule({ name = "nexus-control-float", match = { class = "^(io\\.github\\.vitrixxl\\.Nexus)$", title = "^(Nexus)$" }, float = true, size = "900 640" })`. Application discovery and launching use GIO desktop entries (including their icons and launch flags), without interpreting search text as commands. Usage history is stored locally in `$XDG_STATE_HOME/nexus/launcher.json`.

The notification server runs in the shell process on the session bus GLib finds, so it works in sessions that only autolaunch the bus. It claims the name as the shell starts, before GTK, so an early notification does not get another daemon activated. If another notification daemon (mako, dunst, swaync) already owns the name, Nexus waits and takes over when it exits; stop it, and do not autostart it alongside Nexus. Notifications live in memory: up to 100 stay in the list until dismissed, closed by their application or the shell restarts. Popups that time out leave the notification in the list, still actionable, except transient ones. Do not disturb lasts for the session.

The GUI communicates over newline-delimited JSON on a mode-0600 socket in a mode-0700 `$XDG_RUNTIME_DIR/nexus` directory. Pairing and credential prompts expire after 90 seconds. All processes run as the desktop user.

Nexus lists the networks seen by the first Wi-Fi device NetworkManager manages; hidden networks are not listed. New enterprise networks are created with PEAP/MSCHAPv2 and no CA certificate; other EAP methods and certificates are configured with `nmcli` or `nmtui`, after which Nexus connects to them like any saved network. A new network whose first connection fails is not kept. Bluetooth controls currently target the first adapter. Brightness requires a kernel backlight device and its normal user permissions; external monitor DDC is not included. Selecting an audio device changes the default for new streams; existing application streams may keep their original routing.

Wi-Fi and Bluetooth discovery runs automatically every 15 seconds while each radio is enabled, even with the control center closed. Bluetooth scans last 10 seconds, and manual refreshes share the same discovery session to avoid overlapping scans. Cached network and device lists update every two seconds.

Missing hardware, a stopped service or denied permissions are reported in the UI. No power operation is performed automatically. The IPC is private to your user; it is not a network service.

## Development

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
GDK_BACKEND=x11 xvfb-run -a cargo test --locked --bin nexus keyboard_navigation -- --ignored --test-threads=1
dbus-run-session -- cargo test --locked --test agents -- --ignored
dbus-run-session -- cargo test --locked --test notifications -- --ignored
dbus-run-session -- cargo test --locked --test tray -- --ignored
dbus-run-session -- cargo test --locked --test power -- --ignored
GTK_A11Y=none GDK_BACKEND=x11 xvfb-run -a dbus-run-session -- cargo test --locked --bin nexus tray_menu -- --ignored --test-threads=1
GDK_BACKEND=x11 xvfb-run -a cargo test --locked --bin nexus lock_view -- --ignored
```

D-Bus tests exercise Wi-Fi secret exchange, stale prompt rejection, Bluetooth confirmation/rejection and display-only passkeys on a private bus, without connecting real devices. The tray test registers mock applications on a private bus, checks icon conversion, status changes, activation, menu actions, coexistence with another host and removal on exit. The notification test sends, replaces, closes and activates notifications over a private bus and checks the signals applications receive. Unit tests cover input validation, search ranking, desktop entry launching, workspace event selection and deterministic theme extraction. The lock view test drives checking and refusal offscreen without locking anything or calling PAM; `NEXUS_LOCK_TEST=N nexus lock` locks for real and unlocks on its own after N seconds. The isolated GTK test checks Ctrl+N/P, arrow navigation and Enter launching the selected application through a mock daemon.

Primary API references: [NetworkManager D-Bus API](https://networkmanager.dev/docs/api/latest/spec.html), [BlueZ agent](https://github.com/bluez/bluez/blob/master/doc/org.bluez.Agent.rst), [GTK](https://docs.gtk.org/gtk4/), [login1](https://www.freedesktop.org/software/systemd/man/latest/org.freedesktop.login1.html).
