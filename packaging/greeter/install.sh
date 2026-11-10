#!/usr/bin/env bash
# System install of the Nexus greeter (root). Checks everything it can without
# taking over a VT, and only then swaps the display manager. Takes effect at the
# next boot. Undo: systemctl disable greetd && systemctl enable ly@tty2
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
user="${1:?usage: install.sh <login user>}"
pacman -S --needed --noconfirm greetd cage foot
install -m755 "$root/target/release/nexus-greeter" /usr/local/bin/nexus-greeter
install -m644 "$root/packaging/greeter/foot.ini" /etc/greetd/nexus-greeter-foot.ini
XKBLAYOUT=us XKBVARIANT=
[ -f /etc/vconsole.conf ] && . /etc/vconsole.conf
[ -f /etc/greetd/config.toml ] && [ ! -f /etc/greetd/config.toml.before-nexus ] && cp /etc/greetd/config.toml /etc/greetd/config.toml.before-nexus
sed "s|@XKBLAYOUT@|$XKBLAYOUT|; s|@XKBVARIANT@|${XKBVARIANT:-}|" "$root/packaging/greeter/config.toml" > /etc/greetd/config.toml

echo '== check 1: cage + foot + greeter start as the greeter user (headless)'
cd /  # the greeter user cannot enter a home directory
lab="$(mktemp -d)"; chown greeter "$lab"; chmod 700 "$lab"
runuser -u greeter -- env XDG_RUNTIME_DIR="$lab" WLR_BACKENDS=headless WLR_HEADLESS_OUTPUTS=1 WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
    timeout 8 cage -- foot --config=/etc/greetd/nexus-greeter-foot.ini \
    sh -c "stty size > $lab/size; timeout 4 /usr/local/bin/nexus-greeter > /dev/null; echo \$? > $lab/rc" > "$lab/log" 2>&1 || true
size="$(cat "$lab/size" 2>/dev/null || true)"; rc="$(cat "$lab/rc" 2>/dev/null || true)"
echo "terminal: ${size:-none}, greeter exit: ${rc:-none} (124 = still animating when stopped)"
[ "$rc" = 124 ] || { cat "$lab/log"; rm -rf "$lab"; echo 'FAILED, display manager left unchanged'; exit 1; }
rm -rf "$lab"

echo '== check 2: real greetd + PAM conversation (one deliberately wrong password)'
probe="$(mktemp -d)"; chmod 777 "$probe"
cat > "$probe/config.toml" <<CONF
[terminal]
vt = "none"
[default_session]
command = "sh -c '/usr/local/bin/nexus-greeter --probe $user > $probe/out 2>&1; sleep 20'"
user = "greeter"
CONF
timeout 12 greetd --config "$probe/config.toml" > "$probe/greetd.log" 2>&1 || true
faillock --user "$user" --reset || true
out="$(cat "$probe/out" 2>/dev/null || true)"; echo "${out:-no answer}"
case "$out" in 'probe OK'*) ;; *) cat "$probe/greetd.log"; rm -rf "$probe"; echo 'FAILED, display manager left unchanged'; exit 1 ;; esac
rm -rf "$probe"

echo '== switching the display manager (next boot)'
systemctl disable 'ly@tty2.service' 2>/dev/null || true
systemctl enable greetd.service
systemctl cat greetd.service | grep -vE '^\s*$'
echo DONE
