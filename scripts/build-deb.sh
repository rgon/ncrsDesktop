#!/usr/bin/env bash
# Builds release binaries and assembles a .deb package.
#
# Usage: ./scripts/build-deb.sh [OPTIONS]
#   --version VERSION   Package version (default: workspace version in Cargo.toml)
#   --arch ARCH         Debian architecture (default: dpkg --print-architecture)
#   --out-dir DIR       Output directory for the .deb (default: dist)
#   --skip-gui          Do not build/package the GUI tray app
#   --skip-build        Assemble only; expect binaries already in target/release
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

# ── Parse arguments ───────────────────────────────────────────────────────────
VERSION=""
ARCH=""
OUT_DIR="dist"
SKIP_GUI=false
SKIP_BUILD=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version)    VERSION="$2"; shift 2 ;;
        --arch)       ARCH="$2"; shift 2 ;;
        --out-dir)    OUT_DIR="$2"; shift 2 ;;
        --skip-gui)   SKIP_GUI=true; shift ;;
        --skip-build) SKIP_BUILD=true; shift ;;
        -h|--help)    awk 'NR>1 && !/^#/{exit} NR>1{sub(/^# ?/,""); print}' "$0"; exit 0 ;;
        *) echo "Unknown option: $1 (see --help)" >&2; exit 2 ;;
    esac
done

[[ -n "$VERSION" ]] || VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"
[[ -n "$ARCH" ]] || ARCH="$(dpkg --print-architecture 2>/dev/null || uname -m | sed 's/x86_64/amd64/')"
PKG_DIR="$OUT_DIR/ncrs_${VERSION}_${ARCH}"

echo "Building ncrs ${VERSION} (${ARCH}) → ${OUT_DIR}"

# ── Build core binaries ───────────────────────────────────────────────────────
if ! $SKIP_BUILD; then
    echo "→ Building core binaries..."
    cargo build --release -p ncrs_core
fi

# ── Build GUI (unless --skip-gui) ─────────────────────────────────────────────
if ! $SKIP_GUI && ! $SKIP_BUILD; then
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
        # custom-protocol embeds the frontend; without it the binary expects
        # the vite dev server at devUrl (works on dev machines only).
        cargo build --release -p ncrs-gui --features custom-protocol
    fi
fi

# With --skip-build the staged binaries must already exist; fail fast instead
# of dying mid-assembly (or silently shipping a GUI-less package).
if $SKIP_BUILD; then
    for bin in ncrs ncrs-open; do
        [[ -x "target/release/$bin" ]] || { echo "error: --skip-build set but target/release/$bin is missing" >&2; exit 1; }
    done
    if ! $SKIP_GUI && [[ ! -x target/release/ncrs-gui ]]; then
        echo "error: --skip-build set but target/release/ncrs-gui is missing (pass --skip-gui to package without the GUI)" >&2
        exit 1
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

# Example config for provisioning, generated from the binary's built-in
# template. This executes the staged binary, so it must be runnable on the
# build host (cross-built packages need a matching host or qemu-user).
mkdir -p "$PKG_DIR/usr/share/doc/ncrs"
if ! target/release/ncrs --print-default-config > "$PKG_DIR/usr/share/doc/ncrs/config.yaml.example"; then
    echo "error: 'target/release/ncrs --print-default-config' failed (stale or non-host-arch binary?)" >&2
    exit 1
fi
chmod 644 "$PKG_DIR/usr/share/doc/ncrs/config.yaml.example"

if ! $SKIP_GUI; then
    install -Dm755 target/release/ncrs-gui                               "$PKG_DIR/usr/bin/ncrs-gui"
    install -Dm644 packaging/ncrs-gui.desktop                            "$PKG_DIR/usr/share/applications/ncrs-gui.desktop"
    install -Dm644 packaging/ncrs-gui.desktop                            "$PKG_DIR/etc/xdg/autostart/ncrs-gui.desktop"
    install -Dm644 ncrs-gui/src-tauri/icons/32x32.png                    "$PKG_DIR/usr/share/icons/hicolor/32x32/apps/ncrs.png"
    install -Dm644 ncrs-gui/src-tauri/icons/128x128.png                  "$PKG_DIR/usr/share/icons/hicolor/128x128/apps/ncrs.png"
    install -Dm644 "ncrs-gui/src-tauri/icons/128x128@2x.png"             "$PKG_DIR/usr/share/icons/hicolor/256x256/apps/ncrs.png"
    install -Dm644 ncrs-gui/src-tauri/icons/icon.png                     "$PKG_DIR/usr/share/icons/hicolor/512x512/apps/ncrs.png"
fi

# ── Write DEBIAN/control ──────────────────────────────────────────────────────
# ncrs links libssl at build time; ncrs-gui dlopens libayatana-appindicator3
# for the tray icon (invisible to ldd/shlibdeps) and panics without it.
DEPENDS="fuse3, python3-nautilus | gir1.2-nautilus-3.0, libssl3t64 | libssl3"
if ! $SKIP_GUI; then
    DEPENDS="$DEPENDS, libwebkit2gtk-4.1-0 | libwebkit2gtk-4.0-37, libayatana-appindicator3-1 | libappindicator3-1"
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
mkdir -p "$OUT_DIR"
DEB_PATH="$OUT_DIR/ncrs_${VERSION}_${ARCH}.deb"
dpkg-deb --build --root-owner-group "$PKG_DIR" "$DEB_PATH"
echo ""
echo "✓ Built: $DEB_PATH"
echo "  Install with: sudo apt install ./$DEB_PATH"
echo "  The GUI tray app autostarts at login (/etc/xdg/autostart/ncrs-gui.desktop)."
echo "  Headless (no-GUI) alternative: systemctl --user enable --now ncrs.service"
