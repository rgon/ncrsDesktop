#!/usr/bin/env bash
# Builds the ncrs .rpm: creates a source tarball, substitutes the
# @VERSION@/@CHANGELOG_DATE@ tokens in packaging/ncrs.spec, and drives
# rpmbuild against a private _topdir.
#
# The spec's %install runs packaging/install-tree.sh, shared with
# scripts/build-deb.sh, so the .rpm and the .deb ship the identical payload.
# The output is a local/CI artifact, so rpm's debuginfo subpackage is disabled
# (design spec §6.3) and the result is the single ncrs-<version>-<release>.rpm.
#
# The Dolphin plugin is C++ built against KF6. A normal (from-source) build
# compiles it inside rpmbuild; with --skip-build it must already be staged
# under dist/dolphin/*/ and is folded into the tarball as dolphin-stage/, the
# path the spec's %install expects.
#
# Usage: ./scripts/build-rpm.sh [OPTIONS]
#   --version VERSION   Package version (default: workspace version in Cargo.toml)
#   --out-dir DIR       Output directory for the .rpm (default: dist)
#   --source-tar PATH   Use PATH as the source tarball instead of creating one
#                       from git; its top directory must be ncrs-<version>
#   --skip-build        Package prebuilt target/release/ binaries and the
#                       staged Dolphin tree; tells the spec not to compile
#   --skip-gui          Do not package the GUI tray app
#   --skip-dolphin      Do not package the Dolphin plugin
#   --dolphin-stage DIR Staged Dolphin plugin tree to include (repeatable;
#                       default: every dist/dolphin/*/ that exists; --skip-build only)
#   --require-dolphin   Fail instead of warning when no Dolphin plugin is staged
#   --nodeps            Do not let rpmbuild verify BuildRequires. rpmbuild checks
#                       the RPM database, so a toolchain installed outside it
#                       (rustup's cargo, an npm-installed pnpm) cannot satisfy
#                       `BuildRequires:` even though the tools work. Use this on
#                       such a host once every -devel package is really present.
set -euo pipefail

# ── Parse arguments ───────────────────────────────────────────────────────────
VERSION=""
OUT_DIR="dist"
SOURCE_TAR=""
SKIP_BUILD=false
SKIP_GUI=false
SKIP_DOLPHIN=false
DOLPHIN_STAGES=()
REQUIRE_DOLPHIN=false
NODEPS=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version)         VERSION="${2:?--version requires a value}"; shift 2 ;;
        --out-dir)         OUT_DIR="${2:?--out-dir requires a value}"; shift 2 ;;
        --source-tar)      SOURCE_TAR="${2:?--source-tar requires a value}"; shift 2 ;;
        --skip-build)      SKIP_BUILD=true; shift ;;
        --skip-gui)        SKIP_GUI=true; shift ;;
        --skip-dolphin)    SKIP_DOLPHIN=true; shift ;;
        --dolphin-stage)   DOLPHIN_STAGES+=("${2:?--dolphin-stage requires a value}"); shift 2 ;;
        --require-dolphin) REQUIRE_DOLPHIN=true; shift ;;
        --nodeps)          NODEPS=true; shift ;;
        -h|--help)         awk 'NR>1 && !/^#/{exit} NR>1{sub(/^# ?/,""); print}' "$0"; exit 0 ;;
        *) echo "Unknown option: $1 (see --help)" >&2; exit 2 ;;
    esac
done

# ── Fail fast, before doing any work ─────────────────────────────────────────
# rpmbuild is required for every build and git for creating the source tarball.
# Check with command -v (a shell builtin) first, before any other external
# command, so a stripped PATH reports this clearly instead of failing obscurely.
command -v rpmbuild >/dev/null 2>&1 || { echo "error: rpmbuild not found (install the rpm-build package)" >&2; exit 1; }
command -v git >/dev/null 2>&1 || { echo "error: git not found (needed to create the source tarball)" >&2; exit 1; }

# Resolve --source-tar against the caller's cwd before we cd to the repo root,
# so a relative path keeps its meaning (like install-tree.sh's --dest).
if [[ -n "$SOURCE_TAR" ]]; then
    case "$SOURCE_TAR" in
        /*) ;;
        *) SOURCE_TAR="$PWD/$SOURCE_TAR" ;;
    esac
    [[ -f "$SOURCE_TAR" ]] || { echo "error: --source-tar not found: $SOURCE_TAR" >&2; exit 1; }
fi

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

[[ -n "$VERSION" ]] || VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"

# The spec's %files expects the Dolphin plugin under %{_libdir}; derive it from
# rpm so this check tracks the real package wherever it is built.
LIBDIR="$(rpm --eval '%{_libdir}' 2>/dev/null || true)"
[[ -n "$LIBDIR" ]] || LIBDIR=/usr/lib
DOLPHIN_PLUGIN_REL="${LIBDIR%/}/qt6/plugins/kf6/overlayicon/ncrsoverlayplugin.so"

# ── Resolve the Dolphin stage set ─────────────────────────────────────────────
# Explicit --dolphin-stage wins; otherwise use every dist/dolphin/*/ (mirrors
# scripts/build-deb.sh). The plugin lives at <stage><_libdir>/qt6/...
if [[ ${#DOLPHIN_STAGES[@]} -eq 0 ]] && ! $SKIP_DOLPHIN; then
    for d in dist/dolphin/*/; do
        if [[ -d "$d" ]]; then DOLPHIN_STAGES+=("$d"); fi
    done
fi

HAVE_DOLPHIN_PLUGIN=false
for stage in ${DOLPHIN_STAGES[@]+"${DOLPHIN_STAGES[@]}"}; do
    if [[ -f "${stage%/}$DOLPHIN_PLUGIN_REL" ]]; then HAVE_DOLPHIN_PLUGIN=true; fi
done

echo "Building ncrs ${VERSION} (.rpm) → ${OUT_DIR}"

# With --skip-build the tarball is assembled from prebuilt artifacts, so they
# must exist before rpmbuild starts. (--source-tar is used verbatim and is the
# caller's responsibility.)
if $SKIP_BUILD && [[ -z "$SOURCE_TAR" ]]; then
    for bin in ncrs ncrs-open ncrs-ctl; do
        [[ -x "target/release/$bin" ]] || { echo "error: --skip-build set but target/release/$bin is missing" >&2; exit 1; }
    done
    if ! $SKIP_GUI && [[ ! -x target/release/ncrs-gui ]]; then
        echo "error: --skip-build set but target/release/ncrs-gui is missing (pass --skip-gui to package without the GUI)" >&2
        exit 1
    fi
fi

# The spec unconditionally passes --dolphin-stage dolphin-stage to
# install-tree.sh when built with dolphin, so a generated prebuilt tarball with
# no staged plugin cannot satisfy %files. Fail (or, without --require-dolphin,
# warn and drop dolphin) before rpmbuild runs. A --source-tar tarball is used
# verbatim, so its Dolphin tree is the caller's responsibility.
if $SKIP_BUILD && [[ -z "$SOURCE_TAR" ]] && ! $SKIP_DOLPHIN && ! $HAVE_DOLPHIN_PLUGIN; then
    if $REQUIRE_DOLPHIN; then
        echo "error: no Dolphin plugin staged (looked for ${DOLPHIN_PLUGIN_REL#/} under dist/dolphin/*/);" >&2
        echo "       build it with scripts/build-dolphin-plugin.sh, or pass --skip-dolphin" >&2
        exit 1
    fi
    echo "  warning: no Dolphin plugin staged; the package will have no Dolphin emblems"
    SKIP_DOLPHIN=true
fi

# ── Assemble the source tarball ───────────────────────────────────────────────
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT
TOPDIR="$WORK_DIR/topdir"
mkdir -p "$TOPDIR/BUILD" "$TOPDIR/BUILDROOT" "$TOPDIR/RPMS" "$TOPDIR/SOURCES" "$TOPDIR/SPECS" "$TOPDIR/SRPMS"

SRC_NAME="ncrs-${VERSION}.tar.gz"

# make_source_tar: git archive HEAD into SOURCES as ncrs-<version>.tar.gz. Plain
# for a from-source build; with --skip-build the prebuilt binaries and the
# resolved Dolphin stages are injected first (target/release/ and dolphin-stage/).
make_source_tar() {
    local root="ncrs-${VERSION}"
    local git_tar="$WORK_DIR/source.tar"
    git archive --format=tar --prefix="${root}/" HEAD -o "$git_tar"

    if $SKIP_BUILD; then
        local unpack="$WORK_DIR/unpack"
        mkdir -p "$unpack"
        tar -xf "$git_tar" -C "$unpack"
        mkdir -p "$unpack/$root/target/release"
        local bin
        for bin in ncrs ncrs-open ncrs-ctl; do
            install -m755 "target/release/$bin" "$unpack/$root/target/release/$bin"
        done
        if ! $SKIP_GUI; then
            install -m755 target/release/ncrs-gui "$unpack/$root/target/release/ncrs-gui"
        fi
        if ! $SKIP_DOLPHIN; then
            mkdir -p "$unpack/$root/dolphin-stage"
            local stage
            for stage in ${DOLPHIN_STAGES[@]+"${DOLPHIN_STAGES[@]}"}; do
                cp -a "$stage/." "$unpack/$root/dolphin-stage/"
            done
        fi
        tar -czf "$TOPDIR/SOURCES/$SRC_NAME" -C "$unpack" "$root"
    else
        gzip -9n -c "$git_tar" > "$TOPDIR/SOURCES/$SRC_NAME"
    fi
}

if [[ -n "$SOURCE_TAR" ]]; then
    echo "→ Using source tarball: $SOURCE_TAR"
    cp "$SOURCE_TAR" "$TOPDIR/SOURCES/$SRC_NAME"
else
    echo "→ Creating source tarball ($SRC_NAME)..."
    make_source_tar
fi

# ── Generate the spec (substitute the two tokens) ────────────────────────────
CHANGELOG_DATE="$(LC_ALL=C date '+%a %b %d %Y')"
SPEC="$TOPDIR/SPECS/ncrs.spec"
sed -e "s|@VERSION@|${VERSION}|g" \
    -e "s|@CHANGELOG_DATE@|${CHANGELOG_DATE}|g" \
    packaging/ncrs.spec > "$SPEC"
if grep -q -e '@VERSION@' -e '@CHANGELOG_DATE@' "$SPEC"; then
    echo "error: unsubstituted token remains in the generated spec" >&2
    exit 1
fi

# ── Run rpmbuild ─────────────────────────────────────────────────────────────
RPMBUILD_ARGS=(
    -bb
    --define "_topdir $TOPDIR"
    --define "_sourcedir $TOPDIR/SOURCES"
    # Local/CI artifact, not a distro build: debuginfo of the large Tauri binary
    # is not needed (design spec §6.3), and rpm's default %debug_package
    # subpackage fails to assemble when the staged payload has no ELF files (a
    # --skip-build tarball of stubs). Disabling it keeps the single-artifact output.
    --define "debug_package %{nil}"
)
if $SKIP_BUILD; then
    RPMBUILD_ARGS+=(--define "ncrs_skip_build 1")
fi
if $SKIP_GUI; then
    RPMBUILD_ARGS+=(--without gui)
fi
if $SKIP_DOLPHIN; then
    RPMBUILD_ARGS+=(--without dolphin)
fi
if $NODEPS; then
    # Skip rpmbuild's BuildRequires verification. For hosts whose toolchain is
    # not an RPM (rustup cargo, npm-installed pnpm); see --help.
    RPMBUILD_ARGS+=(--nodeps)
fi

rpmbuild "${RPMBUILD_ARGS[@]}" "$SPEC"

# ── Collect the result ───────────────────────────────────────────────────────
mapfile -t RPMS < <(find "$TOPDIR/RPMS" -type f -name '*.rpm' | sort)
if [[ ${#RPMS[@]} -eq 0 ]]; then
    echo "error: rpmbuild produced no .rpm under $TOPDIR/RPMS" >&2
    exit 1
fi

mkdir -p "$OUT_DIR"
for rpm in "${RPMS[@]}"; do
    cp "$rpm" "$OUT_DIR/"
done

echo ""
for rpm in "${RPMS[@]}"; do
    dest="$OUT_DIR/$(basename "$rpm")"
    # A local install path needs ./ so zypper reads it as a file (a bare path
    # could be taken for a repo package name); an absolute --out-dir already is
    # unambiguous.
    case "$dest" in
        /*) install_arg="$dest" ;;
        *)  install_arg="./$dest" ;;
    esac
    echo "✓ Built: $dest"
    echo "  Install with: sudo zypper install --allow-unsigned-rpm $install_arg"
    echo "  Inspect with: rpm -qlp $dest"
done
