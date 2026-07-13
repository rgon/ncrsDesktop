#!/usr/bin/env bash
# Install the ncRS thumbnailers for the current user (no sudo required).
#
# Dependencies: libimage-exiftool-perl, python3-gi, gir1.2-gdkpixbuf-2.0, evince
#   sudo apt install libimage-exiftool-perl python3-gi gir1.2-gdkpixbuf-2.0 evince
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"

BIN_DIR="$HOME/.local/bin"
THUMBNAILER_DIR="$HOME/.local/share/thumbnailers"
mkdir -p "$BIN_DIR" "$THUMBNAILER_DIR"

install -Dm755 "$DIR/cr3-thumbnailer"   "$BIN_DIR/cr3-thumbnailer"
install -Dm644 "$DIR/cr3.thumbnailer"   "$THUMBNAILER_DIR/cr3.thumbnailer"
sed -i "s|/usr/bin/cr3-thumbnailer|$BIN_DIR/cr3-thumbnailer|g" \
    "$THUMBNAILER_DIR/cr3.thumbnailer"

install -Dm755 "$DIR/ncrs-thumbnailer"  "$BIN_DIR/ncrs-thumbnailer"
install -Dm644 "$DIR/ncrs.thumbnailer"  "$THUMBNAILER_DIR/ncrs.thumbnailer"
sed -i "s|/usr/bin/ncrs-thumbnailer|$BIN_DIR/ncrs-thumbnailer|g" \
    "$THUMBNAILER_DIR/ncrs.thumbnailer"

echo "Installed to $BIN_DIR/cr3-thumbnailer"
echo "Installed to $THUMBNAILER_DIR/cr3.thumbnailer"
echo "Installed to $BIN_DIR/ncrs-thumbnailer"
echo "Installed to $THUMBNAILER_DIR/ncrs.thumbnailer"
echo "Run 'nautilus -q' to restart Nautilus and pick up the new thumbnailers."
