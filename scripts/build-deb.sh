#!/usr/bin/env bash
# Builds release binaries and assembles a .deb package.
# Usage: ./scripts/build-deb.sh [--skip-gui]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

# ── Resolve version ───────────────────────────────────────────────────────────
VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"
ARCH="$(dpkg --print-architecture 2>/dev/null || uname -m | sed 's/x86_64/amd64/')"
PKG_DIR="dist/ncrs_${VERSION}_${ARCH}"

echo "Building ncrs ${VERSION} (${ARCH})"

# ── Build core binaries ───────────────────────────────────────────────────────
echo "→ Building core binaries..."
cargo build --release -p ncrs_core

# ── Build GUI (unless --skip-gui) ─────────────────────────────────────────────
SKIP_GUI=false
for arg in "$@"; do [[ "$arg" == "--skip-gui" ]] && SKIP_GUI=true; done

if ! $SKIP_GUI; then
    echo "→ Building GUI..."
    if ! command -v pnpm >/dev/null 2>&1; then
        echo "  pnpm not found; install it with: npm i -g pnpm"
        echo "  Skipping GUI build."
        SKIP_GUI=true
    else
        cd ncrs-gui
        pnpm install --frozen-lockfile
        pnpm build
        cd ..
        cargo build --release -p ncrs-gui
    fi
fi

# ── Assemble staging tree ─────────────────────────────────────────────────────
echo "→ Assembling package tree..."
rm -rf "$PKG_DIR"
install -Dm755 target/release/ncrs                                       "$PKG_DIR/usr/bin/ncrs"
install -Dm755 target/release/ncrs-open                                  "$PKG_DIR/usr/bin/ncrs-open"
install -Dm644 packaging/ncrs.service                                    "$PKG_DIR/usr/lib/systemd/user/ncrs.service"
install -Dm644 packaging/ncrs-open.desktop                               "$PKG_DIR/usr/share/applications/ncrs-open.desktop"
install -Dm644 shell_integration/nautilus/syncstate.py                   "$PKG_DIR/usr/share/nautilus-python/extensions/ncrs-syncstate.py"
install -Dm755 shell_integration/gnome-search/ncrs-search-provider       "$PKG_DIR/usr/bin/ncrs-search-provider"
install -Dm644 shell_integration/gnome-search/es.rgon.ncrs.SearchProvider.ini \
                                                                         "$PKG_DIR/usr/share/gnome-shell/search-providers/es.rgon.ncrs.SearchProvider.ini"
install -Dm644 shell_integration/gnome-search/es.rgon.ncrs.desktop       "$PKG_DIR/usr/share/applications/es.rgon.ncrs.desktop"
install -Dm644 packaging/es.rgon.ncrs.SearchProvider.service              "$PKG_DIR/usr/share/dbus-1/services/es.rgon.ncrs.SearchProvider.service"

if ! $SKIP_GUI; then
    install -Dm755 target/release/ncrs-gui                               "$PKG_DIR/usr/bin/ncrs-gui"
fi

# ── Write DEBIAN/control ──────────────────────────────────────────────────────
DEPENDS="fuse3, python3-nautilus | gir1.2-nautilus-3.0"
if ! $SKIP_GUI; then
    DEPENDS="$DEPENDS, libwebkit2gtk-4.1-0 | libwebkit2gtk-4.0-37"
fi

mkdir -p "$PKG_DIR/DEBIAN"
cat > "$PKG_DIR/DEBIAN/control" <<EOF
Package: ncrs
Version: ${VERSION}
Architecture: ${ARCH}
Maintainer: Gonzalo Ruiz <gonza@logo.cl>
Depends: ${DEPENDS}
Section: net
Priority: optional
Description: Nextcloud FUSE virtual filesystem client
 ncrs mounts your Nextcloud as a local FUSE filesystem with offline
 caching, real-time sync, conflict detection, and GNOME/Nautilus
 integration including a GNOME Shell search provider.
EOF

# ── Copy maintainer scripts ───────────────────────────────────────────────────
for script in postinst prerm postrm; do
    src="packaging/maintainer-scripts/$script"
    if [[ -f "$src" ]]; then
        install -Dm755 "$src" "$PKG_DIR/DEBIAN/$script"
    fi
done

# ── Build the .deb ────────────────────────────────────────────────────────────
mkdir -p dist
DEB_PATH="dist/ncrs_${VERSION}_${ARCH}.deb"
dpkg-deb --build --root-owner-group "$PKG_DIR" "$DEB_PATH"
echo ""
echo "✓ Built: $DEB_PATH"
echo "  Install with: sudo apt install ./$DEB_PATH"
echo "  Then enable:  systemctl --user enable --now ncrs.service"
