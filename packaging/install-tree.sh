#!/usr/bin/env bash
# Stages the shared ncrs payload (service, CLI tools, file-browser adapters and,
# unless --skip-gui, the GUI tray app) into a package root. Called by
# scripts/build-deb.sh and by the RPM spec's %install, so the .deb and .rpm
# ship the identical tree.
#
# The repo root is derived from this script's own location, so the payload
# source paths resolve whether it runs from a git checkout or from an unpacked
# source root (RPM %install).
#
# Usage: packaging/install-tree.sh --dest DIR [OPTIONS]
#   --dest DIR          Package root to stage into (required)
#   --bin-dir DIR       Directory holding the built binaries (default: target/release)
#   --skip-gui          Do not stage the GUI tray app
#   --skip-dolphin      Do not warn when no Dolphin plugin is staged
#   --dolphin-stage DIR Staged Dolphin plugin tree to include (repeatable;
#                       default: every dist/dolphin/*/ that exists)
#   --require-dolphin   Fail instead of warning when no Dolphin plugin is staged
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

DEST=""
BIN_DIR="target/release"
SKIP_GUI=false
SKIP_DOLPHIN=false
REQUIRE_DOLPHIN=false
DOLPHIN_STAGES=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dest)            DEST="${2:?--dest requires a value}"; shift 2 ;;
        --bin-dir)         BIN_DIR="${2:?--bin-dir requires a value}"; shift 2 ;;
        --skip-gui)        SKIP_GUI=true; shift ;;
        --skip-dolphin)    SKIP_DOLPHIN=true; shift ;;
        --dolphin-stage)   DOLPHIN_STAGES+=("${2:?--dolphin-stage requires a value}"); shift 2 ;;
        --require-dolphin) REQUIRE_DOLPHIN=true; shift ;;
        -h|--help)         awk 'NR>1 && !/^#/{exit} NR>1{sub(/^# ?/,""); print}' "$0"; exit 0 ;;
        *) echo "Unknown option: $1 (see --help)" >&2; exit 2 ;;
    esac
done

[[ -n "$DEST" ]] || { echo "error: --dest is required" >&2; exit 2; }

# Resolve a caller-relative --dest before switching to the repo root, so a
# relative destination means the same thing from any working directory.
case "$DEST" in
    /*) ;;
    *) DEST="$PWD/$DEST" ;;
esac

# Refuse to rm -rf a dangerous destination later on: / or any single-component
# path (/usr, /tmp, ...). --dest is a throwaway package root, so a two-component
# minimum rules out the catastrophic cases without constraining real use.
case "$DEST" in
    /*/*) ;;
    *) echo "error: refusing unsafe --dest '$DEST' (need a path with at least two components)" >&2
       exit 2 ;;
esac

cd "$REPO_ROOT"

# ── Assemble staging tree ─────────────────────────────────────────────────────
echo "→ Assembling package tree..."
rm -rf "$DEST"
install -Dm755 "$BIN_DIR/ncrs"                                           "$DEST/usr/bin/ncrs"
install -Dm755 "$BIN_DIR/ncrs-open"                                      "$DEST/usr/bin/ncrs-open"
install -Dm755 "$BIN_DIR/ncrs-ctl"                                       "$DEST/usr/bin/ncrs-ctl"
install -Dm644 packaging/ncrs.service                                    "$DEST/usr/lib/systemd/user/ncrs.service"
install -Dm644 packaging/05-ncrs-quic.conf                               "$DEST/usr/lib/sysctl.d/05-ncrs-quic.conf"
install -Dm644 packaging/es.rgon.ncrs.Open.desktop                        "$DEST/usr/share/applications/es.rgon.ncrs.Open.desktop"
install -Dm644 shell_integration/file-managers/nautilus/syncstate.py      "$DEST/usr/share/nautilus-python/extensions/ncrs-syncstate.py"
install -Dm755 shell_integration/gnome-search/ncrs-search-provider       "$DEST/usr/bin/ncrs-search-provider"
install -Dm644 shell_integration/gnome-search/es.rgon.ncrs.SearchProvider.ini \
                                                                         "$DEST/usr/share/gnome-shell/search-providers/es.rgon.ncrs.SearchProvider.ini"
install -Dm644 shell_integration/gnome-search/es.rgon.ncrs.SearchProvider.desktop \
                                                                         "$DEST/usr/share/applications/es.rgon.ncrs.SearchProvider.desktop"
install -Dm644 packaging/es.rgon.ncrs.SearchProvider.service              "$DEST/usr/share/dbus-1/services/es.rgon.ncrs.SearchProvider.service"
install -Dm644 packaging/es.rgon.ncrs.metainfo.xml                        "$DEST/usr/share/metainfo/es.rgon.ncrs.metainfo.xml"
install -Dm755 shell_integration/thumbnailer/cr3-thumbnailer               "$DEST/usr/bin/cr3-thumbnailer"
install -Dm644 shell_integration/thumbnailer/cr3.thumbnailer               "$DEST/usr/share/thumbnailers/cr3.thumbnailer"

# Example config for provisioning, generated from the binary's built-in
# template. This executes the staged binary, so it must be runnable on the
# build host (cross-built packages need a matching host or qemu-user).
CONFIG_EXAMPLE="$DEST/usr/share/doc/ncrs/config.yaml.example"
mkdir -p "$DEST/usr/share/doc/ncrs"
install -Dm644 packaging/copyright "$DEST/usr/share/doc/ncrs/copyright"
# Generate to a temp file so a failing binary aborts without leaving a
# truncated config.yaml.example in the staging tree.
if ! "$BIN_DIR/ncrs" --print-default-config > "$CONFIG_EXAMPLE.tmp"; then
    rm -f "$CONFIG_EXAMPLE.tmp"
    echo "error: '$BIN_DIR/ncrs --print-default-config' failed (stale or non-host-arch binary?)" >&2
    exit 1
fi
chmod 644 "$CONFIG_EXAMPLE.tmp"
mv "$CONFIG_EXAMPLE.tmp" "$CONFIG_EXAMPLE"

if ! $SKIP_GUI; then
    install -Dm755 "$BIN_DIR/ncrs-gui"                                   "$DEST/usr/bin/ncrs-gui"
    # GNOME Software never reads the metainfo inside a local .deb: it takes
    # the shortest /usr/share/applications basename as the app id and looks
    # that up as a metainfo <id>. Keep this the shortest desktop file in the
    # package so it resolves to es.rgon.ncrs (screenshots, description).
    install -Dm644 packaging/es.rgon.ncrs.desktop                        "$DEST/usr/share/applications/es.rgon.ncrs.desktop"
    install -Dm644 packaging/es.rgon.ncrs.desktop                        "$DEST/etc/xdg/autostart/es.rgon.ncrs.desktop"
    install -Dm644 ncrs-gui/src-tauri/icons/32x32.png                    "$DEST/usr/share/icons/hicolor/32x32/apps/ncrs.png"
    install -Dm644 ncrs-gui/src-tauri/icons/64x64.png                    "$DEST/usr/share/icons/hicolor/64x64/apps/ncrs.png"
    install -Dm644 ncrs-gui/src-tauri/icons/128x128.png                  "$DEST/usr/share/icons/hicolor/128x128/apps/ncrs.png"
    install -Dm644 "ncrs-gui/src-tauri/icons/128x128@2x.png"             "$DEST/usr/share/icons/hicolor/256x256/apps/ncrs.png"
    install -Dm644 ncrs-gui/src-tauri/icons/icon.png                     "$DEST/usr/share/icons/hicolor/512x512/apps/ncrs.png"
fi

# ── Dolphin plugin (KF5 and/or KF6 builds, staged by build-dolphin-plugin.sh) ─
# Callers normally pass explicit --dolphin-stage arguments; when invoked
# directly with none, auto-detect every staged tree.
if [[ ${#DOLPHIN_STAGES[@]} -eq 0 ]] && ! $SKIP_DOLPHIN; then
    for d in dist/dolphin/*/; do [[ -d "$d" ]] && DOLPHIN_STAGES+=("$d"); done
fi
if [[ ${#DOLPHIN_STAGES[@]} -gt 0 ]]; then
    for stage in "${DOLPHIN_STAGES[@]}"; do
        [[ -d "$stage" ]] || { echo "error: --dolphin-stage $stage is not a directory" >&2; exit 1; }
        cp -a "$stage/." "$DEST/"
    done
fi
if ! $SKIP_DOLPHIN; then
    if [[ -z "$(find "$DEST" -path '*/overlayicon/ncrsoverlayplugin.so' -print -quit)" ]]; then
        $REQUIRE_DOLPHIN && { echo "error: no Dolphin plugin staged (run scripts/build-dolphin-plugin.sh)" >&2; exit 1; }
        echo "  warning: no Dolphin plugin staged; the package will have no Dolphin emblems"
    else
        echo "  Dolphin plugin: $(find "$DEST" -path '*/overlayicon/ncrsoverlayplugin.so' -printf '%P ')"
    fi
fi
