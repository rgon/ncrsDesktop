# cd ncrs-gui/src
# pnpm i
# pnpm run build
# cd ../..
cd ncrs-gui
RUST_LOG=info pnpm run tauri dev
# cargo run --bin ncrs-gui
