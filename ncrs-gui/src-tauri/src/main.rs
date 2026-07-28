// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// Use jemalloc instead of the system (glibc) allocator on Unix. The daemon
// serves the FUSE mount in-process and streams large (up to 64 MB) read-ahead
// and thumbnail buffers across many worker threads. Under glibc those freed
// buffers stayed resident in per-thread arenas — one ~64 MB arena per thread —
// and RSS crept to ~2 GB and never came back. jemalloc, tuned below to purge
// dirty pages ~1 s after they're freed via a background thread, returns that
// memory to the OS promptly.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

// jemalloc runtime config, read from this exported symbol at startup.
// background_thread: a purge thread returns dirty/muzzy pages to the OS.
// *_decay_ms:1000: pages freed by the app are released after ~1 s idle rather
// than the multi-second default, keeping RSS close to the live working set.
//
// `#[used]` and the `unprefixed_malloc_on_supported_platforms` feature are both
// required: without the feature jemalloc reads `_rjem_malloc_conf` and ignores
// this symbol; without `#[used]` the linker garbage-collects it (nothing in Rust
// references it — jemalloc picks it up by symbol name at startup).
#[cfg(not(target_env = "msvc"))]
#[allow(non_upper_case_globals)]
#[used]
#[export_name = "malloc_conf"]
pub static malloc_conf: &[u8] = b"background_thread:true,dirty_decay_ms:1000,muzzy_decay_ms:1000\0";

fn main() {
    ncrs_gui_lib::run()
}
