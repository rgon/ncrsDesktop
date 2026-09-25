#!/usr/bin/env bash
# Per-user (development) install of the GNOME Shell search provider.
#
# Everything here lands in ~/.local/share, which overrides the files the .deb
# installs system-wide. So the script refuses to run while the package is
# installed, and --uninstall removes a previous per-user install.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
INSTALL_DIR="$HOME/.local/share/ncrs"
DBUS_DIR="$HOME/.local/share/dbus-1/services"
APP_DIR="$HOME/.local/share/applications"
PROVIDER_DIR="$HOME/.local/share/gnome-shell/search-providers"

# Before the launcher rename, this script installed the search provider's
# hidden (NoDisplay) entry as es.rgon.ncrs.desktop. That is now the GUI
# launcher's id, so a copy left behind hides the app from the app grid and
# makes App Center offer "Uninstall" instead of "Open".
LEGACY_DESKTOP="$APP_DIR/es.rgon.ncrs.desktop"

remove_user_install() {
    rm -f "$INSTALL_DIR/ncrs-search-provider" \
          "$DBUS_DIR/es.rgon.ncrs.SearchProvider.service" \
          "$APP_DIR/es.rgon.ncrs.SearchProvider.desktop" \
          "$PROVIDER_DIR/es.rgon.ncrs.SearchProvider.ini"
    # Only ever our own old search-provider entry, never a launcher the user made.
    if [[ -f "$LEGACY_DESKTOP" ]] && grep -q '^NoDisplay=true' "$LEGACY_DESKTOP" \
        && grep -q 'Search your Nextcloud' "$LEGACY_DESKTOP"; then
        rm -f "$LEGACY_DESKTOP"
    fi
    command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$APP_DIR" || true
}

if [[ "${1:-}" == "--uninstall" ]]; then
    remove_user_install
    echo "Removed the per-user GNOME Shell search provider."
    exit 0
fi

if [[ -f /usr/share/gnome-shell/search-providers/es.rgon.ncrs.SearchProvider.ini ]]; then
    echo "The ncrs package already installs the search provider system-wide." >&2
    echo "A per-user copy would override it; run with --uninstall to remove an old one." >&2
    exit 1
fi

remove_user_install

mkdir -p "$INSTALL_DIR"
cp "$SCRIPT_DIR/ncrs-search-provider" "$INSTALL_DIR/"
chmod +x "$INSTALL_DIR/ncrs-search-provider"

# D-Bus service file — patch Exec path
mkdir -p "$DBUS_DIR"
sed "s|INSTALL_DIR|$INSTALL_DIR|" "$SCRIPT_DIR/es.rgon.ncrs.SearchProvider.service" \
    > "$DBUS_DIR/es.rgon.ncrs.SearchProvider.service"

# .desktop file
mkdir -p "$APP_DIR"
cp "$SCRIPT_DIR/es.rgon.ncrs.SearchProvider.desktop" "$APP_DIR/"

# GNOME Shell search provider registration
mkdir -p "$PROVIDER_DIR"
cp "$SCRIPT_DIR/es.rgon.ncrs.SearchProvider.ini" "$PROVIDER_DIR/"

echo "Installed GNOME Shell search provider."
echo "Log out and back in (or restart GNOME Shell) to activate."
