#!/usr/bin/env bash
# Verifies a built ncrs .rpm: package metadata, the hard/soft dependency split,
# payload contents, the embedded production frontend, desktop entries, and
# (optionally) a clean-install smoke test in a container.
#
# Desktop-specific relationships must stay soft (Recommends/Suggests/
# Supplements): this script fails if nautilus, dolphin, python-nautilus or any
# kf6-* package appears as a hard Requires, because that would drag another
# desktop's stack onto every install.
#
# Usage: ./scripts/test-rpm.sh [OPTIONS]
#   --rpm PATH        .rpm to test (default: newest dist/ncrs-*.rpm)
#   --container       Also run the clean-install test (needs docker or podman)
#   --image IMAGE     Container image for the install test (default: opensuse/tumbleweed)
#   --skip-gui        The .rpm was built with --skip-gui; skip GUI assertions
#   --skip-dolphin    The .rpm was built without a staged Dolphin plugin
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

RPM=""
CONTAINER=false
IMAGE="opensuse/tumbleweed"
SKIP_GUI=false
SKIP_DOLPHIN=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --rpm)       RPM="${2:?--rpm requires a value}"; shift 2 ;;
        --container) CONTAINER=true; shift ;;
        --image)     IMAGE="${2:?--image requires a value}"; shift 2 ;;
        --skip-gui)  SKIP_GUI=true; shift ;;
        --skip-dolphin) SKIP_DOLPHIN=true; shift ;;
        -h|--help)   awk 'NR>1 && !/^#/{exit} NR>1{sub(/^# ?/,""); print}' "$0"; exit 0 ;;
        *) echo "Unknown option: $1 (see --help)" >&2; exit 2 ;;
    esac
done

if [[ -z "$RPM" ]]; then
    NEWEST=""
    for f in dist/ncrs-*.rpm; do
        [[ -e "$f" ]] || continue
        if [[ -z "$NEWEST" || "$f" -nt "$NEWEST" ]]; then NEWEST="$f"; fi
    done
    RPM="$NEWEST"
fi
[[ -n "$RPM" && -f "$RPM" ]] || { echo "No .rpm found (build one with scripts/build-rpm.sh, or pass --rpm)" >&2; exit 1; }

for tool in rpm rpm2cpio cpio; do
    command -v "$tool" >/dev/null 2>&1 || { echo "error: $tool not found (install the rpm package)" >&2; exit 1; }
done

FAILURES=0
pass() { echo "  ✓ $1"; }
fail() { echo "  ✗ $1" >&2; FAILURES=$((FAILURES + 1)); }
check() { local msg="$1"; shift; if "$@" >/dev/null 2>&1; then pass "$msg"; else fail "$msg"; fi; }

echo "Testing $RPM"

# ── Package metadata ────────────────────────────────────────────────────────
echo "→ Package metadata"
# The version is derived from the workspace Cargo.toml (scripts/build-rpm.sh
# defaults to it) so this test does not rot on every release bump.
VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"
META="$(rpm -qp --qf '%{NAME} %{VERSION} %{LICENSE}\n' "$RPM" 2>/dev/null)"
check "Package is ncrs"             test "${META%% *}" = "ncrs"
check "Version is $VERSION"         grep -q "^ncrs $VERSION " <<<"$META"
check "License is GPL-3.0-or-later" grep -q ' GPL-3\.0-or-later$' <<<"$META"

# ── Required and soft dependencies ──────────────────────────────────────────
echo "→ Required and soft dependencies"
REQUIRES="$(rpm -qpR "$RPM" 2>/dev/null)"
check "Requires fuse3" grep -q '^fuse3' <<<"$REQUIRES"
if ! $SKIP_GUI; then
    check "Requires libwebkit2gtk-4_1-0"       grep -q '^libwebkit2gtk-4_1-0' <<<"$REQUIRES"
    check "Requires libayatana-appindicator3-1" grep -q '^libayatana-appindicator3-1' <<<"$REQUIRES"
fi
# Desktop-specific packages must stay soft (Recommends/Suggests/Supplements),
# never a hard Requires (Review Focus 5).
if grep -qE '^(nautilus|dolphin|python-nautilus|kf6-)' <<<"$REQUIRES"; then
    fail "Requires has no desktop-specific packages"
else
    pass "Requires has no desktop-specific packages"
fi
# The Dolphin overlay plugin links Qt6/KF6, but Dolphin (which dlopens it)
# already brings those libraries, so the spec filters the libQt*/libKF* sonames
# out of Requires. Guard that filter: a Dolphin-enabled package must not
# hard-require them. (Only meaningful against a real compiled plugin; the
# sandbox stub is not an ELF and yields no soname requires.)
if ! $SKIP_DOLPHIN; then
    if grep -qE '^lib(Qt|KF)[0-9]' <<<"$REQUIRES"; then
        fail "Requires has no Qt6/KF6 sonames (Dolphin plugin must stay soft)"
    else
        pass "Requires has no Qt6/KF6 sonames (Dolphin plugin must stay soft)"
    fi
fi
# The soft relationships may carry them, and should keep doing so.
check "Suggests python-nautilus" \
    sh -c "rpm -qp --suggests '$RPM' | grep -qx python-nautilus"
check "Supplements nautilus and dolphin" \
    sh -c "rpm -qp --supplements '$RPM' | grep -qx nautilus && rpm -qp --supplements '$RPM' | grep -qx dolphin"

# ── Scriptlets ──────────────────────────────────────────────────────────────
# The GUI autostarts via /etc/xdg/autostart; the headless service must stay
# opt-in (a default install does not set a global enable).
echo "→ Scriptlets"
SCRIPTS="$(rpm -qp --scripts "$RPM" 2>/dev/null || true)"
if grep -q 'systemctl --global enable' <<<"$SCRIPTS"; then
    fail "post scriptlet must not globally enable ncrs.service"
else
    pass "post scriptlet does not globally enable ncrs.service"
fi

# ── Package contents ────────────────────────────────────────────────────────
echo "→ Package contents"
CONTENTS="$(rpm -qlp "$RPM" 2>/dev/null)"
REQUIRED=(
    /usr/bin/ncrs
    /usr/bin/ncrs-open
    /usr/bin/ncrs-ctl
    /usr/bin/ncrs-search-provider
    /usr/bin/cr3-thumbnailer
    /usr/lib/systemd/user/ncrs.service
    /usr/lib/sysctl.d/05-ncrs-quic.conf
    /usr/share/applications/es.rgon.ncrs.Open.desktop
    /usr/share/applications/es.rgon.ncrs.SearchProvider.desktop
    /usr/share/metainfo/es.rgon.ncrs.metainfo.xml
    /usr/share/dbus-1/services/es.rgon.ncrs.SearchProvider.service
    /usr/share/gnome-shell/search-providers/es.rgon.ncrs.SearchProvider.ini
    /usr/share/nautilus-python/extensions/ncrs-syncstate.py
    /usr/share/thumbnailers/cr3.thumbnailer
    /usr/share/doc/ncrs/config.yaml.example
    /usr/share/doc/ncrs/copyright
)
if ! $SKIP_GUI; then
    REQUIRED+=(
        /usr/bin/ncrs-gui
        /usr/share/applications/es.rgon.ncrs.desktop
        /etc/xdg/autostart/es.rgon.ncrs.desktop
        /usr/share/icons/hicolor/32x32/apps/ncrs.png
        /usr/share/icons/hicolor/64x64/apps/ncrs.png
        /usr/share/icons/hicolor/128x128/apps/ncrs.png
        /usr/share/icons/hicolor/256x256/apps/ncrs.png
        /usr/share/icons/hicolor/512x512/apps/ncrs.png
    )
fi
if ! $SKIP_DOLPHIN; then
    REQUIRED+=(/usr/share/kio/servicemenus/ncrs.desktop)
fi
for f in "${REQUIRED[@]}"; do
    check "$f" grep -qx "$f" <<<"$CONTENTS"
done

# One package carries the KF6 Dolphin plugin. %{_libdir} varies by platform
# (/usr/lib64 here, /usr/lib elsewhere), so match the tail rather than hardcode.
if ! $SKIP_DOLPHIN; then
    check "Dolphin plugin (KF6)" \
        grep -q '/qt6/plugins/kf6/overlayicon/ncrsoverlayplugin\.so$' <<<"$CONTENTS"
fi

# ── Embedded frontend ───────────────────────────────────────────────────────
# A GUI binary built without tauri's custom-protocol feature embeds no frontend
# and tries to load the vite dev server (devUrl) at runtime — "connection
# refused" on any machine not running `pnpm dev`. Embedded asset paths are
# stored uncompressed, so grep -a finds them.
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT
if ! $SKIP_GUI; then
    GUI_BIN="$WORK_DIR/ncrs-gui"
    # Extract to a file first: grepping the pipe would exit early and SIGPIPE
    # cpio/rpm2cpio, which pipefail turns into a spurious failure.
    rpm2cpio "$RPM" | cpio -i --to-stdout ./usr/bin/ncrs-gui > "$GUI_BIN" 2>/dev/null || true
    if grep -aq '_app/immutable' "$GUI_BIN"; then
        pass "ncrs-gui embeds the production frontend"
    else
        fail "ncrs-gui embeds the production frontend (built without --features custom-protocol?)"
    fi
fi

# ── Desktop entry validation ────────────────────────────────────────────────
# Validate the .desktop files actually inside the .rpm under test, not the repo
# checkout's copies (which may differ from the packaged artifact).
echo "→ Desktop entries"
if command -v desktop-file-validate >/dev/null 2>&1; then
    EXTRACT_DIR="$WORK_DIR/desktop"
    mkdir -p "$EXTRACT_DIR"
    rpm2cpio "$RPM" | (cd "$EXTRACT_DIR" && cpio -idm --quiet '*.desktop' 2>/dev/null) || true
    found_desktop=false
    # KIO ServiceMenus are KDE Type=Service files, not freedesktop entries.
    while IFS= read -r d; do
        found_desktop=true
        check "desktop-file-validate ${d#"$EXTRACT_DIR"}" desktop-file-validate "$d"
    done < <(find "$EXTRACT_DIR" -name '*.desktop' -not -path '*/kio/servicemenus/*' | sort)
    $found_desktop || fail "no .desktop files found in the package"
else
    echo "  (desktop-file-validate not installed; skipping)"
fi

# ── rpmlint (informational — pre-existing warnings are tolerated) ───────────
if command -v rpmlint >/dev/null 2>&1; then
    echo "→ rpmlint (informational)"
    rpmlint "$RPM" || true
fi

# ── Clean-install container test ────────────────────────────────────────────
if $CONTAINER; then
    echo "→ Clean-install test in $IMAGE"
    RUNTIME="$(command -v podman || command -v docker || true)"
    if [[ -z "$RUNTIME" ]]; then
        fail "container test requested but neither podman nor docker found"
    else
        RPM_ABS="$(readlink -f "$RPM")"
        GUI_CHECKS=""
        if ! $SKIP_GUI; then
            GUI_CHECKS='
            test -f /etc/xdg/autostart/es.rgon.ncrs.desktop || { echo "FAIL: autostart entry missing"; exit 1; }
            test -x /usr/bin/ncrs-gui || { echo "FAIL: ncrs-gui missing"; exit 1; }
            test -f /usr/share/icons/hicolor/128x128/apps/ncrs.png || { echo "FAIL: icon missing"; exit 1; }
            '
        fi
        # --allow-unsigned-rpm: the local/CI artifact is not GPG-signed.
        CONTAINER_SCRIPT="
            # openSUSE container images set rpm.install.excludedocs = yes in
            # zypp.conf, so /usr/share/doc is skipped on install -- the RPM
            # analogue of the Debian /etc/dpkg/dpkg.cfg.d/excludes that
            # test-deb.sh removes. Turn it off so the smoke test sees the whole
            # payload.
            if [ -f /etc/zypp/zypp.conf ]; then
                sed -i 's/^[[:space:]]*rpm\.install\.excludedocs[[:space:]]*=.*/rpm.install.excludedocs = no/' /etc/zypp/zypp.conf
                grep -q '^rpm\.install\.excludedocs' /etc/zypp/zypp.conf || echo 'rpm.install.excludedocs = no' >> /etc/zypp/zypp.conf
            fi
            zypper --no-gpg-checks -n install --allow-unsigned-rpm /pkg.rpm
            echo 'installed OK'
            test -x /usr/bin/ncrs-ctl || { echo 'FAIL: ncrs-ctl missing'; exit 1; }
            test -f /usr/share/nautilus-python/extensions/ncrs-syncstate.py || { echo 'FAIL: nautilus extension missing'; exit 1; }
            if rpm -qa --qf '%{NAME}\n' | grep -qE '^(nautilus|dolphin|python-nautilus|kf6-)'; then echo 'FAIL: installing ncrs pulled in desktop-specific packages'; exit 1; fi
            # openSUSE container images exclude /usr/share/doc by policy, so the
            # example config can legitimately be absent from the installed tree
            # even though the package carries it (the rpm -qlp content check
            # above asserts that). Fail only if the package itself lacks it.
            if [ -f /usr/share/doc/ncrs/config.yaml.example ]; then
                echo 'example config installed'
            elif rpm -q --list ncrs | grep -qx '/usr/share/doc/ncrs/config.yaml.example'; then
                echo 'note: example config is in the package but not installed (image excludes docs)'
            else
                echo 'FAIL: example config missing from the package'; exit 1
            fi
            test ! -e /etc/systemd/user/default.target.wants/ncrs.service || { echo 'FAIL: ncrs.service globally enabled'; exit 1; }
            /usr/bin/ncrs --print-default-config | grep -q 'ncRS Desktop configuration' || { echo 'FAIL: ncrs --print-default-config'; exit 1; }
            $GUI_CHECKS
            echo 'container checks OK'
        "
        if "$RUNTIME" run --rm -v "$RPM_ABS:/pkg.rpm:ro" "$IMAGE" bash -ec "$CONTAINER_SCRIPT"; then
            pass "clean install + smoke test in $IMAGE"
        else
            fail "clean install + smoke test in $IMAGE"
        fi
    fi
fi

echo ""
if [[ $FAILURES -gt 0 ]]; then
    echo "✗ $FAILURES check(s) failed"
    exit 1
fi
echo "✓ All checks passed"
