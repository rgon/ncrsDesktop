use clap::Parser;
use std::path::PathBuf;

// Use jemalloc instead of glibc malloc. This daemon serves the FUSE mount and
// churns large read-ahead, thumbnail and dir-cache-save buffers across its
// worker threads; glibc keeps that freed memory resident in per-thread arenas,
// so one burst (a crawler walking the mount) left it idling at ~680 MB for
// days. Same tuning as ncrs-gui (see its main.rs for the rationale):
// background purge thread, ~1 s decay, narenas capped at 4.
//
// `#[used]` and the `unprefixed_malloc_on_supported_platforms` feature are both
// required, or jemalloc never sees the `malloc_conf` symbol.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(not(target_env = "msvc"))]
#[allow(non_upper_case_globals)]
#[used]
#[export_name = "malloc_conf"]
pub static malloc_conf: &[u8] =
    b"background_thread:true,dirty_decay_ms:1000,muzzy_decay_ms:1000,narenas:4\0";

#[derive(Parser)]
#[command(name = "ncrs", about = "Nextcloud FUSE virtual filesystem")]
struct Cli {
    /// Start in offline mode (serve only cached data, no network)
    #[arg(long)]
    offline: bool,

    /// Path to config file (default: ~/.config/ncrs/config.yaml)
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Override mount point from config
    #[arg(long, value_name = "PATH")]
    mount_point: Option<PathBuf>,

    /// Override WebDAV URL from config
    #[arg(long, value_name = "URL")]
    url: Option<String>,

    /// Override username from config
    #[arg(long, value_name = "USER")]
    username: Option<String>,

    /// [Deprecated: visible in `ps`; prefer auth_command or login flow]
    #[arg(long, value_name = "PASS", hide = true)]
    password: Option<String>,

    /// Disable optimistic directory listing (re-list every 10s instead of relying on notify_push)
    #[arg(long)]
    no_optimistic_listing: bool,

    /// Check the server for changes before showing a directory whose cached
    /// listing is older than this many minutes. Applies while notify-push is not
    /// live; while it is, only a 24h backstop does (0 disables; default 15)
    #[arg(long, value_name = "MINS")]
    dir_cache_max_stale_mins: Option<u64>,

    /// Keep a local cache copy of every file after it is written and uploaded.
    /// When set, the post-upload emblem is a green checkmark; otherwise no emblem is shown.
    #[arg(long)]
    auto_keep_locally_modified_files: bool,

    /// Print the default config template to stdout and exit
    #[arg(long)]
    print_default_config: bool,

    /// Diagnostic: read content-types from stdin (one per line) and print
    /// "<content-type>\t<hex>" of the synthetic MIME-detect bytes for each,
    /// then exit. Needs no config, network or keyring. Used by
    /// scripts/mime_audit.py to verify the intercept against real GLib.
    #[arg(long, hide = true)]
    dump_mime_magic: bool,
}

fn main() {
    // The GUI spawns the daemon without RUST_LOG; bare env_logger::init() would then log nothing.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let cli = Cli::parse();

    if cli.print_default_config {
        print!("{}", ncrs_core::config::DEFAULT_CONFIG);
        return;
    }

    if cli.dump_mime_magic {
        use std::io::BufRead;
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let ct = line.trim();
            if ct.is_empty() {
                continue;
            }
            let hex: String = ncrs_core::mime_magic_bytes(ct)
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect();
            println!("{}\t{}", ct, hex);
        }
        return;
    }

    let mut opts = if let Some(ref path) = cli.config {
        let yaml = match std::fs::read_to_string(path) {
            Ok(y) => y,
            Err(e) => {
                eprintln!("ncrs: cannot read {}: {}", path.display(), e);
                std::process::exit(1);
            }
        };
        match ncrs_core::configuration_parser(&yaml) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("ncrs: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        match ncrs_core::config::load_config() {
            Ok(o) => o,
            Err(e) => {
                eprintln!("ncrs: {}", e);
                std::process::exit(1);
            }
        }
    };

    if let Some(mp) = cli.mount_point {
        opts.mount_point = mp;
    }
    if let Some(username) = cli.username {
        opts.username = Some(username);
    }
    if let Some(url) = cli.url {
        let username = opts.username.as_deref().unwrap_or("");
        let url = ncrs_core::login_flow::normalize_webdav_url(&url, username);
        if let Err(e) = ncrs_core::login_flow::validate_server_scheme(&url, opts.allow_insecure_http) {
            eprintln!("ncrs: {}", e);
            std::process::exit(1);
        }
        opts.url = url;
    }
    if let Some(password) = cli.password {
        eprintln!("ncrs: warning: --password is deprecated (visible in process listing); use auth_command or the login flow instead");
        opts.password = Some(password);
    }
    if cli.offline {
        opts.offline = true;
    }
    if cli.no_optimistic_listing {
        opts.optimistic_listing = false;
    }
    if let Some(mins) = cli.dir_cache_max_stale_mins {
        opts.dir_cache_max_stale_mins = mins;
    }
    if cli.auto_keep_locally_modified_files {
        opts.auto_keep_locally_modified_files = true;
    }

    if let Err(e) = ncrs_core::mount_ncfs(opts, None, None, None, None, None, None) {
        eprintln!("ncrs: {}", e);
        std::process::exit(1);
    }
}
