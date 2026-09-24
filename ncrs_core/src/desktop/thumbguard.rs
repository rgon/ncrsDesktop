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

use super::process::{self, ProcessMatch};

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

/// If `pid` (a FUSE request's caller) is a thumbnailer per `matchers`, a
/// short description of it for the log.
pub fn thumbnailer_process(pid: u32, matchers: &[ProcessMatch]) -> Option<String> {
    process::first_match(pid, matchers).map(ProcessMatch::describe)
}

/// `thumbnailer_process` for the FUSE dispatch thread: `None` when deciding
/// would read `/proc` files that can hang it (see `process::try_matches`).
pub fn try_thumbnailer_process(pid: u32, matchers: &[ProcessMatch]) -> Option<Option<String>> {
    process::try_first_match(pid, matchers).map(|m| m.map(ProcessMatch::describe))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::process::eval;
    use crate::desktop::process::tests::fake_proc;

    #[test]
    fn recognises_programs_by_exe_or_truncated_comm() {
        let dir = tempfile::tempdir().unwrap();
        fake_proc(dir.path(), 10, "/usr/bin/gdk-pixbuf-thumbnailer", "gdk-pixbuf-thum", &[], &[]);
        // A Python thumbnailer: exe is the interpreter, comm is the script name.
        fake_proc(dir.path(), 11, "/usr/bin/python3.12", "totem-video-thu", &[], &[]);
        fake_proc(dir.path(), 12, "/usr/bin/nautilus", "nautilus", &[], &[]);
        let gdk = ProcessMatch::Program("gdk-pixbuf-thumbnailer".into());
        let totem = ProcessMatch::Program("totem-video-thumbnailer".into());
        assert!(eval(dir.path(), 10, &gdk));
        assert!(eval(dir.path(), 11, &totem));
        assert!(!eval(dir.path(), 12, &gdk) && !eval(dir.path(), 12, &totem), "the file manager itself reads normally");
    }

    #[test]
    fn recognises_kio_thumbnail_worker_but_not_other_workers() {
        let dir = tempfile::tempdir().unwrap();
        let worker = "/usr/lib/x86_64-linux-gnu/libexec/kf6/kioworker";
        fake_proc(dir.path(), 20, worker, "kioworker",
            &[worker, "/usr/lib/x86_64-linux-gnu/qt6/plugins/kf6/kio/thumbnail.so", "thumbnail", "local:/run/user/1000/dolphinXYZ.1.kioworker.socket"], &[]);
        // The same binary copying a file must not be refused.
        fake_proc(dir.path(), 21, worker, "kioworker",
            &[worker, "/usr/lib/x86_64-linux-gnu/qt6/plugins/kf6/kio/file.so", "file", "local:/run/user/1000/x.socket"], &[]);
        let m = ProcessMatch::CmdlineContains("/kio/thumbnail.so");
        assert!(eval(dir.path(), 20, &m));
        assert!(!eval(dir.path(), 21, &m));
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
