#!/usr/bin/env bash
# Builds the Dolphin overlay plugin + ServiceMenu and installs them into a
# staging tree that scripts/build-deb.sh folds into the single ncrs .deb.
#
# Usage: ./scripts/build-dolphin-plugin.sh [OPTIONS]
#   --kf5               Build against Qt5 / KF5 (default: Qt6 / KF6)
#   --dest DIR          Staging root to install into (default: dist/dolphin/kf6 or kf5)
#   --build-dir DIR     CMake build directory to use and keep (default: a temp dir)
#   --test              Also build and run the unit tests before installing
#
# Needs cmake, a C++ compiler, extra-cmake-modules and the Qt/KF development
# packages (qt6-base-dev libkf6kio-dev libkf6coreaddons-dev, or qtbase5-dev
# libkf5kio-dev libkf5coreaddons-dev). The KF5 and KF6 plugins install to
# different Qt plugin dirs, so both can be staged and shipped side by side.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"
SRC_DIR="$REPO_ROOT/shell_integration/file-managers/dolphin"

# ── Parse arguments ───────────────────────────────────────────────────────────
QT_MAJOR=6
DEST=""
BUILD_DIR=""
RUN_TESTS=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --kf5)        QT_MAJOR=5; shift ;;
        --dest)       DEST="$2"; shift 2 ;;
        --build-dir)  BUILD_DIR="$2"; shift 2 ;;
        --test)       RUN_TESTS=true; shift ;;
        -h|--help)    awk 'NR>1 && !/^#/{exit} NR>1{sub(/^# ?/,""); print}' "$0"; exit 0 ;;
        *) echo "Unknown option: $1 (see --help)" >&2; exit 2 ;;
    esac
done

[[ -n "$DEST" ]] || DEST="dist/dolphin/kf${QT_MAJOR}"
rm -rf "$DEST"
mkdir -p "$DEST"
DEST="$(cd "$DEST" && pwd)"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT
[[ -n "$BUILD_DIR" ]] || BUILD_DIR="$WORK_DIR/build"

echo "Building the Dolphin plugin (Qt${QT_MAJOR}/KF${QT_MAJOR}) → ${DEST}"

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

DESTDIR="$DEST" cmake --install "$BUILD_DIR" --strip >/dev/null
PLUGIN="$(find "$DEST" -path "*/kf${QT_MAJOR}/overlayicon/ncrsoverlayplugin.so" -print -quit)"
[[ -n "$PLUGIN" ]] || { echo "error: plugin not found under $DEST" >&2; exit 1; }
echo "✓ ${PLUGIN#"$DEST"}"
