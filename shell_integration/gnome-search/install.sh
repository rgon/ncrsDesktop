#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
INSTALL_DIR="$HOME/.local/share/ncrs"

mkdir -p "$INSTALL_DIR"
cp "$SCRIPT_DIR/ncrs-search-provider" "$INSTALL_DIR/"
chmod +x "$INSTALL_DIR/ncrs-search-provider"

# D-Bus service file — patch Exec path
DBUS_DIR="$HOME/.local/share/dbus-1/services"
mkdir -p "$DBUS_DIR"
sed "s|INSTALL_DIR|$INSTALL_DIR|" "$SCRIPT_DIR/es.rgon.ncrs.SearchProvider.service" \
    > "$DBUS_DIR/es.rgon.ncrs.SearchProvider.service"

# .desktop file
APP_DIR="$HOME/.local/share/applications"
mkdir -p "$APP_DIR"
cp "$SCRIPT_DIR/es.rgon.ncrs.desktop" "$APP_DIR/"

# GNOME Shell search provider registration
PROVIDER_DIR="$HOME/.local/share/gnome-shell/search-providers"
mkdir -p "$PROVIDER_DIR"
cp "$SCRIPT_DIR/es.rgon.ncrs.SearchProvider.ini" "$PROVIDER_DIR/"

echo "Installed GNOME Shell search provider."
echo "Log out and back in (or restart GNOME Shell) to activate."
