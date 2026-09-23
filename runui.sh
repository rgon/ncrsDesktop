# cd ncrs-gui/src
# pnpm i
# pnpm run build
# cd ../..

DEST="$HOME/.local/share/nautilus-python/extensions"
mkdir -p "$DEST"
cp shell_integration/file-managers/nautilus/syncstate.py "$DEST/"
nautilus -q 2>/dev/null || true

# GNOME Shell search provider
bash shell_integration/gnome-search/install.sh

cd ncrs-gui
RUST_LOG=info pnpm run tauri dev
# cargo run --bin ncrs-gui
