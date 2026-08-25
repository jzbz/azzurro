#!/bin/sh
# Re-render the PNG icons from the SVG. Run after changing the SVG.
#
# The PNGs are committed rather than generated at build time because Flathub
# builds offline and its AppStream step needs an icon it can read without an
# SVG loader, which the freedesktop SDK does not have.
set -eu
cd "$(dirname "$0")/../crates/azzurro-gui/desktop"
# 64, 128 and 256 are what the desktop entry and AppStream want. The rest are
# for the macOS .icns, which wants every size from 16 to 1024 and gets them
# rendered from the SVG here rather than scaled up from a small PNG on the
# build runner: an icon upscaled from 256 to 1024 looks exactly as soft as it
# sounds, and the runner has no SVG renderer to do better.
for size in 16 32 64 128 256 512 1024; do
    rsvg-convert -w "$size" -h "$size" blue.azzurro.Azzurro.svg \
        -o "blue.azzurro.Azzurro-$size.png"
done
echo "rendered 16 to 1024 px icons from blue.azzurro.Azzurro.svg"
