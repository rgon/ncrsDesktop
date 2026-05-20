use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

fn main() {
    let uri = match std::env::args().nth(1) {
        Some(u) => u,
        None => {
            eprintln!("usage: ncrs-open nc://open/<user>@<server>/<token>");
            std::process::exit(1);
        }
    };

    let parsed = match ncrs_core::edit_locally::parse_nc_uri(&uri) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ncrs-open: {}", e);
            std::process::exit(1);
        }
    };

    let config = match ncrs_core::config::load_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ncrs-open: config error: {}", e);
            std::process::exit(1);
        }
    };

    let base_url = ncrs_core::notifications::base_url(&config.url);
    let creds = config.credentials().unwrap_or_else(|e| {
        eprintln!("ncrs-open: {}", e);
        std::process::exit(1);
    });

    let data = match ncrs_core::edit_locally::resolve_token(
        &base_url, &creds, &parsed.token,
    ) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ncrs-open: failed to resolve token: {}", e);
            std::process::exit(1);
        }
    };

    let remote_path = &data.path_for_user;
    let rel = remote_path.strip_prefix('/').unwrap_or(remote_path);
    let local_path = config.mount_point.join(rel);

    let sock_path = ncrs_core::ipc::socket_path();
    if let Ok(mut stream) = UnixStream::connect(&sock_path) {
        stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
        let keep_cmd = format!("KEEP {}\n", local_path.display());
        if stream.write_all(keep_cmd.as_bytes()).is_ok() {
            let mut reader = std::io::BufReader::new(stream);
            let mut response = String::new();
            let _ = reader.read_line(&mut response);
        }
    }

    wait_for_file(&local_path, Duration::from_secs(30));

    let status = std::process::Command::new("xdg-open")
        .arg(&local_path)
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => {
            eprintln!("ncrs-open: xdg-open exited with {}", s);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("ncrs-open: failed to run xdg-open: {}", e);
            std::process::exit(1);
        }
    }
}

fn wait_for_file(path: &PathBuf, timeout: Duration) {
    let start = std::time::Instant::now();
    loop {
        if path.exists() && std::fs::metadata(path).map_or(false, |m| m.len() > 0) {
            return;
        }
        if start.elapsed() > timeout {
            eprintln!("ncrs-open: timed out waiting for {}", path.display());
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
