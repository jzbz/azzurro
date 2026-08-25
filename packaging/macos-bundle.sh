#!/usr/bin/env bash
# Lay out Azzurro.app around a universal binary.
#
# A .app is a directory with a particular shape and a plist that names the
# executable; nothing here needs Xcode. Signing and notarisation are not done —
# see .github/workflows/package.yml for why — so a Mac will refuse this until
# the user allows it in Privacy & Security, or until someone with a Developer
# ID signs it.
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

# The icon is the same artwork the Linux desktop entry uses, at the largest
# size we render. A real .icns would carry every size; this is legible and
# costs no new tooling.
cp "$root/crates/azzurro-gui/desktop/blue.azzurro.Azzurro-256.png" \
   "$app/Contents/Resources/azzurro.png"

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
  <key>CFBundleIconFile</key><string>azzurro.png</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <!-- Discovery is a UDP broadcast to every interface, which macOS treats as
       reaching out to devices on the local network and gates behind a prompt.
       Without this the prompt has no wording and the request is refused. -->
  <key>NSLocalNetworkUsageDescription</key>
  <string>Azzurro looks for BluOS players on your network.</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

echo "built $app"
