#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
profile="${VASYA_BUILD_PROFILE:-release}"
if [[ "$profile" != release && "$profile" != dev ]]; then echo 'VASYA_BUILD_PROFILE must be release or dev' >&2; exit 1; fi
cargo build --profile "$profile" -p vasyaapp-gpui -p vasyaapp-iced
cargo build --manifest-path sidecars/stt-sidecar/Cargo.toml --release
mkdir -p target/native-tools dist
swiftc -O native/macos/Capture.swift -o target/native-tools/vasya-capture
version="$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "vasya-native"))')"
minimum_os="$(sw_vers -productVersion | cut -d. -f1).0"
binprofile="$profile"
[[ "$profile" != dev ]] || binprofile=debug
for gui in gpui iced; do
    if [[ "$gui" == gpui ]]; then name='Vasya GPUI'; else name='Vasya Iced'; fi
    app="dist/$name.app"
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    cp "target/$binprofile/vasyaapp-$gui" "$app/Contents/MacOS/vasyaapp-$gui"
    cp native/macos/icon.icns "$app/Contents/Resources/icon.icns"
    cp target/native-tools/vasya-capture "$app/Contents/Resources/vasya-capture"
    cp sidecars/stt-sidecar/target/release/stt-sidecar "$app/Contents/Resources/stt-sidecar"
    python3 scripts/bundle-ffmpeg.py "$app/Contents/Resources"
    cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleName</key><string>$name</string>
<key>CFBundleDisplayName</key><string>$name</string>
<key>CFBundleIdentifier</key><string>cc.marketmaker.vasya.$gui</string>
<key>CFBundleExecutable</key><string>vasyaapp-$gui</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>$version</string>
<key>CFBundleVersion</key><string>$version</string>
<key>LSMinimumSystemVersion</key><string>$minimum_os</string>
<key>CFBundleIconFile</key><string>icon.icns</string>
<key>NSHighResolutionCapable</key><true/>
<key>NSMicrophoneUsageDescription</key><string>Record voice messages you choose to send.</string>
<key>NSCameraUsageDescription</key><string>Take photos you choose to send.</string>
</dict></plist>
PLIST
    codesign --force --deep --sign - "$app"
    codesign --verify --deep --strict "$app"
    /usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' "$app/Contents/Info.plist"
done
