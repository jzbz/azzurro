#!/usr/bin/env bash
# Lay out Azzurro.app around a universal binary.
#
# A .app is a directory with a particular shape and a plist that names the
# executable; nothing here needs Xcode. The bundle is signed ad hoc at the end
# and no more than that: Developer ID signing and notarization are done by
# packaging/macos-sign.sh, on the machine holding the key, and it replaces the
# ad hoc signature outright. `release.yml` calls this script and ships what it
# makes as the `-unsigned` zip — not Developer ID signed, which is what
# Gatekeeper asks of a download, so still refused by it.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
app="$root/dist/Azzurro.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

# One binary carrying both architectures. `lipo` ships with the toolchain on
# every Mac, including the CI runners.
lipo -create \
    "$root/target/aarch64-apple-darwin/release/azzurro" \
    "$root/target/x86_64-apple-darwin/release/azzurro" \
    -output "$app/Contents/MacOS/azzurro"
chmod +x "$app/Contents/MacOS/azzurro"

# A real .icns, built from the sizes packaging/icons.sh renders out of the SVG.
#
# A lone PNG is what this did first, and a Finder that wants an .icns simply
# falls back to a generic document icon for anything else. Nothing is scaled
# here: every size is rendered from the vector on a machine that has an SVG
# renderer, so the 1024 is as sharp as the 16 rather than a blown-up 256.
#
# `iconutil` ships with macOS and exists nowhere else, which is why this step
# lives in the bundle script rather than beside icons.sh.
iconset="$root/dist/azzurro.iconset"
rm -rf "$iconset"
mkdir -p "$iconset"

# The names are Apple's, and iconutil refuses anything it does not recognize.
# A size appears twice wherever it serves both a scale-1 slot and the retina
# slot of the size below it.
cp "$root/crates/azzurro-gui/desktop/blue.azzurro.Azzurro-16.png"   "$iconset/icon_16x16.png"
cp "$root/crates/azzurro-gui/desktop/blue.azzurro.Azzurro-32.png"   "$iconset/icon_16x16@2x.png"
cp "$root/crates/azzurro-gui/desktop/blue.azzurro.Azzurro-32.png"   "$iconset/icon_32x32.png"
cp "$root/crates/azzurro-gui/desktop/blue.azzurro.Azzurro-64.png"   "$iconset/icon_32x32@2x.png"
cp "$root/crates/azzurro-gui/desktop/blue.azzurro.Azzurro-128.png"  "$iconset/icon_128x128.png"
cp "$root/crates/azzurro-gui/desktop/blue.azzurro.Azzurro-256.png"  "$iconset/icon_128x128@2x.png"
cp "$root/crates/azzurro-gui/desktop/blue.azzurro.Azzurro-256.png"  "$iconset/icon_256x256.png"
cp "$root/crates/azzurro-gui/desktop/blue.azzurro.Azzurro-512.png"  "$iconset/icon_256x256@2x.png"
cp "$root/crates/azzurro-gui/desktop/blue.azzurro.Azzurro-512.png"  "$iconset/icon_512x512.png"
cp "$root/crates/azzurro-gui/desktop/blue.azzurro.Azzurro-1024.png" "$iconset/icon_512x512@2x.png"

iconutil --convert icns "$iconset" --output "$app/Contents/Resources/azzurro.icns"
rm -rf "$iconset"

version="$(grep -m1 '^version' "$root/Cargo.toml" | cut -d'"' -f2)"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Azzurro</string>
  <key>CFBundleDisplayName</key><string>Azzurro</string>
  <key>CFBundleIdentifier</key><string>blue.azzurro.Azzurro</string>
  <key>CFBundleVersion</key><string>${version}</string>
  <key>CFBundleShortVersionString</key><string>${version}</string>
  <key>CFBundleExecutable</key><string>azzurro</string>
  <!-- Named without its extension, which is what Finder expects. -->
  <key>CFBundleIconFile</key><string>azzurro</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <!-- Discovery is a UDP broadcast to every interface, and every player is on
       the local network, which macOS gates behind a prompt. This is what the
       prompt says. Without it macOS 26 still asks, in its own words, that the
       app be let "discover, connect to, and collect data from devices on
       your networks": the request is not refused for the want of it, but
       the question arrives without the reason. -->
  <key>NSLocalNetworkUsageDescription</key>
  <string>Azzurro looks for BluOS players on your network.</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# Signed ad hoc — no identity, no team, no key — so the bundle has a code
# identity at all. Without one, macOS 26 records Local Network access as
# allowed for it and goes on refusing every connection with "No route to
# host": a CI build run on a test Mac never reached a player until it was
# signed this way, and did at once afterwards. It proves nothing about who
# built it, and macos-sign.sh's `--force` discards it for the Developer ID
# signature. No `--deep`, for the reason macos-sign.sh gives: there is no
# nested code here, and `--deep` is deprecated for signing.
codesign --force --sign - "$app"

echo "built $app"
