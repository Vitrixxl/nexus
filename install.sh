#!/usr/bin/env bash
# User installation. System dependencies: gtk4, connman, bluez, bluez-utils,
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
for bin in nexus nexusd; do
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
"$prefix/bin/nexus" init-theme
if command -v update-desktop-database >/dev/null; then update-desktop-database "$prefix/share/applications"; fi
printf 'Installed Nexus in %s. Start with nexus-session (or nexusd and nexus).\n' "$prefix"
printf 'Optional power policy: sudo install -Dm644 %q /etc/polkit-1/rules.d/49-nexus-power.rules\n' "$prefix/share/nexus/49-nexus-power.rules"
