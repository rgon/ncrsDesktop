//! `ncrs-ctl` — a thin command-line client for the ncrs service's IPC socket.
//!
//! It is what file-manager context menus that cannot talk to a socket
//! themselves call into (Dolphin ServiceMenus, Nemo actions, Thunar custom
//! actions), and a handy scripting/debugging tool. It speaks the published
//! protocol (shell_integration/file-managers/PROTOCOL.md) and nothing else.

use clap::{Parser, Subcommand};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "ncrs-ctl", about = "Control a running ncrs service over its IPC socket")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Always keep these files on this device (downloads them)
    Keep { paths: Vec<PathBuf> },
    /// Free up space: drop the local copies of these files
    Evict { paths: Vec<PathBuf> },
    /// Print (or open with --open) the Nextcloud web URL of a file
    Weburl {
        path: PathBuf,
        #[arg(long)]
        open: bool,
    },
    /// Print the sync status of each path
    Status { paths: Vec<PathBuf> },
    /// List desktop / file-browser integration profiles
    Integrations,
    /// Enable, disable or reset (auto = on if installed) a profile
    IntegrationSet {
        profile: String,
        #[arg(value_parser = ["on", "off", "auto"])]
        mode: String,
    },
    /// List connected clients (file-manager adapters, GUI)
    Clients,
    /// Print the change feed as it happens (WATCH)
    Watch,
    /// Send one raw protocol line and print the reply
    Raw { line: Vec<String> },
}

struct Conn {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Conn {
    fn open() -> Result<Self, String> {
        let sock = ncrs_core::ipc::socket_path();
        let stream = UnixStream::connect(&sock)
            .map_err(|e| format!("cannot reach the ncrs service at {}: {}", sock.display(), e))?;
        stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
        let reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
        let mut c = Conn { stream, reader };
        c.request(&format!("HELLO ncrs-ctl {}", ncrs_core::ipc::PROTOCOL_VERSION))?;
        Ok(c)
    }

    fn request(&mut self, line: &str) -> Result<String, String> {
        writeln!(self.stream, "{}", line).map_err(|e| e.to_string())?;
        self.read_line()
    }

    fn read_line(&mut self) -> Result<String, String> {
        let mut reply = String::new();
        match self.reader.read_line(&mut reply) {
            Ok(0) => Err("the ncrs service closed the connection".into()),
            Ok(_) => Ok(reply.trim_end_matches(['\n', '\r']).to_string()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Absolute form of a user-supplied path, without resolving symlinks (a
/// canonicalize would stat through the mount and could trigger work there).
fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

fn check(reply: String) -> Result<String, String> {
    match reply.strip_prefix("error: ") {
        Some(e) => Err(e.to_string()),
        None if reply == "unknown" => Err("the ncrs service does not know this command (update it)".into()),
        None => Ok(reply),
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let mut c = Conn::open()?;
    match cli.cmd {
        Cmd::Keep { paths } | Cmd::Evict { paths } if paths.is_empty() => {
            return Err("no paths given".into());
        }
        Cmd::Keep { paths } => {
            for p in paths {
                check(c.request(&format!("KEEP {}", absolute(&p).display()))?)?;
            }
        }
        Cmd::Evict { paths } => {
            for p in paths {
                check(c.request(&format!("EVICT {}", absolute(&p).display()))?)?;
            }
        }
        Cmd::Weburl { path, open } => {
            let url = check(c.request(&format!("WEBURL {}", absolute(&path).display()))?)?;
            if open {
                std::process::Command::new("xdg-open")
                    .arg(&url)
                    .spawn()
                    .map_err(|e| format!("xdg-open: {}", e))?;
            } else {
                println!("{}", url);
            }
        }
        Cmd::Status { paths } => {
            for p in paths {
                let abs = absolute(&p);
                let status = c.request(&format!("STATUS {}", abs.display()))?;
                println!("{}\t{}", status, abs.display());
            }
        }
        Cmd::Integrations => {
            let json = check(c.request("INTEGRATIONS")?)?;
            let v: serde_json::Value = serde_json::from_str(&json).map_err(|e| e.to_string())?;
            for p in v.as_array().into_iter().flatten() {
                let s = |k: &str| p.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
                let b = |k: &str| p.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
                let list = |k: &str| {
                    p.get(k)
                        .and_then(|x| x.as_array())
                        .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(","))
                        .unwrap_or_default()
                };
                let adapter = if p.get("adapter_client_ids").and_then(|v| v.as_array()).is_some_and(|a| !a.is_empty()) {
                    format!(
                        " adapter={}{}{}",
                        if b("adapter_installed") { "installed" } else { "missing" },
                        if b("adapter_connected") { ",connected" } else { "" },
                        p.get("adapter_needs_package")
                            .and_then(|v| v.as_str())
                            .map(|pkg| format!(",needs:{pkg}"))
                            .unwrap_or_default()
                    )
                } else {
                    String::new()
                };
                let required_by = list("required_by");
                println!(
                    "{:<8} {:<9} {:<4} mode={:<4} installed={:<5}{}{}",
                    s("kind"),
                    s("id"),
                    if b("enabled") { "on" } else { "off" },
                    s("mode"),
                    b("installed"),
                    adapter,
                    if required_by.is_empty() { String::new() } else { format!(" required-by={}", required_by) },
                );
            }
        }
        Cmd::IntegrationSet { profile, mode } => {
            check(c.request(&format!("INTEGRATION_SET {} {}", profile, mode))?)?;
        }
        Cmd::Clients => println!("{}", check(c.request("CLIENTS")?)?),
        Cmd::Watch => {
            c.stream.set_read_timeout(None).ok();
            println!("{}", c.request("WATCH")?);
            loop {
                let line = c.read_line()?;
                if line != "PING" {
                    println!("{}", line.replace('\t', "\n  "));
                }
            }
        }
        Cmd::Raw { line } => println!("{}", c.request(&line.join(" "))?),
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ncrs-ctl: {}", e);
            ExitCode::FAILURE
        }
    }
}
