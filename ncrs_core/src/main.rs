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

    /// Disable optimistic directory listing (re-list every 10s instead of relying on notify_push)
    #[arg(long)]
    no_optimistic_listing: bool,
}

fn main() {
    env_logger::init();
    let cli = Cli::parse();

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
    if cli.offline {
        opts.offline = true;
    }
    if cli.no_optimistic_listing {
        opts.optimistic_listing = false;
    }

    if let Err(e) = ncrs_core::mount_ncfs(opts) {
        eprintln!("ncrs: {}", e);
        std::process::exit(1);
    }
}
