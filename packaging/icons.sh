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
# 24 and 48 are Windows's: it asks for those two in places nothing else does.
for size in 16 24 32 48 64 128 256 512 1024; do
    rsvg-convert -w "$size" -h "$size" blue.azzurro.Azzurro.svg \
        -o "blue.azzurro.Azzurro-$size.png"
done
echo "rendered 16 to 1024 px icons from blue.azzurro.Azzurro.svg"

# And the .ico that goes inside the Windows executable. Windows chooses a size
# per context — 16 in the title bar, 32 in Alt-Tab, 256 on the desktop — so all
# of them go in rather than one to be scaled. The payloads are the PNGs
# themselves, which Windows has read since Vista and which keeps the file about
# a seventh the size of the bitmap form.
python3 - <<'ICO'
import struct
sizes = [16, 24, 32, 48, 64, 128, 256]
imgs = [(s, open(f"blue.azzurro.Azzurro-{s}.png", "rb").read()) for s in sizes]
head = struct.pack("<HHH", 0, 1, len(imgs))
offset = 6 + 16 * len(imgs)
entries, blobs = b"", b""
for s, blob in imgs:
    # Width and height are one byte each, so 256 is written as zero.
    entries += struct.pack("<BBBBHHII", s % 256, s % 256, 0, 0, 1, 32,
                           len(blob), offset + len(blobs))
    blobs += blob
open("blue.azzurro.Azzurro.ico", "wb").write(head + entries + blobs)
ICO
echo "packed blue.azzurro.Azzurro.ico with $(echo 16 24 32 48 64 128 256 | wc -w) sizes"
