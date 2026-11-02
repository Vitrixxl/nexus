#!/bin/sh
# Builds the APK with the SDK's build tools alone: no Gradle, no dependencies.
set -eu
cd "$(dirname "$0")"
JAVA_HOME=${JAVA_HOME:-$(ls -d "$HOME"/.local/share/jdk/jdk-* | head -1)}
export PATH="$JAVA_HOME/bin:$PATH"
sdk=${ANDROID_HOME:-$HOME/Android/Sdk}
tools=$sdk/build-tools/$(ls "$sdk/build-tools" | sort -V | tail -1)
jar=$sdk/platforms/$(ls "$sdk/platforms" | sort -V | tail -1)/android.jar
out=${1:-$HOME/.local/share/nexus/nexus-phone.apk}
# A local signing key, outside the repository; updates must keep using it.
keystore=$HOME/.local/share/nexus/phone.keystore
rm -rf build
mkdir -p build/classes "$(dirname "$out")"
javac -Xlint:-options --release 17 -classpath "$jar" -d build/classes $(find src -name '*.java')
"$tools/d8" --release --min-api 26 --lib "$jar" --output build $(find build/classes -name '*.class')
"$tools/aapt2" compile --dir res -o build/res.zip
"$tools/aapt2" link -o build/base.apk -I "$jar" --manifest AndroidManifest.xml build/res.zip \
    --min-sdk-version 26 --target-sdk-version 35
jar -u -M -f build/base.apk -C build classes.dex
"$tools/zipalign" -f -p 4 build/base.apk build/aligned.apk
[ -f "$keystore" ] || keytool -genkeypair -keystore "$keystore" -storepass nexusphone \
    -alias nexus -keyalg RSA -keysize 2048 -validity 10000 -dname CN=Nexus
"$tools/apksigner" sign --ks "$keystore" --ks-pass pass:nexusphone --out "$out" build/aligned.apk
echo "$out: $(wc -c <"$out") bytes"
