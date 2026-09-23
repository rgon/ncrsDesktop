#!/usr/bin/env bash
set -euo pipefail

DEST="$HOME/.local/share/nautilus-python/extensions"
# The .deb package installs the same extension here. Loading BOTH copies in one
# Nautilus process registers the NcrsInfoProvider/NcrsColumnProvider GObject
# types twice — a name collision that makes the second copy fail to load and
# leaves an unpredictable (possibly stale) version active. Refuse to create the
# duplicate so a manual dev install can't shadow — or fight with — the package.
SYSTEM_COPY="/usr/share/nautilus-python/extensions/ncrs-syncstate.py"
if [ -e "$SYSTEM_COPY" ]; then
    echo "A packaged copy already exists at:" >&2
    echo "  $SYSTEM_COPY" >&2
    echo "Installing a second copy under ~/.local would collide with it." >&2
    echo "Remove the package copy first (sudo rm '$SYSTEM_COPY') or skip this" >&2
    echo "dev install and just edit the packaged file." >&2
    exit 1
fi

mkdir -p "$DEST"
cp "$(dirname "$0")/syncstate.py" "$DEST/"
echo "Installed to $DEST/syncstate.py"
echo "Run 'nautilus -q' to restart Nautilus and load the extension."
