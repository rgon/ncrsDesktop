#!/usr/bin/env bash
# Install the CR3/CR2 thumbnailer for the current user (no sudo required).
#
# Dependencies: libimage-exiftool-perl, python3-gi, gir1.2-gdkpixbuf-2.0
#   sudo apt install libimage-exiftool-perl python3-gi gir1.2-gdkpixbuf-2.0
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"

BIN_DIR="$HOME/.local/bin"
THUMBNAILER_DIR="$HOME/.local/share/thumbnailers"
mkdir -p "$BIN_DIR" "$THUMBNAILER_DIR"

install -Dm755 "$DIR/cr3-thumbnailer"  "$BIN_DIR/cr3-thumbnailer"
install -Dm644 "$DIR/cr3.thumbnailer"  "$THUMBNAILER_DIR/cr3.thumbnailer"

# Patch the Exec line to point at the user-local binary
sed -i "s|/usr/bin/cr3-thumbnailer|$BIN_DIR/cr3-thumbnailer|g" \
    "$THUMBNAILER_DIR/cr3.thumbnailer"

echo "Installed to $BIN_DIR/cr3-thumbnailer"
echo "Installed to $THUMBNAILER_DIR/cr3.thumbnailer"
echo "Run 'nautilus -q' to restart Nautilus and pick up the new thumbnailer."
