# Nexus

A complete, quiet shell for Hyprland. The bar, application launcher, settings panels and power menu share one native Rust + GTK 4 interface, backed by a separate Rust daemon. No Waybar or external launcher is required.

- **Bar** — workspaces, clock and stateful Wi-Fi, Bluetooth, sound, brightness, battery and power icons. One bar per monitor, with hotplug support.
- **Launcher** — click the center of the bar or press Super+Space. A search panel slides down from the bar; search desktop applications, move with ↑/↓ or Ctrl+N / Ctrl+P, open with Enter, dismiss with Escape or an outside click. Frequently used apps rise in the list.
- **Wi-Fi** — ConnMan networks, signal, radio, scanning, connect/disconnect and forget. Credentials are requested through a D-Bus agent and kept out of command-line arguments, logs and Nexus settings.
- **Bluetooth** — BlueZ discovery, pairing with PIN/passkey confirmation, connect/disconnect and forget.
- **Sound** — PipeWire output and microphone levels, mute and device selection, plus a level and mute switch for each application playing sound.
- **Display** — backlight brightness with `brightnessctl`.
- **Appearance** — light/dark, PNG/JPEG/WebP wallpaper picker, optional wallpaper-derived accent shared by the bar, launcher and settings.
- **Power** — full-screen Sleep / Restart / Shutdown chooser, with confirmation. Uses login1 (elogind or systemd-logind), without shelling out to sudo.

The UI is entirely in English. A singleton daemon keeps state and the application catalogue in memory before panels open. GIO monitors desktop entry changes; the shell caches rows and icons. Workspace changes arrive through Hyprland’s event socket rather than a polling timer. The shell stays running when a panel is dismissed. Opening the same page again toggles it closed. Escape dismisses the launcher, power chooser or control center without stopping the bar.

## Install

Build dependencies: a current stable Rust toolchain, a C compiler, `pkgconf`, GTK 4.10 or later, and `gtk4-layer-shell`.
Runtime dependencies: `connman`, `bluez`, `pipewire`, `wireplumber`, `libpulse` (`pactl`), `brightnessctl`, `swaybg`, `coreutils`, `procps-ng`, `polkit`, plus **elogind on Artix** or **systemd-logind on Arch**. The shell requires Wayland with the layer-shell protocol (Hyprland supports it).

```sh
git clone https://github.com/Vitrixxl/nexus.git
cd nexus
./install.sh
```

The installer builds with the committed lockfile and installs into `~/.local` without root. Add `~/.local/bin` to your PATH. It installs the desktop launcher, daemon, session script and an optional systemd user service. It also installs the bundled, separately GPL-licensed `xdg-terminal-exec` helper if missing, so terminal applications open in an installed terminal such as Foot. It does not switch your network manager or enable system services.

The [dotfiles installer](https://github.com/Vitrixxl/dotfiles) installs Nexus automatically and starts the complete shell. **ConnMan is currently the only Wi-Fi backend**; existing NetworkManager configuration is deliberately left alone. Make sure ConnMan/BlueZ and your audio services are running. Do not run two network managers against the same interface.

## Start / shortcuts

```sh
nexus-session             # daemon + complete shell; works with runit/OpenRC/systemd
nexus shell               # persistent bar, with panels initially closed
nexus launcher            # toggle the integrated application launcher
nexus launcher firefox    # open with a search query
nexus control             # control center on the last page shown
nexus wifi
nexus bluetooth
nexus sound
nexus display
nexus appearance
nexus power               # full-screen power chooser
nexus status             # JSON diagnostics, without passwords
```

Hyprland Lua startup: `hl.exec_cmd("~/.local/bin/nexus-session")` inside the `hyprland.start` handler. The dotfiles replace Fuzzel with the integrated launcher on `Super+D` / `Super+Space`, and add `Super+N` for the control center, `Super+W` for Wi-Fi, `Super+Alt+N/B/A/P` for Nexus / Bluetooth / Sound / Power and `Ctrl+Alt+Delete` for Power. Hyprland's live and repository configs remain separate files.

On systemd desktops you can instead enable the optional service with `systemctl --user enable --now nexusd`. Do not enable both startup methods unnecessarily; a runtime file lock prevents duplicate daemons. Restart the daemon after updating binaries. On runit/OpenRC it is a desktop-session process, not a root service.

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

Settings live in `$XDG_CONFIG_HOME/nexus/settings.json` (default `~/.config/nexus`). The daemon generates `gtk.css`; the persistent shell applies theme changes live across the bar and all panels. Nexus also sets the desktop light/dark preference when a session bus is available; arbitrary third-party applications are not recolored.

Wallpaper colors are opt-in. Images are quantized into color buckets to obtain a deterministic accent, with light/dark surfaces selected independently for legibility. Wallpaper rendering uses `swaybg` across outputs. Selecting an image replaces the legacy `mpvpaper` wallpaper; until then the existing wallpaper is retained. A selected image is restored at the next session. Video wallpaper selection is not yet supported.

## Architecture and limits

`nexusd` talks to ConnMan, BlueZ and login1 over the system bus via `zbus`. Audio and backlight helpers run with argument arrays and bounded execution time. Independent background pollers isolate each subsystem. The shell uses GTK4 layer-shell surfaces for the reserved top bar, the launcher panel that drops from its center and the full-screen power chooser. The Wi-Fi, Bluetooth, Sound, Display and Appearance pages live in a separate control center window (class `nexus`, title `Nexus`); on Hyprland, float it with a window rule such as `hl.window_rule({ name = "nexus-control-float", match = { class = "^(nexus)$", title = "^(Nexus)$" }, float = true, size = "900 640" })`. Application discovery and launching use GIO desktop entries (including their icons and launch flags), without interpreting search text as commands. Usage history is stored locally in `$XDG_STATE_HOME/nexus/launcher.json`.

The GUI communicates over newline-delimited JSON on a mode-0600 socket in a mode-0700 `$XDG_RUNTIME_DIR/nexus` directory. Pairing and credential prompts expire after 90 seconds. All processes run as the desktop user.

New personal/open Wi-Fi networks use ConnMan's agent. Enterprise networks work when provisioned in ConnMan already; the UI does not configure EAP methods or CA certificates. Forgetting an immutable provisioned network may be refused by ConnMan: edit its provisioning file through your normal administration process. Bluetooth controls currently target the first adapter. Brightness requires a kernel backlight device and its normal user permissions; external monitor DDC is not included. Selecting an audio device changes the default for new streams; existing application streams may keep their original routing.

Missing hardware, a stopped service or denied permissions are reported in the UI. No power operation is performed automatically. The IPC is private to your user; it is not a network service.

## Development

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
GDK_BACKEND=x11 xvfb-run -a cargo test --locked --bin nexus keyboard_navigation -- --ignored --test-threads=1
dbus-run-session -- cargo test --locked --test agents -- --ignored
```

D-Bus tests exercise Wi-Fi secret exchange, stale prompt rejection, Bluetooth confirmation/rejection and display-only passkeys on a private bus, without connecting real devices. Unit tests cover input validation, search ranking, desktop entry launching, workspace event selection and deterministic theme extraction. The isolated GTK test checks Ctrl+N/P, arrow navigation and Enter launching the selected application through a mock daemon.

Primary API references: [ConnMan agent](https://git.kernel.org/pub/scm/network/connman/connman.git/tree/doc/agent-api.txt), [BlueZ agent](https://github.com/bluez/bluez/blob/master/doc/org.bluez.Agent.rst), [GTK](https://docs.gtk.org/gtk4/), [login1](https://www.freedesktop.org/software/systemd/man/latest/org.freedesktop.login1.html).
