#!/usr/bin/env bash
# Builds release binaries and assembles the ncrs .deb: the service, CLI tools,
# the GUI (unless --skip-gui) and the file-browser adapters (Nautilus
# extension, Dolphin plugin).
#
# The Dolphin plugin is C++ built against Qt/KF, so it is staged beforehand by
# scripts/build-dolphin-plugin.sh (once per KF major) and copied in from
# dist/dolphin/* or --dolphin-stage.
#
# Usage: ./scripts/build-deb.sh [OPTIONS]
#   --version VERSION   Package version (default: workspace version in Cargo.toml)
#   --arch ARCH         Debian architecture (default: dpkg --print-architecture)
#   --out-dir DIR       Output directory for the .deb (default: dist)
#   --skip-gui          Do not build/package the GUI tray app
#   --skip-build        Assemble only; expect binaries already in target/release
#   --dolphin-stage DIR Staged Dolphin plugin tree to include (repeatable;
#                       default: every dist/dolphin/*/ that exists)
#   --require-dolphin   Fail instead of warning when no Dolphin plugin is staged
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

# ── Parse arguments ───────────────────────────────────────────────────────────
VERSION=""
ARCH=""
OUT_DIR="dist"
SKIP_GUI=false
SKIP_BUILD=false
DOLPHIN_STAGES=()
REQUIRE_DOLPHIN=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version)    VERSION="$2"; shift 2 ;;
        --arch)       ARCH="$2"; shift 2 ;;
        --out-dir)    OUT_DIR="$2"; shift 2 ;;
        --skip-gui)   SKIP_GUI=true; shift ;;
        --skip-build) SKIP_BUILD=true; shift ;;
        --dolphin-stage)   DOLPHIN_STAGES+=("$2"); shift 2 ;;
        --require-dolphin) REQUIRE_DOLPHIN=true; shift ;;
        -h|--help)    awk 'NR>1 && !/^#/{exit} NR>1{sub(/^# ?/,""); print}' "$0"; exit 0 ;;
        *) echo "Unknown option: $1 (see --help)" >&2; exit 2 ;;
    esac
done

[[ -n "$VERSION" ]] || VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"
[[ -n "$ARCH" ]] || ARCH="$(dpkg --print-architecture 2>/dev/null || uname -m | sed 's/x86_64/amd64/')"
PKG_DIR="$OUT_DIR/ncrs_${VERSION}_${ARCH}"

echo "Building ncrs ${VERSION} (${ARCH}) → ${OUT_DIR}"

# ── Build GUI frontend (must precede cargo so Tauri can embed it) ─────────────
if ! $SKIP_GUI && ! $SKIP_BUILD; then
    echo "→ Building GUI frontend..."
    if ! command -v pnpm >/dev/null 2>&1; then
        echo "  pnpm not found; install it with: npm i -g pnpm"
        echo "  Skipping GUI build."
        SKIP_GUI=true
    else
        cd ncrs-gui
        pnpm install --frozen-lockfile
        pnpm build
        cd ..
    fi
fi

# ── Build Rust binaries (single invocation so shared deps compile once) ────────
if ! $SKIP_BUILD; then
    if $SKIP_GUI; then
        echo "→ Building core binaries..."
        cargo build --release -p ncrs_core
    else
        echo "→ Building core and GUI binaries..."
        # custom-protocol embeds the frontend; without it ncrs-gui expects
        # the vite dev server at devUrl (works on dev machines only).
        cargo build --release -p ncrs_core -p ncrs-gui --features ncrs-gui/custom-protocol
    fi
fi

# With --skip-build the staged binaries must already exist; fail fast instead
# of dying mid-assembly (or silently shipping a GUI-less package).
if $SKIP_BUILD; then
    for bin in ncrs ncrs-open ncrs-ctl; do
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
install -Dm755 target/release/ncrs-ctl                                   "$PKG_DIR/usr/bin/ncrs-ctl"
install -Dm644 packaging/ncrs.service                                    "$PKG_DIR/usr/lib/systemd/user/ncrs.service"
install -Dm644 packaging/05-ncrs-quic.conf                               "$PKG_DIR/usr/lib/sysctl.d/05-ncrs-quic.conf"
install -Dm644 packaging/es.rgon.ncrs.Open.desktop                        "$PKG_DIR/usr/share/applications/es.rgon.ncrs.Open.desktop"
install -Dm644 shell_integration/file-managers/nautilus/syncstate.py      "$PKG_DIR/usr/share/nautilus-python/extensions/ncrs-syncstate.py"
install -Dm755 shell_integration/gnome-search/ncrs-search-provider       "$PKG_DIR/usr/bin/ncrs-search-provider"
install -Dm644 shell_integration/gnome-search/es.rgon.ncrs.SearchProvider.ini \
                                                                         "$PKG_DIR/usr/share/gnome-shell/search-providers/es.rgon.ncrs.SearchProvider.ini"
install -Dm644 shell_integration/gnome-search/es.rgon.ncrs.SearchProvider.desktop \
                                                                         "$PKG_DIR/usr/share/applications/es.rgon.ncrs.SearchProvider.desktop"
install -Dm644 packaging/es.rgon.ncrs.SearchProvider.service              "$PKG_DIR/usr/share/dbus-1/services/es.rgon.ncrs.SearchProvider.service"
install -Dm644 packaging/es.rgon.ncrs.metainfo.xml                        "$PKG_DIR/usr/share/metainfo/es.rgon.ncrs.metainfo.xml"
install -Dm755 shell_integration/thumbnailer/cr3-thumbnailer               "$PKG_DIR/usr/bin/cr3-thumbnailer"
install -Dm644 shell_integration/thumbnailer/cr3.thumbnailer               "$PKG_DIR/usr/share/thumbnailers/cr3.thumbnailer"

# Example config for provisioning, generated from the binary's built-in
# template. This executes the staged binary, so it must be runnable on the
# build host (cross-built packages need a matching host or qemu-user).
mkdir -p "$PKG_DIR/usr/share/doc/ncrs"
install -Dm644 packaging/copyright "$PKG_DIR/usr/share/doc/ncrs/copyright"
if ! target/release/ncrs --print-default-config > "$PKG_DIR/usr/share/doc/ncrs/config.yaml.example"; then
    echo "error: 'target/release/ncrs --print-default-config' failed (stale or non-host-arch binary?)" >&2
    exit 1
fi
chmod 644 "$PKG_DIR/usr/share/doc/ncrs/config.yaml.example"

if ! $SKIP_GUI; then
    install -Dm755 target/release/ncrs-gui                               "$PKG_DIR/usr/bin/ncrs-gui"
    # GNOME Software never reads the metainfo inside a local .deb: it takes
    # the shortest /usr/share/applications basename as the app id and looks
    # that up as a metainfo <id>. Keep this the shortest desktop file in the
    # package so it resolves to es.rgon.ncrs (screenshots, description).
    install -Dm644 packaging/es.rgon.ncrs.desktop                        "$PKG_DIR/usr/share/applications/es.rgon.ncrs.desktop"
    install -Dm644 packaging/es.rgon.ncrs.desktop                        "$PKG_DIR/etc/xdg/autostart/es.rgon.ncrs.desktop"
    install -Dm644 ncrs-gui/src-tauri/icons/32x32.png                    "$PKG_DIR/usr/share/icons/hicolor/32x32/apps/ncrs.png"
    install -Dm644 ncrs-gui/src-tauri/icons/64x64.png                    "$PKG_DIR/usr/share/icons/hicolor/64x64/apps/ncrs.png"
    install -Dm644 ncrs-gui/src-tauri/icons/128x128.png                  "$PKG_DIR/usr/share/icons/hicolor/128x128/apps/ncrs.png"
    install -Dm644 "ncrs-gui/src-tauri/icons/128x128@2x.png"             "$PKG_DIR/usr/share/icons/hicolor/256x256/apps/ncrs.png"
    install -Dm644 ncrs-gui/src-tauri/icons/icon.png                     "$PKG_DIR/usr/share/icons/hicolor/512x512/apps/ncrs.png"
fi

# ── Dolphin plugin (KF5 and/or KF6 builds, staged by build-dolphin-plugin.sh) ─
if [[ ${#DOLPHIN_STAGES[@]} -eq 0 ]]; then
    for d in dist/dolphin/*/; do [[ -d "$d" ]] && DOLPHIN_STAGES+=("$d"); done
fi
for stage in ${DOLPHIN_STAGES[@]+"${DOLPHIN_STAGES[@]}"}; do
    [[ -d "$stage" ]] || { echo "error: --dolphin-stage $stage is not a directory" >&2; exit 1; }
    cp -a "$stage/." "$PKG_DIR/"
done
if [[ -z "$(find "$PKG_DIR" -path '*/overlayicon/ncrsoverlayplugin.so' -print -quit)" ]]; then
    $REQUIRE_DOLPHIN && { echo "error: no Dolphin plugin staged (run scripts/build-dolphin-plugin.sh)" >&2; exit 1; }
    echo "  warning: no Dolphin plugin staged; the package will have no Dolphin emblems"
else
    echo "  Dolphin plugin: $(find "$PKG_DIR" -path '*/overlayicon/ncrsoverlayplugin.so' -printf '%P ')"
fi

# ── Write DEBIAN/control ──────────────────────────────────────────────────────
# ncrs links libssl at build time; ncrs-gui dlopens libayatana-appindicator3
# for the tray icon (invisible to ldd/shlibdeps) and panics without it.
#
# Everything desktop-specific is a soft relationship, so one package suits
# every desktop without pulling another's stack (or upgrading its browser):
#   Recommends  the GNOME search provider / CR3 thumbnailer helpers, which
#               degrade gracefully without them
#   Suggests    python3-nautilus (the extension's loader). The service detects
#               Nautilus and the GUI offers the install; a dpkg trigger then
#               reloads Nautilus.
#   (nothing)   the Dolphin plugin's Qt/KF libraries: only Dolphin loads it,
#               and Dolphin brings them.
DEPENDS="fuse3, libssl3t64 | libssl3"
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
Recommends: libcap2-bin, python3-gi, gir1.2-gdkpixbuf-2.0, libimage-exiftool-perl
Suggests: python3-nautilus | gir1.2-nautilus-3.0
Enhances: nautilus, dolphin
Section: net
Priority: optional
Description: Nextcloud FUSE virtual filesystem client
 ncrs mounts your Nextcloud as a local FUSE filesystem with offline
 caching, real-time sync, conflict detection and a GNOME Shell search
 provider. File-browser integration (indexer exclusion, thumbnails,
 type detection) is applied per installed browser by the service, with
 sync emblems and menus in Nautilus and Dolphin.
 .
 libcap2-bin (setcap) is used at install time to grant the ncrs binary
 CAP_SYS_ADMIN, which enables zero-copy kernel read passthrough for
 fully-cached files on Linux 6.9+. Without it, ncrs runs identically but
 always falls back to normal buffered reads.
EOF

# ── dpkg triggers ─────────────────────────────────────────────────────────────
# Fire when python3-nautilus installs its loader later, so postinst can reload
# Nautilus. dpkg trigger paths are literal, hence the multiarch lookup.
MULTIARCH="$(dpkg-architecture -a"$ARCH" -qDEB_HOST_MULTIARCH 2>/dev/null || true)"
if [[ -z "$MULTIARCH" ]]; then
    case "$ARCH" in
        amd64) MULTIARCH=x86_64-linux-gnu ;;
        arm64) MULTIARCH=aarch64-linux-gnu ;;
        *) echo "error: cannot map $ARCH to a multiarch triplet (install dpkg-dev)" >&2; exit 1 ;;
    esac
fi
printf 'interest-noawait /usr/lib/%s/nautilus/extensions-4\ninterest-noawait /usr/lib/%s/nautilus/extensions-3.0\n' \
    "$MULTIARCH" "$MULTIARCH" > "$PKG_DIR/DEBIAN/triggers"

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
echo "  The GUI tray app autostarts at login (/etc/xdg/autostart/es.rgon.ncrs.desktop)."
echo "  Headless (no-GUI) alternative: systemctl --user enable --now ncrs.service"
