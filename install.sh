#!/usr/bin/env bash
# User installation. System dependencies: gtk4, networkmanager, bluez, bluez-utils,
# pipewire, wireplumber, libpulse, brightnessctl, swaybg, gtk4-layer-shell, polkit,
# elogind (Artix) or systemd-logind (Arch). Build: rust, pkgconf, base-devel.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
prefix="${PREFIX:-$HOME/.local}"
if ! pkg-config --exists gtk4-layer-shell-0 && [ -d "$HOME/.local/share/nexus-runtime/usr/lib/pkgconfig" ]; then
    export PKG_CONFIG_PATH="$HOME/.local/share/nexus-runtime/usr/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
    export LD_LIBRARY_PATH="$HOME/.local/share/nexus-runtime/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
fi
case "${1:-}" in
    --no-build) ;;
    '') cargo build --manifest-path "$root/Cargo.toml" --release --locked -j "${CARGO_BUILD_JOBS:-4}" ;;
    *) echo 'Usage: ./install.sh [--no-build]' >&2; exit 2 ;;
esac
install -d "$prefix/bin" "$prefix/share/applications" "$prefix/share/nexus" "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
# Atomic replacement works while the old process is running.
for bin in nexus nexusd nexus-files nexus-notes nexus-vaultd nexus-vault-host; do
    install -m755 "$root/target/release/$bin" "$prefix/bin/$bin.new"
    if [ ! -f /usr/lib/libgtk4-layer-shell.so ] && [ -f "$HOME/.local/share/nexus-runtime/usr/lib/libgtk4-layer-shell.so" ] && command -v patchelf >/dev/null; then
        patchelf --set-rpath '$ORIGIN/../share/nexus-runtime/usr/lib' "$prefix/bin/$bin.new"
    fi
    mv -f "$prefix/bin/$bin.new" "$prefix/bin/$bin"
done
install -m755 "$root/packaging/nexus-session" "$prefix/bin/nexus-session"
install -m644 "$root/packaging/nexus.desktop" "$prefix/share/applications/nexus.desktop"
install -m644 "$root/packaging/49-nexus-power.rules" "$prefix/share/nexus/49-nexus-power.rules"
install -m644 "$root/packaging/nexusd.service" "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/nexusd.service"
# GIO uses this standard helper for Terminal=true desktop entries (e.g. btop).
if ! command -v xdg-terminal-exec >/dev/null; then
    install -m755 "$root/vendor/xdg-terminal-exec/xdg-terminal-exec" "$prefix/bin/xdg-terminal-exec"
    install -Dm644 "$root/vendor/xdg-terminal-exec/xdg-terminals.list" "$prefix/share/xdg-terminal-exec/xdg-terminals.list"
    terminal_config="${XDG_CONFIG_HOME:-$HOME/.config}/xdg-terminals.list"
    if [ ! -e "$terminal_config" ]; then
        install -Dm644 "$root/vendor/xdg-terminal-exec/xdg-terminals.list" "$terminal_config"
        # Older Foot packages do not declare their execution argument.
        printf '\n/execarg_default:foot.desktop:--\n' >> "$terminal_config"
    fi
    install -Dm644 "$root/vendor/xdg-terminal-exec/LICENSE" "$prefix/share/licenses/nexus/xdg-terminal-exec.LICENSE"
fi
# Nexus Files: default file manager, FileManager1 ("Show in folder") and the
# FileChooser portal backend, all D-Bus activated into one resident process.
install -m644 "$root/packaging/nexus-files.desktop" "$prefix/share/applications/nexus-files.desktop"
install -m644 "$root/packaging/nexus-messages.desktop" "$prefix/share/applications/nexus-messages.desktop"
install -m644 "$root/packaging/nexus-notes.desktop" "$prefix/share/applications/nexus-notes.desktop"
install -m644 "$root/packaging/nexus-call.desktop" "$prefix/share/applications/nexus-call.desktop"
install -m644 "$root/packaging/nexus-passwords.desktop" "$prefix/share/applications/nexus-passwords.desktop"
install -m644 "$root/packaging/nexus-paint.desktop" "$prefix/share/applications/nexus-paint.desktop"
# The browser extension reaches the vault through this host, named by absolute path.
for browser in BraveSoftware/Brave-Browser chromium; do
    native="${XDG_CONFIG_HOME:-$HOME/.config}/$browser/NativeMessagingHosts"
    install -d "$native"
    sed "s|@HOST@|$prefix/bin/nexus-vault-host|" "$root/extension/native-host.json.in" > "$native/io.github.vitrixxl.nexus.json"
done
data="${XDG_DATA_HOME:-$HOME/.local/share}"
install -d "$data/dbus-1/services" "$data/xdg-desktop-portal/portals"
for service in org.freedesktop.FileManager1 org.freedesktop.impl.portal.desktop.nexus io.github.vitrixxl.NexusFiles; do
    sed "s|@bindir@|$prefix/bin|" "$root/packaging/$service.service" > "$data/dbus-1/services/$service.service"
done
portal_changed=
cmp -s "$root/packaging/nexus.portal" "$data/xdg-desktop-portal/portals/nexus.portal" || portal_changed=1
install -m644 "$root/packaging/nexus.portal" "$data/xdg-desktop-portal/portals/nexus.portal"
# Only the file chooser moves to Nexus; every other portal stays on hyprland/gtk.
portals="${XDG_CONFIG_HOME:-$HOME/.config}/xdg-desktop-portal/hyprland-portals.conf"
if [ ! -e "$portals" ]; then
    install -Dm644 /dev/stdin "$portals" <<'CONF'
[preferred]
default=hyprland;gtk
org.freedesktop.impl.portal.FileChooser=nexus;gtk
CONF
    portal_changed=1
elif ! grep -q '^org.freedesktop.impl.portal.FileChooser=' "$portals"; then
    sed -i '/^\[preferred\]/a org.freedesktop.impl.portal.FileChooser=nexus;gtk' "$portals"
    portal_changed=1
fi
if command -v gdbus >/dev/null; then
    gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus \
        --method org.freedesktop.DBus.ReloadConfig >/dev/null 2>&1 || true
fi
if [ -n "$portal_changed" ] && command -v systemctl >/dev/null; then
    systemctl --user try-restart xdg-desktop-portal.service 2>/dev/null || true
fi
if command -v xdg-mime >/dev/null; then
    xdg-mime default nexus-files.desktop inode/directory
    xdg-mime default nexus-call.desktop x-scheme-handler/tel
fi
"$prefix/bin/nexus" init-theme
# What the phone updates itself from: rebuilt when its sources changed.
apk="$data/nexus/nexus-phone.apk"
if [ -d "${ANDROID_HOME:-$HOME/Android/Sdk}" ] && { [ ! -f "$apk" ] ||
    [ -n "$(find "$root/android" -path "$root/android/build" -prune -o -type f -newer "$apk" -print -quit)" ]; }; then
    "$root/android/build.sh" >/dev/null || printf 'The phone app could not be rebuilt.\n' >&2
fi
if command -v update-desktop-database >/dev/null; then update-desktop-database "$prefix/share/applications"; fi
printf 'Installed Nexus in %s. Start with nexus-session (or nexusd and nexus).\n' "$prefix"
printf 'Optional power policy: sudo install -Dm644 %q /etc/polkit-1/rules.d/49-nexus-power.rules\n' "$prefix/share/nexus/49-nexus-power.rules"
