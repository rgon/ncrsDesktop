use clap::Parser;
use std::path::PathBuf;

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

    /// Override password from config
    #[arg(long, value_name = "PASS")]
    password: Option<String>,

    /// Disable optimistic directory listing (re-list every 10s instead of relying on notify_push)
    #[arg(long)]
    no_optimistic_listing: bool,

    /// Check the server for changes before showing a directory whose cached
    /// listing is older than this many minutes. Relaxed 8x while notify-push is
    /// connected (0 disables; default 15)
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
    env_logger::init();
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
        opts.url = ncrs_core::login_flow::normalize_webdav_url(&url, username);
    }
    if let Some(password) = cli.password {
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

    if let Err(e) = ncrs_core::mount_ncfs(opts, None, None, None, None, None) {
        eprintln!("ncrs: {}", e);
        std::process::exit(1);
    }
}
