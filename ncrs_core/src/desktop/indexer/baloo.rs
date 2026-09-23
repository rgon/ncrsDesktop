//! KDE Baloo file indexer.
//!
//! Baloo is on by default in Plasma, crawls `$HOME` — mount included — and
//! extracts content, which on ncrs means downloading every file. It ignores
//! `.trackerignore`, so the mount is added to Baloo's own `exclude folders`.
//!
//! Ownership rule: the service records the folder it added (in the profile
//! store) and only ever removes that entry. An exclusion the user already had
//! is left alone and never recorded, so disabling the profile cannot drop it.
//!
//! `balooctl6 config add|rm excludeFolders` is preferred because it also tells
//! the running `baloo_file` to reconfigure; editing `baloofilerc` directly is
//! the fallback when the tool is missing or fails.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::desktop::detect::DetectEnv;
use crate::desktop::{Component, ComponentCtx, ComponentId, DesktopPolicy};

pub struct Baloo;

const APPLIED_KEY: &str = "baloo.exclude";
const EXCLUDE_KEY: &str = "exclude folders";
const TOOL_TIMEOUT: Duration = Duration::from_secs(10);

impl Component for Baloo {
    fn id(&self) -> ComponentId {
        ComponentId::Baloo
    }

    fn contribute(&self, _p: &mut DesktopPolicy) {}

    fn activate(&self, ctx: &mut ComponentCtx) -> Result<(), String> {
        let tool = balooctl(ctx.env);
        let rc_path = baloofilerc_path();
        if tool.is_none() && !ctx.env.find_binary("baloo_file").is_some() && !rc_path.exists() {
            return Ok(()); // Baloo is not installed: nothing crawls the mount.
        }
        let folder = folder_entry(ctx.mount_point);
        let rc = std::fs::read_to_string(&rc_path).unwrap_or_default();
        if excludes(&rc).iter().any(|e| same_folder(e, &folder)) {
            return Ok(()); // Already excluded (by us earlier, or by the user).
        }
        let via_tool = tool
            .as_deref()
            .is_some_and(|t| run(t, &["config", "add", "excludeFolders", &folder]).is_ok());
        if !via_tool {
            write_rc(&rc_path, &with_exclude(&rc, &folder, true))?;
        }
        log::info!("Baloo: excluded {} from indexing{}", folder, if via_tool { "" } else { " (baloofilerc)" });
        ctx.store.applied.insert(APPLIED_KEY.into(), folder);
        Ok(())
    }

    fn deactivate(&self, ctx: &mut ComponentCtx) -> Result<(), String> {
        let Some(folder) = ctx.store.applied.get(APPLIED_KEY).cloned() else {
            return Ok(()); // We never added one.
        };
        let rc_path = baloofilerc_path();
        let via_tool = balooctl(ctx.env)
            .as_deref()
            .is_some_and(|t| run(t, &["config", "rm", "excludeFolders", &folder]).is_ok());
        let rc = std::fs::read_to_string(&rc_path).unwrap_or_default();
        if excludes(&rc).iter().any(|e| same_folder(e, &folder)) {
            if via_tool {
                log::debug!("Baloo: balooctl left {} in baloofilerc; editing it directly", folder);
            }
            write_rc(&rc_path, &with_exclude(&rc, &folder, false))?;
        }
        log::info!("Baloo: removed the {} exclusion ncrs had added", folder);
        ctx.store.applied.remove(APPLIED_KEY);
        Ok(())
    }
}

fn balooctl(env: &DetectEnv) -> Option<PathBuf> {
    env.find_binary("balooctl6").or_else(|| env.find_binary("balooctl"))
}

fn baloofilerc_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("baloofilerc")
}

/// Baloo stores folders with a trailing slash.
fn folder_entry(mount: &Path) -> String {
    let s = mount.to_string_lossy();
    if s.ends_with('/') { s.into_owned() } else { format!("{}/", s) }
}

fn expand_home(s: &str) -> String {
    match (s.strip_prefix("$HOME"), dirs::home_dir()) {
        (Some(rest), Some(home)) => format!("{}{}", home.display(), rest),
        _ => s.to_string(),
    }
}

fn same_folder(a: &str, b: &str) -> bool {
    expand_home(a).trim_end_matches('/') == expand_home(b).trim_end_matches('/')
}

fn run(tool: &Path, args: &[&str]) -> Result<(), String> {
    let mut child = std::process::Command::new(tool)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("{}: {}", tool.display(), e))?;
    let deadline = Instant::now() + TOOL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("{} exited with {}", tool.display(), status)),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{} timed out", tool.display()));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn write_rc(path: &Path, content: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("ncrs-tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("{}: {}", tmp.display(), e))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {}", path.display(), e))
}

// ── baloofilerc (KConfig INI) editing ───────────────────────────────────────

/// Split a KConfig list value on unescaped commas.
fn split_list(v: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            ',' => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn join_list(items: &[String]) -> String {
    items.iter().map(|i| i.replace('\\', "\\\\").replace(',', "\\,")).collect::<Vec<_>>().join(",")
}

/// `exclude folders` key name, tolerating KConfig flags such as `[$e]`.
fn is_exclude_key(key: &str) -> bool {
    key.trim().split('[').next().unwrap_or("").trim() == EXCLUDE_KEY
}

/// The `exclude folders` entries of the `[General]` group.
pub fn excludes(rc: &str) -> Vec<String> {
    let mut in_general = false;
    for line in rc.lines() {
        let t = line.trim();
        if t.starts_with('[') && !t.contains('=') {
            in_general = t == "[General]";
            continue;
        }
        if in_general {
            if let Some((k, v)) = t.split_once('=') {
                if is_exclude_key(k) {
                    return split_list(v);
                }
            }
        }
    }
    Vec::new()
}

/// `rc` with `folder` added to (or removed from) `[General] exclude folders`,
/// every other line preserved.
pub fn with_exclude(rc: &str, folder: &str, add: bool) -> String {
    let mut lines: Vec<String> = rc.lines().map(str::to_string).collect();
    let mut general: Option<usize> = None;
    let mut in_general = false;
    for i in 0..lines.len() {
        let t = lines[i].trim().to_string();
        if t.starts_with('[') && !t.contains('=') {
            in_general = t == "[General]";
            if in_general {
                general = Some(i);
            }
            continue;
        }
        if !in_general {
            continue;
        }
        if let Some((k, v)) = t.split_once('=') {
            if is_exclude_key(k) {
                let current = split_list(v);
                if add && current.iter().any(|e| same_folder(e, folder)) {
                    return finish(lines);
                }
                let mut items: Vec<String> = current.into_iter().filter(|e| !same_folder(e, folder)).collect();
                if add {
                    items.push(folder.to_string());
                }
                lines[i] = format!("{}={}", k.trim(), join_list(&items));
                return finish(lines);
            }
        }
    }
    if add {
        let entry = format!("{}[$e]={}", EXCLUDE_KEY, join_list(&[folder.to_string()]));
        match general {
            Some(i) => lines.insert(i + 1, entry),
            None => {
                if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                    lines.push(String::new());
                }
                lines.push("[General]".into());
                lines.push(entry);
            }
        }
    }
    finish(lines)
}

fn finish(lines: Vec<String>) -> String {
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const RC: &str = "[Basic Settings]\nIndexing-Enabled=true\n\n[General]\ndbVersion=2\nexclude filters=*~,*.part\nexclude folders[$e]=$HOME/.cache/,/data/tmp/\nfolders[$e]=$HOME/\n";

    #[test]
    fn reads_exclude_folders_only_from_general() {
        assert_eq!(excludes(RC), vec!["$HOME/.cache/", "/data/tmp/"]);
        assert!(excludes("[Other]\nexclude folders=/x/\n").is_empty());
    }

    #[test]
    fn add_then_remove_round_trips_and_keeps_user_entries() {
        let added = with_exclude(RC, "/home/u/Nextcloud/", true);
        assert_eq!(excludes(&added), vec!["$HOME/.cache/", "/data/tmp/", "/home/u/Nextcloud/"]);
        assert!(added.contains("exclude filters=*~,*.part"), "unrelated keys untouched");
        assert!(added.contains("folders[$e]=$HOME/"));
        let removed = with_exclude(&added, "/home/u/Nextcloud/", false);
        assert_eq!(removed, RC);
    }

    #[test]
    fn adding_twice_does_not_duplicate() {
        let once = with_exclude(RC, "/m/", true);
        assert_eq!(with_exclude(&once, "/m", true), once);
    }

    #[test]
    fn creates_the_key_and_group_when_missing() {
        let rc = with_exclude("[Basic Settings]\nIndexing-Enabled=true\n", "/m/", true);
        assert_eq!(excludes(&rc), vec!["/m/"]);
        assert!(rc.starts_with("[Basic Settings]\nIndexing-Enabled=true\n\n[General]\n"));
        let rc2 = with_exclude("", "/m/", true);
        assert_eq!(excludes(&rc2), vec!["/m/"]);
    }

    #[test]
    fn commas_in_paths_are_escaped() {
        let rc = with_exclude("", "/a,b/", true);
        assert_eq!(excludes(&rc), vec!["/a,b/"]);
    }

    #[test]
    fn folder_matching_ignores_trailing_slash() {
        assert!(same_folder("/m/", "/m"));
        assert!(!same_folder("/m/", "/mm/"));
    }
}
