#!/usr/bin/env bash
# Builds the Dolphin overlay plugin + ServiceMenu and assembles ncrs-dolphin.deb.
#
# Usage: ./scripts/build-deb-dolphin.sh [OPTIONS]
#   --kf5               Build against Qt5 / KF5 (default: Qt6 / KF6)
#   --version VERSION   Package version (default: version.txt at the repo root)
#   --arch ARCH         Debian architecture (default: dpkg --print-architecture)
#   --out-dir DIR       Output directory for the .deb (default: dist)
#   --build-dir DIR     CMake build directory to use and keep (default: a temp dir)
#   --test              Also build and run the unit tests before packaging
#
# Needs cmake, a C++ compiler, extra-cmake-modules, the Qt/KF development
# packages (qt6-base-dev libkf6kio-dev libkf6coreaddons-dev, or the qt5/kf5
# equivalents) and dpkg-dev for dpkg-shlibdeps.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"
SRC_DIR="$REPO_ROOT/shell_integration/file-managers/dolphin"

# ── Parse arguments ───────────────────────────────────────────────────────────
QT_MAJOR=6
VERSION=""
ARCH=""
OUT_DIR="dist"
BUILD_DIR=""
RUN_TESTS=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --kf5)        QT_MAJOR=5; shift ;;
        --version)    VERSION="$2"; shift 2 ;;
        --arch)       ARCH="$2"; shift 2 ;;
        --out-dir)    OUT_DIR="$2"; shift 2 ;;
        --build-dir)  BUILD_DIR="$2"; shift 2 ;;
        --test)       RUN_TESTS=true; shift ;;
        -h|--help)    awk 'NR>1 && !/^#/{exit} NR>1{sub(/^# ?/,""); print}' "$0"; exit 0 ;;
        *) echo "Unknown option: $1 (see --help)" >&2; exit 2 ;;
    esac
done

[[ -n "$VERSION" ]] || VERSION="$(tr -d '[:space:]' < version.txt)"
[[ -n "$ARCH" ]] || ARCH="$(dpkg --print-architecture 2>/dev/null || uname -m | sed 's/x86_64/amd64/')"
command -v dpkg-shlibdeps >/dev/null || { echo "error: dpkg-shlibdeps not found (install dpkg-dev)" >&2; exit 1; }

mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT
[[ -n "$BUILD_DIR" ]] || BUILD_DIR="$WORK_DIR/build"
PKG_DIR="$WORK_DIR/ncrs-dolphin_${VERSION}_${ARCH}"

echo "Building ncrs-dolphin ${VERSION} (${ARCH}, Qt${QT_MAJOR}/KF${QT_MAJOR}) → ${OUT_DIR}"

# ── Build ─────────────────────────────────────────────────────────────────────
echo "→ Configuring..."
cmake -S "$SRC_DIR" -B "$BUILD_DIR" \
    -DQT_MAJOR_VERSION="$QT_MAJOR" \
    -DCMAKE_INSTALL_PREFIX=/usr \
    -DCMAKE_BUILD_TYPE=Release \
    -DBUILD_TESTING="$($RUN_TESTS && echo ON || echo OFF)" >/dev/null

echo "→ Building..."
cmake --build "$BUILD_DIR" -j"$(nproc)"

if $RUN_TESTS; then
    echo "→ Running tests..."
    (cd "$BUILD_DIR" && QT_QPA_PLATFORM=offscreen ctest --output-on-failure)
fi

# ── Assemble staging tree ─────────────────────────────────────────────────────
DESTDIR="$PKG_DIR" cmake --install "$BUILD_DIR" --strip >/dev/null
PLUGIN="$(find "$PKG_DIR" -path "*/kf${QT_MAJOR}/overlayicon/ncrsoverlayplugin.so" -print -quit)"
[[ -n "$PLUGIN" ]] || { echo "error: plugin not found under $PKG_DIR" >&2; exit 1; }
install -Dm644 "$SRC_DIR/README.md" "$PKG_DIR/usr/share/doc/ncrs-dolphin/README.md"

# ── Write DEBIAN/control ──────────────────────────────────────────────────────
# dpkg-shlibdeps wants a debian/control next to it; a stub is enough.
mkdir -p "$WORK_DIR/shlibs/debian"
printf 'Source: ncrs-dolphin\n\nPackage: ncrs-dolphin\nArchitecture: any\n' > "$WORK_DIR/shlibs/debian/control"
SHLIBS="$(cd "$WORK_DIR/shlibs" && dpkg-shlibdeps -O -e"$PLUGIN" 2>/dev/null | sed -n 's/^shlibs:Depends=//p')"
[[ -n "$SHLIBS" ]] || { echo "error: dpkg-shlibdeps found no dependencies" >&2; exit 1; }
# ncrs provides the IPC socket and ncrs-ctl, which the ServiceMenu calls.
DEPENDS="ncrs (>= ${VERSION}), ${SHLIBS}"

mkdir -p "$PKG_DIR/DEBIAN"
cat > "$PKG_DIR/DEBIAN/control" <<EOF
Package: ncrs-dolphin
Version: ${VERSION}
Architecture: ${ARCH}
Maintainer: Gonzalo Ruiz <gonza@logo.cl>
Depends: ${DEPENDS}
Recommends: dolphin
Enhances: dolphin
Section: kde
Priority: optional
Homepage: https://github.com/rgon/ncrsDesktop
Description: Dolphin integration for the ncrs Nextcloud client
 Sync-status and sharing emblems for files under the ncrs Nextcloud mount in
 Dolphin (KF${QT_MAJOR} overlay-icon plugin), plus context-menu actions to keep
 files on this device, free up space and open them in Nextcloud Web.
 Restart Dolphin after installing (kquitapp${QT_MAJOR} dolphin).
EOF

# ── Build the .deb ────────────────────────────────────────────────────────────
DEB_PATH="$OUT_DIR/ncrs-dolphin_${VERSION}_${ARCH}.deb"
dpkg-deb --build --root-owner-group "$PKG_DIR" "$DEB_PATH" >/dev/null
echo "✓ $DEB_PATH"
