#!/usr/bin/env bash
# Verifies a built ncrs .deb: control metadata, package contents, desktop
# entries, and (optionally) a clean-install smoke test in a container.
#
# Usage: ./scripts/test-deb.sh [OPTIONS]
#   --deb PATH        .deb to test (default: newest dist/ncrs_*.deb)
#   --container       Also run the clean-install test (needs docker or podman)
#   --image IMAGE     Container image for the install test (default: ubuntu:24.04)
#   --skip-gui        The .deb was built with --skip-gui; skip GUI assertions
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

DEB=""
CONTAINER=false
IMAGE="ubuntu:24.04"
SKIP_GUI=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --deb)       DEB="$2"; shift 2 ;;
        --container) CONTAINER=true; shift ;;
        --image)     IMAGE="$2"; shift 2 ;;
        --skip-gui)  SKIP_GUI=true; shift ;;
        -h|--help)   awk 'NR>1 && !/^#/{exit} NR>1{sub(/^# ?/,""); print}' "$0"; exit 0 ;;
        *) echo "Unknown option: $1 (see --help)" >&2; exit 2 ;;
    esac
done

if [[ -z "$DEB" ]]; then
    DEB="$(ls -t dist/ncrs_*.deb 2>/dev/null | head -1 || true)"
fi
[[ -n "$DEB" && -f "$DEB" ]] || { echo "No .deb found (build one with scripts/build-deb.sh, or pass --deb)" >&2; exit 1; }

FAILURES=0
pass() { echo "  ✓ $1"; }
fail() { echo "  ✗ $1" >&2; FAILURES=$((FAILURES + 1)); }
check() { local msg="$1"; shift; if "$@" >/dev/null 2>&1; then pass "$msg"; else fail "$msg"; fi; }

echo "Testing $DEB"

# ── Control metadata ──────────────────────────────────────────────────────────
echo "→ Control metadata"
CONTROL="$(dpkg-deb -f "$DEB")"
check "Package is ncrs"            grep -q '^Package: ncrs$' <<<"$CONTROL"
check "Depends on fuse3"           grep -q '^Depends: .*fuse3' <<<"$CONTROL"
check "Depends on nautilus ext"    grep -q 'python3-nautilus' <<<"$CONTROL"
if ! $SKIP_GUI; then
    check "Depends on webkit2gtk"  grep -q 'libwebkit2gtk' <<<"$CONTROL"
fi

echo "→ Maintainer scripts"
CTRL_FILES="$(dpkg-deb --ctrl-tarfile "$DEB" | tar -t)"
for s in postinst prerm postrm; do
    check "$s present" grep -qx "\./$s" <<<"$CTRL_FILES"
done
# The GUI autostarts via /etc/xdg/autostart; the headless service must stay opt-in.
if dpkg-deb --ctrl-tarfile "$DEB" | tar -xO ./postinst 2>/dev/null | grep -q 'systemctl --global enable'; then
    fail "postinst must not globally enable ncrs.service"
else
    pass "postinst does not globally enable ncrs.service"
fi

# ── Package contents ──────────────────────────────────────────────────────────
echo "→ Package contents"
CONTENTS="$(dpkg-deb -c "$DEB" | awk '{print $NF}')"
REQUIRED=(
    ./usr/bin/ncrs
    ./usr/bin/ncrs-open
    ./usr/bin/ncrs-search-provider
    ./usr/lib/systemd/user/ncrs.service
    ./usr/share/applications/ncrs-open.desktop
    ./usr/share/applications/es.rgon.ncrs.desktop
    ./usr/share/dbus-1/services/es.rgon.ncrs.SearchProvider.service
    ./usr/share/gnome-shell/search-providers/es.rgon.ncrs.SearchProvider.ini
    ./usr/share/nautilus-python/extensions/ncrs-syncstate.py
    ./usr/share/doc/ncrs/config.yaml.example
)
if ! $SKIP_GUI; then
    REQUIRED+=(
        ./usr/bin/ncrs-gui
        ./usr/share/applications/ncrs-gui.desktop
        ./etc/xdg/autostart/ncrs-gui.desktop
        ./usr/share/icons/hicolor/32x32/apps/ncrs.png
        ./usr/share/icons/hicolor/128x128/apps/ncrs.png
        ./usr/share/icons/hicolor/256x256/apps/ncrs.png
        ./usr/share/icons/hicolor/512x512/apps/ncrs.png
    )
fi
for f in "${REQUIRED[@]}"; do
    check "$f" grep -qx "$f" <<<"$CONTENTS"
done

# ── Desktop entry validation ──────────────────────────────────────────────────
# Validate the .desktop files actually inside the .deb under test, not the
# repo checkout's copies (which may differ from the packaged artifact).
echo "→ Desktop entries"
if command -v desktop-file-validate >/dev/null 2>&1; then
    EXTRACT_DIR="$(mktemp -d)"
    trap 'rm -rf "$EXTRACT_DIR"' EXIT
    dpkg-deb --fsys-tarfile "$DEB" | tar -x -C "$EXTRACT_DIR" --wildcards '*.desktop'
    found_desktop=false
    while IFS= read -r d; do
        found_desktop=true
        check "desktop-file-validate ${d#"$EXTRACT_DIR"}" desktop-file-validate "$d"
    done < <(find "$EXTRACT_DIR" -name '*.desktop' | sort)
    $found_desktop || fail "no .desktop files found in the package"
else
    echo "  (desktop-file-validate not installed; skipping)"
fi

# ── Lintian (informational — pre-existing warnings are tolerated) ────────────
if command -v lintian >/dev/null 2>&1; then
    echo "→ Lintian (informational)"
    lintian "$DEB" || true
fi

# ── Clean-install container test ──────────────────────────────────────────────
if $CONTAINER; then
    echo "→ Clean-install test in $IMAGE"
    RUNTIME="$(command -v podman || command -v docker || true)"
    if [[ -z "$RUNTIME" ]]; then
        fail "container test requested but neither podman nor docker found"
    else
        DEB_ABS="$(readlink -f "$DEB")"
        GUI_CHECKS=""
        if ! $SKIP_GUI; then
            GUI_CHECKS='
            test -f /etc/xdg/autostart/ncrs-gui.desktop || { echo "FAIL: autostart entry missing"; exit 1; }
            test -x /usr/bin/ncrs-gui || { echo "FAIL: ncrs-gui missing"; exit 1; }
            test -f /usr/share/icons/hicolor/128x128/apps/ncrs.png || { echo "FAIL: icon missing"; exit 1; }
            '
        fi
        "$RUNTIME" run --rm -v "$DEB_ABS:/pkg.deb:ro" "$IMAGE" bash -ec "
            # Minimized cloud images exclude /usr/share/doc — undo so we can
            # assert the provisioning example config actually installs.
            rm -f /etc/dpkg/dpkg.cfg.d/excludes
            apt-get update -qq >/dev/null
            DEBIAN_FRONTEND=noninteractive apt-get install -y -qq /pkg.deb >/dev/null
            echo 'installed OK'
            test -f /usr/share/doc/ncrs/config.yaml.example || { echo 'FAIL: example config missing'; exit 1; }
            ! ls /etc/systemd/user/default.target.wants/ncrs.service >/dev/null 2>&1 || { echo 'FAIL: ncrs.service globally enabled'; exit 1; }
            /usr/bin/ncrs --print-default-config | grep -q 'ncRS Desktop configuration' || { echo 'FAIL: ncrs --print-default-config'; exit 1; }
            $GUI_CHECKS
            echo 'container checks OK'
        " && pass "clean install + smoke test in $IMAGE" || fail "clean install + smoke test in $IMAGE"
    fi
fi

echo ""
if [[ $FAILURES -gt 0 ]]; then
    echo "✗ $FAILURES check(s) failed"
    exit 1
fi
echo "✓ All checks passed"
