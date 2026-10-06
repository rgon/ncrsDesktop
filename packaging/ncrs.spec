# openSUSE RPM spec for the ncRS desktop client.
#
# Targets openSUSE Leap 16 / Slowroll / Tumbleweed (Qt6/KF6 only). The Rust
# workspace, the pnpm frontend and the KF6 Dolphin plugin are all built from
# source in the build section; packaging/install-tree.sh stages the shared
# payload, so this package and the .deb ship the identical tree.
#
# Desktop-specific relationships stay soft (Recommends/Suggests/Supplements):
# installing ncRS must not pull in Nautilus or the KDE Frameworks stacks.
#
# scripts/build-rpm.sh substitutes the version and changelog date tokens below.

# Off-openSUSE fallback so the spec parses anywhere; on openSUSE the real
# macro wins.
%{!?_userunitdir:%global _userunitdir %{_prefix}/lib/systemd/user}

%bcond_without gui
%bcond_without dolphin

# No debuginfo subpackage: this is a local/CI artifact, and rpm's default
# debug_package subpackage fails to assemble when a --skip-build tarball has no
# ELF files. Matches scripts/build-rpm.sh's --define (which stays, harmlessly).
%global debug_package %{nil}

# The KF6 Dolphin overlay plugin links Qt6/KF6 sonames, but only Dolphin dlopens
# it and Dolphin already pulls in those libraries. Excluding libQt*/libKF*
# soname Requires keeps desktop integration soft, per this spec's rule; nothing
# else in the package links Qt/KF, so the filter hides no real dependency.
%global __requires_exclude ^lib(Qt|KF)[0-9].*

Name:           ncrs
Version:        @VERSION@
Release:        0
Summary:        Nextcloud FUSE virtual filesystem client
License:        GPL-3.0-or-later
URL:            https://github.com/rgon/ncrsDesktop
Source0:        %{name}-%{version}.tar.gz
# Arch-specific binary package: ExclusiveArch (not BuildArch, which declares
# noarch) both expresses "x86_64 only" and satisfies rpmlint.
ExclusiveArch:  x86_64

# Toolchain
BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  gcc
BuildRequires:  gcc-c++
BuildRequires:  make
BuildRequires:  pkgconf-pkg-config
# Core
BuildRequires:  fuse3-devel
BuildRequires:  libopenssl-devel
# GUI (Tauri) -- only needed when the tray app is built.
%if %{with gui}
BuildRequires:  gtk3-devel
# openSUSE names the WebKitGTK-4.1 (GTK3) development package `webkitgtk3-devel`.
# The "3" is GTK3, and it is NOT `webkit2gtk3-devel` -- that name no longer
# exists on current Tumbleweed/Slowroll (zypper: "No provider ... found").
BuildRequires:  webkitgtk3-devel
BuildRequires:  librsvg-devel
# Frontend (pnpm builds the Tauri web assets). Ask for the generic `nodejs`
# capability, not a pinned release: openSUSE's versioned nodejs packages all
# Provide `nodejs` (nodejs24 Provides nodejs), so this accepts whatever default
# the release ships instead of forcing one. No version floor: the provide may be
# unversioned, and the distro's node is new enough for the frontend by policy.
BuildRequires:  nodejs
BuildRequires:  pnpm
%endif
# Dolphin plugin (KF6)
BuildRequires:  cmake
BuildRequires:  kf6-extra-cmake-modules
BuildRequires:  qt6-base-devel
BuildRequires:  kf6-kio-devel
BuildRequires:  kf6-kcoreaddons-devel

# RPM auto-derives the linked-soname dependencies, so only what find-requires
# cannot see is listed here. Everything desktop-specific stays soft, so one
# package suits every desktop without pulling another's stack.
Requires:       fuse3
%if %{with gui}
Requires:       libwebkit2gtk-4_1-0
# libayatana-appindicator3-1 (the tray icon backend) is Package Hub-only on
# Leap 16, not in OSS/Update, so a hard Requires would make the .rpm
# uninstallable on a stock Leap 16. Keep it hard on Tumbleweed/Slowroll and
# soft on Leap 16; without it the GUI still runs, only the tray icon is absent.
%if 0%{?sle_version} >= 160000
Recommends:     libayatana-appindicator3-1
%else
Requires:       libayatana-appindicator3-1
%endif
%endif
Recommends:     libcap-progs
Recommends:     python3-gobject
Recommends:     typelib-1_0-GdkPixbuf-2_0
Recommends:     perl-Image-ExifTool
Suggests:       python-nautilus
Supplements:    nautilus, dolphin

%description
ncrs mounts your Nextcloud as a local FUSE filesystem with offline
caching, real-time sync, conflict detection and a GNOME Shell search
provider. File-browser integration (indexer exclusion, thumbnails,
type detection) is applied per installed browser by the service, with
sync emblems and menus in Nautilus and Dolphin.
.
libcap-progs (setcap) is used at install time to grant the ncrs binary
CAP_SYS_ADMIN, which enables zero-copy kernel read passthrough for
fully-cached files on Linux 6.9+. Without it, ncrs runs identically
but always falls back to normal buffered reads.

%prep
%setup -q -n %{name}-%{version}

%build
%if ! 0%{?ncrs_skip_build}
# .cargo/config.toml forces -fuse-ld=mold, which Leap 16 does not package.
# Override build.rustflags on the command line so --cfg reqwest_unstable
# survives but the mold link arg does not.
export CARGO_ENCODED_RUSTFLAGS=$'--cfg\x1freqwest_unstable'

# pnpm/npm want a writable HOME for their cache. Scope it to these two commands:
# exporting HOME globally hides a user-level rustup toolchain (~/.rustup) from
# the cargo call below, which then fails with "rustup could not choose a version
# of cargo ... no default is configured". cargo keeps the real HOME.
%if %{with gui}
pushd ncrs-gui
HOME="$PWD" pnpm install --frozen-lockfile
HOME="$PWD" pnpm build
popd

cargo build --release -p ncrs_core -p ncrs-gui --features ncrs-gui/custom-protocol
%else
cargo build --release -p ncrs_core
%endif

%if %{with dolphin}
# Dolphin overlay plugin + ServiceMenu. build-dolphin-plugin.sh defaults to
# Qt6/KF6 (pass --kf5 for Plasma 5), so no version flag is needed here.
bash scripts/build-dolphin-plugin.sh --dest dolphin-stage
%endif
%endif

%install
install_args="--dest %{buildroot} --bin-dir target/release"
%if %{with dolphin}
install_args="$install_args --dolphin-stage dolphin-stage"
%else
install_args="$install_args --skip-dolphin"
%endif
%if ! %{with gui}
install_args="$install_args --skip-gui"
%endif
packaging/install-tree.sh $install_args

%files
# install-tree.sh (shared with the .deb) stages these under /usr/share/doc/ncrs,
# so list that path. Do not use the default-docdir macro: on openSUSE it resolves
# to /usr/share/doc/packages and would not match. (No macro names in this comment
# -- rpm expands macros in comments too, which warns.)
%{_datadir}/doc/ncrs/copyright
%{_datadir}/doc/ncrs/config.yaml.example
%{_bindir}/ncrs
%{_bindir}/ncrs-open
%{_bindir}/ncrs-ctl
%{_bindir}/ncrs-search-provider
%{_bindir}/cr3-thumbnailer
%{_userunitdir}/ncrs.service
# rpmlint flags this as hardcoded-library-path, but /usr/lib/sysctl.d is the
# correct systemd drop-in location (it is not a library directory).
%{_prefix}/lib/sysctl.d/05-ncrs-quic.conf
%{_datadir}/applications/es.rgon.ncrs.Open.desktop
%{_datadir}/applications/es.rgon.ncrs.SearchProvider.desktop
%{_datadir}/gnome-shell/search-providers/es.rgon.ncrs.SearchProvider.ini
%{_datadir}/dbus-1/services/es.rgon.ncrs.SearchProvider.service
%{_datadir}/metainfo/es.rgon.ncrs.metainfo.xml
%{_datadir}/nautilus-python/extensions/ncrs-syncstate.py
%{_datadir}/thumbnailers/cr3.thumbnailer
%if %{with dolphin}
%{_libdir}/qt6/plugins/kf6/overlayicon/ncrsoverlayplugin.so
%{_datadir}/kio/servicemenus/ncrs.desktop
%endif
%if %{with gui}
%{_bindir}/ncrs-gui
%{_datadir}/applications/es.rgon.ncrs.desktop
%{_sysconfdir}/xdg/autostart/es.rgon.ncrs.desktop
%{_datadir}/icons/hicolor/*/apps/ncrs.png
%endif

%post
set -e

# Reload Nautilus so the extension is picked up immediately. Only quits
# instances that are already running; reads each process's own D-Bus address
# so this works correctly from a root rpm scriptlet.
reload_nautilus() {
    if command -v nautilus >/dev/null 2>&1 && command -v runuser >/dev/null 2>&1; then
        for user in $(who | awk '{print $1}' | sort -u); do
            uid=$(id -u "$user" 2>/dev/null) || continue
            pid=$(pgrep -u "$uid" -x nautilus 2>/dev/null | head -1) || true
            [ -n "$pid" ] || continue
            dbus=$(grep -z DBUS_SESSION_BUS_ADDRESS /proc/"$pid"/environ 2>/dev/null \
                   | tr '\0' '\n' | grep '^DBUS_SESSION_BUS_ADDRESS=' \
                   | sed 's/^DBUS_SESSION_BUS_ADDRESS=//') || true
            [ -n "$dbus" ] || continue
            DBUS_SESSION_BUS_ADDRESS="$dbus" runuser -u "$user" -- nautilus -q 2>/dev/null || true
        done
    fi
}

# Reload Dolphin so the overlay plugin is picked up immediately. Dolphin only
# reads plugins at process startup, so a running instance has to be quit;
# nothing relaunches it for the user. Prefers kquitapp6 (KF6) and falls back to
# kquitapp5 (KF5); only touches instances already running.
reload_dolphin() {
    if command -v kquitapp6 >/dev/null 2>&1; then
        kquit=kquitapp6
    elif command -v kquitapp5 >/dev/null 2>&1; then
        kquit=kquitapp5
    else
        return 0
    fi
    command -v runuser >/dev/null 2>&1 || return 0
    for user in $(who | awk '{print $1}' | sort -u); do
        uid=$(id -u "$user" 2>/dev/null) || continue
        pid=$(pgrep -u "$uid" -x dolphin 2>/dev/null | head -1) || true
        [ -n "$pid" ] || continue
        dbus=$(grep -z DBUS_SESSION_BUS_ADDRESS /proc/"$pid"/environ 2>/dev/null \
               | tr '\0' '\n' | grep '^DBUS_SESSION_BUS_ADDRESS=' \
               | sed 's/^DBUS_SESSION_BUS_ADDRESS=//') || true
        [ -n "$dbus" ] || continue
        DBUS_SESSION_BUS_ADDRESS="$dbus" runuser -u "$user" -- "$kquit" dolphin 2>/dev/null || true
    done
}

# Raise a sysctl to at least $2 now, without waiting for the next boot to apply
# /usr/lib/sysctl.d/05-ncrs-quic.conf. Only ever raises: a value already at or
# above $2 (an admin's choice) is left alone, which sysctl -p on the file would
# not do. Best-effort -- in a container /proc/sys is usually read-only.
raise_sysctl() {
    cur=$(sysctl -n "$1" 2>/dev/null) || return 0
    [ "$cur" -lt "$2" ] 2>/dev/null || return 0
    sysctl -q -w "$1=$2" >/dev/null 2>&1 || \
        echo "ncrs: could not raise $1 to $2 -- HTTP/3 downloads may drop packets until it is" >&2
}

# Room for the QUIC download sockets' 4 MiB buffers (see the drop-in).
if command -v sysctl >/dev/null 2>&1; then
    raise_sysctl net.core.rmem_max 4194304
    raise_sysctl net.core.wmem_max 4194304
fi
# Grant CAP_SYS_ADMIN via a file capability so ncrs -- an ordinary per-user
# process, not root or setuid -- can use kernel FUSE_PASSTHROUGH (zero-copy
# reads for fully-cached files, Linux 6.9+). Best-effort: setcap comes from
# libcap-progs, a Recommends (not a hard Requires), so its absence is expected
# on minimal installs. ncrs detects the missing capability itself at runtime
# and falls back to normal reads, so this is never fatal to the install.
if command -v setcap >/dev/null 2>&1; then
    setcap cap_sys_admin+ep /usr/bin/ncrs || \
        echo "ncrs: setcap failed -- FUSE passthrough will stay disabled (buffered reads still work normally)" >&2
fi
# Register the nc:// URI handler and other desktop entries
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications || true
fi
# Refresh MIME info cache
if command -v update-mime-database >/dev/null 2>&1; then
    update-mime-database /usr/share/mime || true
fi
# Refresh icon cache for the ncrs hicolor icons
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q /usr/share/icons/hicolor || true
fi
reload_nautilus
# The Dolphin plugin ships inside this package when built with it (no separate
# loader to wait on, unlike python-nautilus), so reloading is always safe --
# and a no-op when the plugin is not present.
reload_dolphin

%preun
# Only on final removal: drop any global enable (a default install no longer
# sets one, but an admin may have opted in for headless use).
if [ "$1" -eq 0 ]; then
    if command -v systemctl >/dev/null 2>&1 && systemctl is-system-running --quiet 2>/dev/null; then
        systemctl --global disable ncrs.service || true
    fi
fi

%postun
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q /usr/share/icons/hicolor || true
fi

%posttrans
# Reload Nautilus at the end of the transaction that installs/upgrades ncrs (and
# if Nautilus itself was upgraded alongside). RPM posttrans runs only for the
# *current* transaction, so the .deb's dpkg-trigger behavior is NOT reproduced
# when python-nautilus (Suggests) is installed later, in a separate transaction:
# this scriptlet does not run then, and Nautilus must be restarted for the
# extension to load.
reload_nautilus() {
    if command -v nautilus >/dev/null 2>&1 && command -v runuser >/dev/null 2>&1; then
        for user in $(who | awk '{print $1}' | sort -u); do
            uid=$(id -u "$user" 2>/dev/null) || continue
            pid=$(pgrep -u "$uid" -x nautilus 2>/dev/null | head -1) || true
            [ -n "$pid" ] || continue
            dbus=$(grep -z DBUS_SESSION_BUS_ADDRESS /proc/"$pid"/environ 2>/dev/null \
                   | tr '\0' '\n' | grep '^DBUS_SESSION_BUS_ADDRESS=' \
                   | sed 's/^DBUS_SESSION_BUS_ADDRESS=//') || true
            [ -n "$dbus" ] || continue
            DBUS_SESSION_BUS_ADDRESS="$dbus" runuser -u "$user" -- nautilus -q 2>/dev/null || true
        done
    fi
}
reload_nautilus

%changelog
* @CHANGELOG_DATE@ ncRS packagers - @VERSION@-0
- Initial openSUSE RPM packaging.
