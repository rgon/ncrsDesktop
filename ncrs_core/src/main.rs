mod config;

fn main() {
    env_logger::init();

    let opts = match config::load_config() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("ncrs: {}", e);
            std::process::exit(1);
        }
    };

    if let Err(e) = ncrs_core::mount_ncfs(opts) {
        eprintln!("ncrs: {}", e);
        std::process::exit(1);
    }
}
