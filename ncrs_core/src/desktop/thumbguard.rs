//! Thumbnailer guard: the mount has exactly one thumbnail source — the server.
//!
//! Desktop thumbnailers (GNOME's per-MIME `.thumbnailer` programs, KIO's
//! thumbnail worker) render previews by reading the file. On an uncached file
//! that is a full download, and pre-filling the freedesktop cache only makes
//! it *rarer* (a cache miss, a race with the prefetch, a size ncrs did not
//! fill), never impossible. So when a thumbnailer process opens an uncached
//! file, the FUSE layer refuses the open without touching the network and
//! fetches the server-rendered preview into the cache instead. The browser then
//! picks that up (the prefetch clears the fail entry and touches the file).
//! A file with no server preview keeps the thumbnailer's failure entry, so it
//! is not retried.
//!
//! Which processes count as thumbnailers is decided per toolkit component, so
//! this only applies while that toolkit's profile is enabled.

use std::path::{Path, PathBuf};

/// How a toolkit's thumbnailer processes are recognised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThumbnailerMatch {
    /// A thumbnailer program, by executable name (also matched against the
    /// kernel's 15-byte `comm`, which is what a script interpreter shows).
    Program(String),
    /// A generic host process whose command line names the thumbnailer
    /// (e.g. KIO's `kioworker …/kio/thumbnail.so thumbnail …`).
    CmdlineContains(&'static str),
}

/// Programs named by the `Exec=` lines of freedesktop `.thumbnailer` files
/// under `<data dir>/thumbnailers/`.
pub fn thumbnailer_programs(data_dirs: &[PathBuf]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for dir in data_dirs {
        let Ok(entries) = std::fs::read_dir(dir.join("thumbnailers")) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("thumbnailer") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else { continue };
            for line in text.lines() {
                if let Some(exec) = line.trim().strip_prefix("Exec=") {
                    if let Some(prog) = exec.split_whitespace().next() {
                        let name = Path::new(prog).file_name().and_then(|n| n.to_str()).unwrap_or(prog);
                        if !name.is_empty() && !out.iter().any(|o| o == name) {
                            out.push(name.to_string());
                        }
                    }
                }
            }
        }
    }
    out.sort();
    out
}

/// If `pid` (a FUSE request's caller, possibly a thread id) is a thumbnailer
/// per `matchers`, a short description of it for the log.
pub fn thumbnailer_process(pid: u32, matchers: &[ThumbnailerMatch]) -> Option<String> {
    classify(Path::new("/proc"), pid, matchers)
}

fn classify(proc_root: &Path, pid: u32, matchers: &[ThumbnailerMatch]) -> Option<String> {
    if pid == 0 || matchers.is_empty() {
        return None;
    }
    let dir = proc_root.join(pid.to_string());
    let exe = std::fs::read_link(dir.join("exe"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    let comm = std::fs::read_to_string(dir.join("comm")).ok().map(|c| c.trim_end().to_string());
    let mut cmdline: Option<String> = None;
    for m in matchers {
        match m {
            ThumbnailerMatch::Program(name) => {
                let comm_name: String = name.chars().take(15).collect();
                if exe.as_deref() == Some(name.as_str()) || comm.as_deref() == Some(comm_name.as_str()) {
                    return Some(name.clone());
                }
            }
            ThumbnailerMatch::CmdlineContains(needle) => {
                let cl = cmdline.get_or_insert_with(|| {
                    std::fs::read(dir.join("cmdline"))
                        .map(|b| String::from_utf8_lossy(&b).replace('\0', " "))
                        .unwrap_or_default()
                });
                if cl.contains(needle) {
                    return Some(format!("{} ({})", comm.as_deref().unwrap_or("?"), needle));
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_proc(root: &Path, pid: u32, exe: &str, comm: &str, cmdline: &[&str]) {
        let d = root.join(pid.to_string());
        std::fs::create_dir_all(&d).unwrap();
        std::os::unix::fs::symlink(exe, d.join("exe")).unwrap();
        std::fs::write(d.join("comm"), format!("{}\n", comm)).unwrap();
        std::fs::write(d.join("cmdline"), cmdline.join("\0")).unwrap();
    }

    #[test]
    fn recognises_programs_by_exe_or_truncated_comm() {
        let dir = tempfile::tempdir().unwrap();
        fake_proc(dir.path(), 10, "/usr/bin/gdk-pixbuf-thumbnailer", "gdk-pixbuf-thum", &[]);
        // A Python thumbnailer: exe is the interpreter, comm is the script name.
        fake_proc(dir.path(), 11, "/usr/bin/python3.12", "totem-video-thu", &[]);
        fake_proc(dir.path(), 12, "/usr/bin/nautilus", "nautilus", &[]);
        let m = vec![
            ThumbnailerMatch::Program("gdk-pixbuf-thumbnailer".into()),
            ThumbnailerMatch::Program("totem-video-thumbnailer".into()),
        ];
        assert_eq!(classify(dir.path(), 10, &m).as_deref(), Some("gdk-pixbuf-thumbnailer"));
        assert_eq!(classify(dir.path(), 11, &m).as_deref(), Some("totem-video-thumbnailer"));
        assert_eq!(classify(dir.path(), 12, &m), None, "the file manager itself reads normally");
    }

    #[test]
    fn recognises_kio_thumbnail_worker_but_not_other_workers() {
        let dir = tempfile::tempdir().unwrap();
        fake_proc(dir.path(), 20, "/usr/lib/x86_64-linux-gnu/libexec/kf6/kioworker", "kioworker",
            &["/usr/lib/x86_64-linux-gnu/libexec/kf6/kioworker", "/usr/lib/x86_64-linux-gnu/qt6/plugins/kf6/kio/thumbnail.so", "thumbnail", "local:/run/user/1000/dolphinXYZ.1.kioworker.socket"]);
        // The same binary copying a file must not be refused.
        fake_proc(dir.path(), 21, "/usr/lib/x86_64-linux-gnu/libexec/kf6/kioworker", "kioworker",
            &["/usr/lib/x86_64-linux-gnu/libexec/kf6/kioworker", "/usr/lib/x86_64-linux-gnu/qt6/plugins/kf6/kio/file.so", "file", "local:/run/user/1000/x.socket"]);
        let m = vec![ThumbnailerMatch::CmdlineContains("/kio/thumbnail.so")];
        assert!(classify(dir.path(), 20, &m).is_some());
        assert_eq!(classify(dir.path(), 21, &m), None);
    }

    #[test]
    fn unknown_or_vanished_process_is_not_a_thumbnailer() {
        let dir = tempfile::tempdir().unwrap();
        let m = vec![ThumbnailerMatch::Program("x".into())];
        assert_eq!(classify(dir.path(), 999, &m), None);
        assert_eq!(classify(dir.path(), 0, &m), None);
    }

    #[test]
    fn reads_programs_from_thumbnailer_files() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("thumbnailers");
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(t.join("gdk-pixbuf-thumbnailer.thumbnailer"),
            "[Thumbnailer Entry]\nTryExec=/usr/bin/gdk-pixbuf-thumbnailer\nExec=/usr/bin/gdk-pixbuf-thumbnailer -s %s %u %o\nMimeType=image/png;\n").unwrap();
        std::fs::write(t.join("totem.thumbnailer"), "[Thumbnailer Entry]\nExec=totem-video-thumbnailer -s %s %u %o\n").unwrap();
        std::fs::write(t.join("README"), "Exec=not-a-thumbnailer\n").unwrap();
        assert_eq!(thumbnailer_programs(&[dir.path().to_path_buf()]), vec!["gdk-pixbuf-thumbnailer", "totem-video-thumbnailer"]);
    }
}
