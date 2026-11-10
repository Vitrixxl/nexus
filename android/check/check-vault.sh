#!/bin/sh
# Runs VaultStore's checks against the interop vector on a device, under
# app_process: the real Conscrypt, org.json and Base64, with no debug path in the
# APK. Pass the emulator's serial. A real phone would work too; don't.
set -eu
serial=${1:?usage: check-vault.sh <adb serial>}
cd "$(dirname "$0")/.."
JAVA_HOME=${JAVA_HOME:-$(ls -d "$HOME"/.local/share/jdk/jdk-* | head -1)}
export PATH="$JAVA_HOME/bin:$PATH"
sdk=${ANDROID_HOME:-$HOME/Android/Sdk}
tools=$sdk/build-tools/$(ls "$sdk/build-tools" | sort -V | tail -1)
jar=$sdk/platforms/$(ls "$sdk/platforms" | sort -V | tail -1)/android.jar
adb="$sdk/platform-tools/adb -s $serial"
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
mkdir "$out/c"
javac -Xlint:-options --release 17 -classpath "$jar" -d "$out/c" src/io/github/vitrixxl/nexus/VaultStore.java check/VaultCheck.java
"$tools/d8" --min-api 26 --lib "$jar" --output "$out" $(find "$out/c" -name '*.class')
$adb push "$out/classes.dex" /data/local/tmp/vaultcheck.dex >/dev/null
$adb push ../tests/vault-vector.json /data/local/tmp/vault-vector.json >/dev/null
$adb shell 'mkdir -p /data/local/tmp/vaultcheck && CLASSPATH=/data/local/tmp/vaultcheck.dex app_process /data/local/tmp/vaultcheck io.github.vitrixxl.nexus.VaultCheck /data/local/tmp/vault-vector.json /data/local/tmp/vaultcheck 2>&1; rc=$?; rm -rf /data/local/tmp/vaultcheck /data/local/tmp/vaultcheck.dex /data/local/tmp/vault-vector.json; exit $rc'
