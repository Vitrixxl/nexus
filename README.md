# Nexus

A complete, quiet shell for Hyprland. The bar, application launcher, Control Center, settings window and power menu share one native Rust + GTK 4 interface, backed by a separate Rust daemon. No Waybar or external launcher is required.

- **Bar** — workspaces, clock and stateful Wi-Fi, Bluetooth, sound, brightness, battery and power icons. One bar per monitor, with hotplug support.
- **System tray** — when an application publishes a StatusNotifier/AppIndicator icon, a small applications button appears immediately to the right of Search. Click it to open a compact panel directly below that button, with application icons in one horizontal row and names shown on hover. Click an application to activate it; right-click for its own menu, including checkboxes and submenus. Icons update immediately on application events, with an additional refresh every second for clients that omit signals. Menus update while applications run, and the button disappears when the tray is empty. Applications without a tray icon are not listed; legacy XEmbed-only icons are not supported.
- **Launcher** — click the center of the bar or press Super+Space. A search panel slides down from the bar; search desktop applications, move with ↑/↓ or Ctrl+N / Ctrl+P, open with Enter, dismiss with Escape or an outside click. Frequently used apps rise in the list.
- **Notifications** — Nexus is the desktop's notification server (`org.freedesktop.Notifications`). Popups stack in the top-right corner from the very top of the screen, above the bar and fullscreen windows, with images, body markup, links, progress and action buttons; clicking one runs its default action, hovering holds it on screen. The bell in the bar counts live notifications and drops a list with Do not disturb (only critical notifications pop up) and Clear all.
- **Control Center** — under the bar: switches and levels, and all of Wi-Fi, Bluetooth, Sound and Display unfolding in place with a back button. `nexus wifi`, `bluetooth`, `sound` and `display` open it on that page on the focused monitor, the same command closes it, and the bar's icons do the same. Nothing takes keyboard focus when it opens.
- **Wi-Fi** — NetworkManager networks, signal, radio, scanning, connect/disconnect, forget, showing and copying a saved password, and the choice between Ethernet and Wi-Fi. Passwords are asked in a Nexus prompt (and through a NetworkManager secret agent when a saved one is rejected), handed to NetworkManager over D-Bus and kept out of command-line arguments, logs and Nexus settings. Enterprise networks use PEAP/MSCHAPv2.
- **Bluetooth** — BlueZ discovery, pairing with PIN/passkey confirmation, connect/disconnect and forget.
- **Sound** — PipeWire output and microphone levels, mute and device selection, plus a level and mute switch for each application playing sound.
- **Display** — backlight brightness with `brightnessctl`, and a night light (on/off and colour temperature, on from 21:00 to 7:30) set by `nexusd` itself through wlr-gamma-control.
- **Phone** — pair an Android phone over Tailscale by scanning the QR code on the Phone page with its camera: the page it opens installs the Nexus app (`android/`, plain Java, no dependencies) and pairs it. The page then shows the phone's battery, storage, memory and CPU, opens its files in Nexus Files, and reads and sends SMS. Its notifications pop up in the bottom-left corner (dismissing one dismisses it on the phone). `nexus phone` opens it.
- **Monitor** — CPU (per core, temperature, load), memory, network and disk with two minutes of history, and the processes, sortable and filterable, to end with Delete (Shift+Delete forces it). Read straight from `/proc` and `/sys`, once a second and only while the page shows; `nexus monitor` opens it.
- **Appearance** — light/dark, Opaque or Blur material with its opacity, PNG/JPEG/WebP wallpaper picker, accent or a wallpaper-derived one shared by the bar, launcher and settings, and the lock screen's composition.
- **Screenshots** — `nexus screenshot` dims every monitor and highlights the window under the pointer; click to capture it, drag to capture an area, Escape, a right click or the same shortcut again to cancel. The screen is frozen at the moment of the shortcut, or stays live with `--live` and is captured on release. The PNG goes to the clipboard, and with `--save` also to `~/Pictures/screenshots`; a notification shows a thumbnail. `screen` and `window` capture the focused monitor or the active window at once. Outputs are copied in memory through wlr-screencopy and the picker's windows stay ready in the shell, so it opens within a few frames and animates at the display's refresh rate in the Nexus colours.
- **Lock screen** — `nexus lock` locks every monitor with the Wayland session-lock protocol: a capture of the screen blurs in and a card in the theme's colours rises from the bottom with the time and the password field. Checking, refusals (the field shakes) and PAM messages (such as a faillock lockout) are shown; on success the card drops and the blur clears before unlocking. `nexus lock --preview` shows it in a window without locking anything.
- **Passwords** — `nexus passwords`, a local password vault: AES-256-GCM, PBKDF2-HMAC-SHA256 with one master password, no server, no account. `nexus-vaultd` alone holds the key, and exits once locked and idle; it locks after 15 idle minutes, on the lock screen and on suspend, and answers only the Nexus binaries. With the Android app, a phone syncs ciphertext both ways only once both sides have said yes.
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
nexus quick               # Control Center
nexus wifi                # Control Center on a page, again to close: also bluetooth, sound, display
nexus control             # settings window on the last page shown
nexus appearance          # settings window on a page: also phone, monitor, packages
nexus power               # full-screen power chooser
nexus notifications       # toggle the notification list
nexus screenshot          # pick a window or an area on the frozen screen
nexus screenshot --live --save   # on the live screen, also saved to ~/Pictures/screenshots
nexus screenshot screen   # focused monitor at once (window: active window)
nexus lock                # lock the session
nexus status             # JSON diagnostics, without passwords
```

The complete Hyprland configuration Nexus needs is in [HYPRLAND.md](HYPRLAND.md).

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

## Files

`nexus-files` is the file manager, in the Nexus colours: places, bookmarks (`~/.config/gtk-3.0/bookmarks`), mounted drives and the paired phone in the sidebar; back/forward/up and an editable path (Ctrl+L) in the header; the folder as a grid with thumbnails or a list (Ctrl+G), sorted by name, size or date, hidden files with Ctrl+H.

```sh
nexus-files [PATH|URI]        # a folder, or a file's folder with it selected
nexus-files --phone [PATH]    # the paired phone's storage
```

- **Fast** — one resident process (D-Bus activated, `io.github.vitrixxl.NexusFiles`) serves every window, so a second window or file dialog opens in a few tens of milliseconds. Folders are listed asynchronously into a 64-folder memory cache: visited folders, and folders the pointer rests on or the keyboard selects, open instantly, then a fresh listing and a folder monitor reconcile them.
- **Typing filters** the folder as you type; the globe button next to the filter searches the whole computer by name instead (home folder first, then the rest, without `/proc`, `/sys`, `/dev`, `/run`, `.git` and `node_modules`; files listed in `.gitignore` files are skipped), with `fd`, results streaming in, at most 2000.
- **Thumbnails** come from the freedesktop cache (`~/.cache/thumbnails`); missing ones are made off the main thread for the files on screen only — images by gdk-pixbuf, videos and others by the installed thumbnailers (`ffmpegthumbnailer`…), PDFs by `pdftocairo` — and written back to the cache.
- **Files** — open (Enter, double-click), Open With, cut/copy/paste (Ctrl+X/C/V, interoperable with other file managers), drag and drop in and out, rename (F2), new folder (Ctrl+Shift+N), move to trash (Delete; restore and empty from Trash), delete permanently (Shift+Delete), open a terminal here, a rubber band to select. Archives (zip, 7z, rar, tar.*, iso, deb, rpm…, and single `.gz`/`.xz`/`.zst`/`.bz2` files) extract here or into a folder with `bsdtar`, and Compress… packs a selection (the name's extension picks the format).
- **Phone** — the paired phone's files browse like local ones, with thumbnails; opening downloads a copy into `~/.cache/nexus/phone`; copy and paste work both ways.
- **Default file manager** — the installer makes it the handler of `inode/directory` and of `org.freedesktop.FileManager1`, so "Show in folder" in browsers opens it. Thunar is left installed.

**Browser file dialogs.** Nexus Files implements the `org.freedesktop.impl.portal.FileChooser` portal backend (open, save, save several, folders, filters), so applications that use the file chooser portal — Brave, Chromium, Firefox with `widget.use-xdg-desktop-portal.file-picker=1`, Flatpaks — show it for uploads and downloads. The installer adds `nexus.portal` to `~/.local/share/xdg-desktop-portal/portals` and selects it for the file chooser only in `~/.config/xdg-desktop-portal/hyprland-portals.conf` (`org.freedesktop.impl.portal.FileChooser=nexus;gtk`, everything else stays on `hyprland;gtk`), then restarts the user portal. Dialogs float over the browser window that asked. For a save dialog on every download in Brave, enable Settings › Downloads › "Ask where to save each file before downloading". After updating, `pkill -x nexus-files` (with no window open) loads the new binary.

## Notes

`nexus-notes` is a block editor for notes, in the Nexus colours. Pages nest in a tree in the sidebar; each page is a plain Markdown file in `~/Documents/Notes` (the XDG documents folder; `NEXUS_NOTES_DIR` overrides it), and a page's subpages live in the folder of the same name, so the notes open in any editor. The title is the file name: editing it renames the file.

- **Blocks** — Enter starts a block, Shift+Enter breaks a line inside it (code, quotes), Backspace at its start lifts it to text or joins it to the one above, arrows cross blocks, Tab / Shift+Tab nest any block under the one above (saved as indented Markdown), Alt+Up/Down or the handle on the left move a block (with what is nested in it), Ctrl+Z / Ctrl+Shift+Z undo across blocks. `/` opens a filterable menu of blocks; Markdown typed at the start of a block converts it (`# `, `## `, `### `, `- `, `1. `, `[] `, `> `, ```` ``` ````, `---`). Text, three headings, bulleted, numbered and to-do lists (click the box or Ctrl+Enter), quote, code, divider, callout (GitHub's `> [!NOTE]`). Inline: Ctrl+B bold, Ctrl+I italic, Ctrl+E code (with nothing selected they style what is typed next), Ctrl+K link (Ctrl+click opens one with the default handler).
- **Whole blocks** — Esc or a click on a block's handle selects the block; Shift+Up/Down extend the selection block by block, Up/Down move it, Ctrl+A takes the page. Then Ctrl+C copies as Markdown, Ctrl+X cuts, Backspace deletes, Ctrl+D duplicates, Alt+Up/Down moves, Tab / Shift+Tab nest, Enter edits again.
- **Clipboard** — copying puts real line breaks on the clipboard and the primary selection (Markdown when it spans blocks); pasting Markdown or several lines makes blocks of them.
- **Vim** — the Vim button at the bottom of the sidebar turns vim keys on (off by default; remembered). A small NORMAL / INSERT / VISUAL label shows the mode. hjkl, w b e, 0 ^ $, gg G, counts, i a I A o O, x, dd yy p P, c and d with motions and text objects (cw, dw, d$, ciw, diw…), u and Ctrl+R on the editor's undo, v and V with y d c, / ? n N in the page, `.` repeats a simple edit, Esc or Ctrl+[ back to normal. Motions step over hidden markers; in normal mode keys never type, in insert mode every shortcut and conversion works. The yank register is the editor's own, not the system clipboard.
- **Pages** — Ctrl+N new page, Ctrl+Shift+N subpage, F2 rename; right-click a page for New subpage, Rename, Delete. The title is the file name; a title another page here already has is refused with a note under the page head, and the file keeps its name until the title is free. An optional icon from a small set (the button above the title) is kept as `icon:` in the page's front matter. Drag pages to reorder them (a line shows before or after) or drop one on another to nest it (the order is kept in a `.order` file per folder).
- **Trash** — Delete moves the page and its subpages to `.trash` in the notes folder, with Undo. The Trash view (bottom of the sidebar) lists them with Restore and Delete Permanently, and Empty Trash after a confirmation; items older than 30 days are purged at start.
- **Search** — Ctrl+P searches titles and text of every page, off the main thread; opening a result selects the match.
- **Safe** — saves 0.6 s after typing stops, on page switch, on close, Ctrl+Q and SIGTERM, by writing a temporary file and renaming it. The files round-trip byte for byte, blank lines included; Markdown the editor does not draw (tables, HTML, footnotes…) is kept as written. Files changed by another program (a sync tool) are picked up while open: the page shown reloads in place when not being edited, pages added, removed or renamed appear in the tree, and if both sides changed, ours is saved and the other version is kept beside it as "Page (changed elsewhere DATE)".

## Themes / wallpapers

Settings live in `$XDG_CONFIG_HOME/nexus/settings.json` (default `~/.config/nexus`). The daemon generates `gtk.css`; the persistent shell applies theme changes live across the bar and all panels. Nexus also sets the desktop light/dark preference when a session bus is available.

When adw-gtk3 is installed (system-wide or in `~/.local/share/themes`), Nexus generates the `Nexus` / `Nexus-dark` GTK 3/4 themes on top of it with the same surfaces and accent, selects them, and writes the matching libadwaita colours inside a marked block of `~/.config/gtk-4.0/gtk.css` (the rest of that file is kept). Chromium-based browsers set to the GTK theme (Brave: Settings › Appearance › Theme › GTK) use the same colours. Running GTK 3 applications follow accent changes live; libadwaita applications pick up new colours when restarted. Nexus also writes colours for applications outside GTK:

- **foot**: `~/.config/nexus/foot.ini` with `[colors-dark]` and `[colors-light]`; add `include=~/.config/nexus/foot.ini` to `foot.ini`. Open windows switch mode live (SIGUSR1/SIGUSR2); accent changes apply to new windows.
- **Neovim**: the `nexus` colorscheme in `~/.local/share/nvim/site/colors/nexus.lua`. Reload it when the file changes, e.g. with a `vim.uv.new_fs_event()` watcher on that directory.
- **T3 Code**: the `nexus` theme in `~/.t3/userdata/themes/nexus.json`, written when that directory exists: the background and accent T3 Code derives its palette from. Choose it once with `t3 theme set nexus`; it repaints whenever the file changes.
- **btop**: the `nexus` theme in `~/.config/btop/themes/nexus.theme`, written when `~/.config/btop` exists; set `color_theme = "nexus"` in `btop.conf`. Running btops reload it on SIGUSR2, which Nexus sends when it changes.
- **Equibop / Vesktop**: a marked block at the top of their QuickCSS, overriding the colour variables of the [midnight](https://github.com/refact0r/midnight-discord) theme it imports. The clients reload QuickCSS live.

Other third-party applications are not recolored.

Wallpaper colors are opt-in. Images are quantized into color buckets to obtain a deterministic accent, with light/dark surfaces selected independently for legibility. Wallpaper rendering uses `swaybg` across outputs. Selecting an image replaces the legacy `mpvpaper` wallpaper; until then the existing wallpaper is retained. A selected image is restored at the next session. Video wallpaper selection is not yet supported.

## Phone

**Updates:** `install.sh` rebuilds the phone app when its sources changed; the app then offers "Update Nexus" on its main screen.

**Screen:** `nexus phone mirror` (or Screen on the Phone page) shows the phone's screen in a window, where the mouse and keyboard act on it. Agents and scripts use the same commands from a shell: `nexus phone tree` prints what is on screen as JSON (windows, texts, boxes, possible actions), `shot` saves a screenshot, `tap`, `hold`, `swipe`, `type`, `key`, `apps` and `open` act, with positions in screen pixels. It takes Allow screen control in the app (Nexus in Android's Accessibility settings). Screenshots and control need no further agreement; the live stream asks Android to share the screen each time, and that prompt can be answered from the mirror itself. Windows that forbid capture (banking apps) come out black, and a PIN-locked phone must be unlocked by hand.

**Calendar:** the notification center shows the phone's calendars, Google's included, with each day's events, and adds, changes or deletes them. It needs Allow calendar in the app. Changes made while the phone is away wait on the desktop and reach it when it connects; of a repeating event, a change applies to that occurrence alone. The app has the same month and a home-screen widget that resizes from today and what is next to the whole month.

**Calls:** `nexus call [number or tel: link]` places the call on the phone; `tel:` links open it, and calling takes a click. The sound stays on the phone. **Messages** are kept in `~/.cache/nexus/messages` (last 500 per conversation, removed on unpairing) and only what is new is read from the phone.

`nexusd` listens on the desktop's Tailscale address only (port 47913), so the phone and the desktop must be on the same tailnet. The token in the QR code is made on the first run and kept in `$XDG_CONFIG_HOME/nexus/phone.json`; it never changes, so the code stays the same and a paired phone reconnects with a tap on the desktop in the app. It is the only credential: delete the file and restart `nexusd` to revoke every phone. The app links to one desktop at a time, the one tapped last, and a desktop to one phone, the one that connected last; Disconnect drops the phone until it is tapped again. Tailscale encrypts the link. Build the app with `android/build.sh` (a JDK and the Android SDK build-tools); the daemon serves `~/.local/share/nexus/nexus-phone.apk` to the pairing page. On the phone, grant what the app lists: notification access, all files access, SMS and contacts, and no battery optimisation so the link survives Doze. The listener binds the address Tailscale had when the daemon started. A phone that can't scan pairs with the link the Phone page copies (Copy link), pasted in the app under Pair with a link. The bar shows the phone's battery while it is connected, a dimmed phone while the paired one is away, and a red one when the link can't start.


**Paint** (`nexus paint`, Super+P) is an infinite whiteboard in a floating window, 90% of the screen wide and 80% tall, that follows the Nexus theme: rectangles, diamonds, ellipses, arrows, lines, freehand and text, with the keys shown in each tool's tooltip. Double-click a shape, or press Enter on a selected one, to write inside it; the shape grows when its text needs the room. An arrow or a line dragged from a shape to another stays tied to both and follows them. Boards save by themselves in `~/.local/share/nexus/boards`, one file each, and the Boards sidebar lists them with a preview. The pen and the eraser, which has three sizes, show their real footprint as the cursor. Space-drag or the middle button pans, Ctrl+scroll or a pinch zooms.

The clipboard is shared: text copied on the desktop lands on the phone's clipboard (not what password managers mark as secret). Android lets only the app in front read its clipboard, so the other way goes through the app's Quick Settings tile, Send clipboard, or Share → Copy on desktop. `nexus messages` opens the phone's conversations in a window of their own. `nexus clipboard` (Super+Shift+V) opens the clipboard history at the pointer: the last 50 texts copied here or on the phone, the phone's marked, kept in memory only.

From a terminal, `nexus phone` takes commands; relative phone paths start at its storage root:

```sh
nexus phone ls Download
nexus phone push -m ~/Videos/clip.mp4 DCIM   # -m moves instead of copying
nexus phone pull DCIM/Camera ~/Pictures
nexus phone mv Download/a.pdf Documents/a.pdf
nexus phone rm Download/old.zip
nexus phone sms +33600000000 On arrive
```

## Architecture and limits

`nexusd` talks to NetworkManager, BlueZ and login1 over the system bus via `zbus`. Audio and backlight helpers run with argument arrays and bounded execution time. Independent background pollers isolate each subsystem. The shell uses GTK4 layer-shell surfaces for the reserved top bar, the launcher panel that drops from its center and the full-screen power chooser. The Wi-Fi, Bluetooth, Sound, Display and Appearance pages live in a separate control center window (class `io.github.vitrixxl.Nexus`, title `Nexus`); on Hyprland, float it with a window rule such as `hl.window_rule({ name = "nexus-control-float", match = { class = "^(io\\.github\\.vitrixxl\\.Nexus)$", title = "^(Nexus)$" }, float = true, size = "900 640" })`. Application discovery and launching use GIO desktop entries (including their icons and launch flags), without interpreting search text as commands. Usage history is stored locally in `$XDG_STATE_HOME/nexus/launcher.json`.

The notification server runs in the shell process on the session bus GLib finds, so it works in sessions that only autolaunch the bus. It claims the name as the shell starts, before GTK, so an early notification does not get another daemon activated. If another notification daemon (mako, dunst, swaync) already owns the name, Nexus waits and takes over when it exits; stop it, and do not autostart it alongside Nexus. Notifications live in memory: up to 100 stay in the list until dismissed, closed by their application or the shell restarts. Popups that time out leave the notification in the list, still actionable, except transient ones. Do not disturb lasts for the session.

The GUI communicates over newline-delimited JSON on a mode-0600 socket in a mode-0700 `$XDG_RUNTIME_DIR/nexus` directory. Pairing and credential prompts expire after 90 seconds. All processes run as the desktop user.

Nexus lists the networks seen by the first Wi-Fi device NetworkManager manages; hidden networks are not listed. New enterprise networks are created with PEAP/MSCHAPv2 and no CA certificate; other EAP methods and certificates are configured with `nmcli` or `nmtui`, after which Nexus connects to them like any saved network. A new network whose first connection fails is not kept. Bluetooth controls currently target the first adapter. Brightness requires a kernel backlight device and its normal user permissions; external monitor DDC is not included. Selecting an audio device changes the default for new streams; existing application streams may keep their original routing.

Wi-Fi and Bluetooth discovery runs automatically every 15 seconds while each radio is enabled, even with Control Center closed. Bluetooth scans last 10 seconds, and manual refreshes share the same discovery session to avoid overlapping scans. Cached network and device lists update every two seconds.

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
