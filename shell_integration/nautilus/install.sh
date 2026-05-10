#!/usr/bin/env bash
set -euo pipefail

DEST="$HOME/.local/share/nautilus-python/extensions"
mkdir -p "$DEST"
cp "$(dirname "$0")/syncstate.py" "$DEST/"
echo "Installed to $DEST/syncstate.py"
echo "Run 'nautilus -q' to restart Nautilus and load the extension."
