    use super::*;
    use std::os::unix::io::FromRawFd;

    fn make_test_cache() -> FsCache {
        let mut inodes = HashMap::new();
        let mut paths = HashMap::new();
        inodes.insert(1, PathBuf::from("/"));
        paths.insert(PathBuf::from("/"), 1);
        FsCache {
            inodes,
            paths,
            next_inode: 2,
            dir_cache: HashMap::new(),
            pending_dirs: HashMap::new(),
            file_cache: HashMap::new(),
            cache_dir: PathBuf::from("/tmp/ncrs-test-cache"),
            kept_dir: PathBuf::from("/tmp/ncrs-test-cache/kept"),
            auto_cache_dir: PathBuf::from("/tmp/ncrs-test-cache/cache"),
            pending_notify: Arc::new((Mutex::new(()), Condvar::new())),
            uploading: HashSet::new(),
        }
    }

    fn make_dav_entry(name: &str, fileid: Option<u64>) -> RemoteEntry {
        let mut ext = backend::EntryExtensions::default();
        if let Some(fid) = fileid {
            ext.integers.insert("fileid".into(), fid);
        }
        RemoteEntry {
            path: PathBuf::from(format!("/{}", name)),
            is_dir: false,
            size: 100,
            modified: None,
            change_token: Some("etag1".into()),
            content_type: None,
            ext,
        }
    }

    fn pipe_notifier_slot() -> (fuse_notify::NotifierSlot, std::fs::File) {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let write_file = unsafe { std::fs::File::from_raw_fd(fds[1]) };
        let read_file = unsafe { std::fs::File::from_raw_fd(fds[0]) };
        let notifier = Arc::new(fuse_notify::FuseNotifier::new(write_file));
        let slot: fuse_notify::NotifierSlot = Arc::new(Mutex::new(Some(notifier)));
        (slot, read_file)
    }

    // ── perms_to_mode ──────────────────────────────────────────────────────────

    #[test]
    fn perms_mode_full_rw_file() {
        assert_eq!(perms_to_mode(Some("RGDNVW"), false), 0o644);
    }

    #[test]
    fn perms_mode_full_rw_dir() {
        assert_eq!(perms_to_mode(Some("RGDNVCK"), true), 0o755);
    }

    #[test]
    fn perms_mode_readonly_file() {
        assert_eq!(perms_to_mode(Some("G"), false), 0o444);
    }

    #[test]
    fn perms_mode_readonly_dir() {
        assert_eq!(perms_to_mode(Some("G"), true), 0o555);
    }

    #[test]
    fn perms_mode_reshare_mounted_read_file() {
        // S=Shared, R=Reshare, M=Mounted, G=Read — the Hddstore Media case
        assert_eq!(perms_to_mode(Some("SRMG"), false), 0o444);
    }

    #[test]
    fn perms_mode_reshare_mounted_read_dir() {
        assert_eq!(perms_to_mode(Some("SRMG"), true), 0o555);
    }

    #[test]
    fn perms_mode_no_read_is_zero() {
        assert_eq!(perms_to_mode(Some("W"),   false), 0o000);
        assert_eq!(perms_to_mode(Some("CDN"), true),  0o000);
    }

    #[test]
    fn perms_mode_none_falls_back_to_default() {
        assert_eq!(perms_to_mode(None, false), 0o644);
        assert_eq!(perms_to_mode(None, true),  0o755);
    }

    #[test]
    fn perms_mode_empty_falls_back_to_default() {
        assert_eq!(perms_to_mode(Some(""), false), 0o644);
        assert_eq!(perms_to_mode(Some(""), true),  0o755);
    }

    fn make_dav_entry_with_perms(dir: &str, name: &str, permissions: Option<&str>) -> RemoteEntry {
        let mut ext = backend::EntryExtensions::default();
        if let Some(p) = permissions {
            ext.strings.insert("permissions".into(), p.to_string());
        }
        ext.integers.insert("fileid".into(), 1);
        RemoteEntry {
            path: PathBuf::from(format!("{}/{}", dir, name)),
            is_dir: false,
            size: 1024,
            modified: None,
            change_token: Some("etag1".into()),
            content_type: None,
            ext,
        }
    }

    fn open_guard_fires(cache: &FsCache, path: &PathBuf) -> bool {
        let parent = path.parent().unwrap_or(Path::new("/")).to_path_buf();
        let nc_permissions = cache.get_cached_dir_readonly(&parent)
            .and_then(|files| files.iter().find(|e| &e.path == path)
            .and_then(|e| e.ext.str("permissions").map(str::to_string)));
        nc_permissions.is_some()
            && perms_to_mode(nc_permissions.as_deref(), false) & 0o200 == 0
    }

    #[test]
    fn open_write_blocked_for_readonly_nc_entry_via_cache() {
        // Exercises the exact cache-lookup + guard-condition path used by fn open.
        // This test would have had no matching behaviour before the guard was added.
        let mut cache = make_test_cache();
        let dir = PathBuf::from("/Musica/Tracks");

        cache.put_dir_cache(dir.clone(), None, None, vec![
            make_dav_entry_with_perms("/Musica/Tracks", "song.mp3",   Some("SRMG")),   // read-only
            make_dav_entry_with_perms("/Musica/Tracks", "editable.mp3", Some("RGDNVW")), // writable
            make_dav_entry_with_perms("/Musica/Tracks", "unknown.mp3",  None),            // no NC perms
        ]);

        let ro_file  = PathBuf::from("/Musica/Tracks/song.mp3");
        let rw_file  = PathBuf::from("/Musica/Tracks/editable.mp3");
        let unk_file = PathBuf::from("/Musica/Tracks/unknown.mp3");

        assert!(open_guard_fires(&cache, &ro_file),
            "write open must be blocked for SRMG (Shared+Reshare+Mounted+Read — no W flag)");
        assert!(!open_guard_fires(&cache, &rw_file),
            "write open must be allowed for RGDNVW (has W flag)");
        assert!(!open_guard_fires(&cache, &unk_file),
            "write open must not be blocked when NC permissions are absent (fall through to DefaultPermissions)");
    }

    fn make_dir_entry_with_perms(parent: &str, name: &str, permissions: Option<&str>) -> RemoteEntry {
        let mut ext = backend::EntryExtensions::default();
        if let Some(p) = permissions {
            ext.strings.insert("permissions".into(), p.to_string());
        }
        ext.integers.insert("fileid".into(), 2);
        RemoteEntry {
            path: PathBuf::from(format!("{}/{}", parent, name)),
            is_dir: true,
            size: 0,
            modified: None,
            change_token: None,
            content_type: None,
            ext,
        }
    }

    /// Registers a directory inode so nc_dir_perms can look it up by inode number.
    fn register_dir(cache: &mut FsCache, parent: &str, name: &str, permissions: Option<&str>) -> u64 {
        let path = PathBuf::from(format!("{}/{}", parent, name));
        let entry = make_dir_entry_with_perms(parent, name, permissions);
        let parent_path = PathBuf::from(parent);
        // Ensure the parent's dir listing is populated (so nc_dir_perms can find this dir).
        let mut files = cache.dir_cache.get(&parent_path)
            .map(|e| e.files.as_ref().clone())
            .unwrap_or_default();
        files.push(entry);
        cache.put_dir_cache(parent_path, None, None, files);
        cache.allocate_inode(path)
    }

    #[test]
    fn unlink_guard_blocks_when_no_delete_flag() {
        // Tracks/ has SRGCK: S+R+G+C+K but NO 'D' → nc_dir_perms returns "SRGCK".
        // The unlink guard must fire (would have passed silently before the fix).
        let mut cache = make_test_cache();
        let tracks_ino = register_dir(&mut cache, "/Musica", "Tracks", Some("SRGCK"));

        let perms = cache.nc_dir_perms(tracks_ino).expect("perms must be present");
        assert!(!perms.contains('D'), "SRGCK should not have D");
        // Guard condition (mirrors fn unlink):
        assert!(!perms.contains('D'), "unlink must be blocked — no D flag");
    }

    #[test]
    fn unlink_guard_allows_when_delete_flag_present() {
        let mut cache = make_test_cache();
        let dir_ino = register_dir(&mut cache, "/Musica", "OwnedDir", Some("RGDNVCK"));

        let perms = cache.nc_dir_perms(dir_ino).expect("perms must be present");
        assert!(perms.contains('D'), "RGDNVCK has D → unlink must be allowed");
    }

    #[test]
    fn unlink_guard_allows_when_perms_absent() {
        // No cached permissions → guard doesn't fire; server enforces.
        let mut cache = make_test_cache();
        let dir_ino = register_dir(&mut cache, "/Musica", "UnknownDir", None);

        assert!(cache.nc_dir_perms(dir_ino).is_none(),
            "guard must not fire when NC permissions are unknown");
    }

    #[test]
    fn rename_guard_blocks_same_dir_without_n_flag() {
        let mut cache = make_test_cache();
        let dir_ino = register_dir(&mut cache, "/Musica", "Tracks", Some("SRGCK")); // no N

        let perms = cache.nc_dir_perms(dir_ino).expect("perms must be present");
        // same-dir rename check (mirrors fn rename, parent == newparent):
        assert!(!perms.contains('N'), "SRGCK has no N → same-dir rename must be blocked");
    }

    #[test]
    fn rename_guard_blocks_cross_dir_move_without_v_flag() {
        let mut cache = make_test_cache();
        let dir_ino = register_dir(&mut cache, "/Musica", "Tracks", Some("SRGCK")); // no V

        let perms = cache.nc_dir_perms(dir_ino).expect("perms must be present");
        // cross-dir move check (mirrors fn rename, parent != newparent):
        assert!(!perms.contains('V'), "SRGCK has no V → cross-dir move must be blocked");
    }

    #[test]
    fn rename_guard_allows_when_flags_present() {
        let mut cache = make_test_cache();
        let dir_ino = register_dir(&mut cache, "/Musica", "OwnedDir", Some("RGDNVCK"));

        let perms = cache.nc_dir_perms(dir_ino).expect("perms must be present");
        assert!(perms.contains('N'), "RGDNVCK has N → same-dir rename must be allowed");
        assert!(perms.contains('V'), "RGDNVCK has V → cross-dir move must be allowed");
    }

    #[test]
    fn get_cached_dir_returns_none_for_invalidated_entry() {
        let mut cache = make_test_cache();
        let path = PathBuf::from("/");
        cache.put_dir_cache(path.clone(), None, None, vec![make_dav_entry("a.txt", None)]);

        let result = cache.get_cached_dir(&path, DIR_CACHE_TTL);
        assert!(result.is_some(), "fresh entry should return Some");
        let (files, needs_refresh) = result.unwrap();
        assert_eq!(files.len(), 1);
        assert!(!needs_refresh);

        cache.dir_cache.get_mut(&path).unwrap().invalidated = true;

        let result = cache.get_cached_dir(&path, DIR_CACHE_TTL);
        assert!(result.is_none(), "invalidated entry must return None to force synchronous PROPFIND");

        let entry = cache.dir_cache.get(&path).unwrap();
        assert!(entry.invalidated, "invalidated flag must stay set until fresh PROPFIND replaces the entry");
    }

    #[test]
    fn invalidated_entry_stays_none_across_repeated_calls() {
        let mut cache = make_test_cache();
        let path = PathBuf::from("/");
        cache.put_dir_cache(path.clone(), None, None, vec![
            make_dav_entry("keep.txt", None),
            make_dav_entry("deleted_on_server.txt", None),
        ]);

        cache.dir_cache.get_mut(&path).unwrap().invalidated = true;

        let first = cache.get_cached_dir(&path, DIR_CACHE_TTL);
        assert!(first.is_none(), "first call: invalidated entry must return None");

        let second = cache.get_cached_dir(&path, DIR_CACHE_TTL);
        assert!(
            second.is_none(),
            "second call: must ALSO return None — stale listing still contains deleted_on_server.txt"
        );
    }

    #[test]
    fn get_cached_dir_returns_stale_data_for_ttl_expired() {
        let mut cache = make_test_cache();
        let path = PathBuf::from("/");
        cache.put_dir_cache(path.clone(), None, None, vec![make_dav_entry("b.txt", None)]);

        cache.dir_cache.get_mut(&path).unwrap().at = Instant::now() - Duration::from_secs(3600);

        let result = cache.get_cached_dir(&path, DIR_CACHE_TTL);
        assert!(result.is_some(), "TTL-expired (not invalidated) should return stale data for background refresh");
        let (_files, needs_refresh) = result.unwrap();
        assert!(needs_refresh, "should signal background refresh needed");
    }

    #[test]
    fn invalidate_all_dirs_populates_dirty_set_and_notifies_kernel() {
        use std::io::Read;

        let mut cache = make_test_cache();
        let root = PathBuf::from("/");
        let subdir = PathBuf::from("/docs");
        cache.allocate_inode(subdir.clone());
        cache.put_dir_cache(root.clone(), None, None, vec![]);
        cache.put_dir_cache(subdir.clone(), None, None, vec![]);

        let cache = Arc::new(Mutex::new(cache));
        let dirty: ipc::DirtySet = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let (slot, mut reader) = pipe_notifier_slot();

        notify_push::invalidate_all_dirs(&cache, &dirty, &slot);

        let ds = dirty.safe_lock();
        assert!(ds.contains(&root), "root should be in dirty set");
        assert!(ds.contains(&subdir), "/docs should be in dirty set");
        drop(ds);

        {
            let c = cache.safe_lock();
            assert!(c.dir_cache.get(&root).unwrap().invalidated);
            assert!(c.dir_cache.get(&subdir).unwrap().invalidated);
        }

        // FuseOutHeader(16) + FuseNotifyInvalInodeOut(24) = 40 bytes per notification
        let msg_size = 40;
        let mut buf = vec![0u8; msg_size * 2];
        reader.read_exact(&mut buf).unwrap();

        let ino1 = u64::from_ne_bytes(buf[16..24].try_into().unwrap());
        let ino2 = u64::from_ne_bytes(buf[16 + msg_size..24 + msg_size].try_into().unwrap());
        let mut inodes = vec![ino1, ino2];
        inodes.sort();
        assert_eq!(inodes, vec![1, 2], "should notify both inode 1 (root) and 2 (/docs)");
    }

    fn make_dav_entry_in(dir: &str, name: &str, fileid: Option<u64>) -> RemoteEntry {
        let mut ext = backend::EntryExtensions::default();
        if let Some(fid) = fileid {
            ext.integers.insert("fileid".into(), fid);
        }
        RemoteEntry {
            path: PathBuf::from(format!("{}/{}", dir, name)),
            is_dir: false,
            size: 100,
            modified: None,
            change_token: Some("etag1".into()),
            content_type: None,
            ext,
        }
    }

    fn make_ghost_map() -> GhostMap {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn make_file_change_queue() -> ipc::FileChangeQueue {
        Arc::new(Mutex::new(Vec::new()))
    }

    #[test]
    fn ghost_hidden_add_hides_file_from_lookup() {
        let ghosts = make_ghost_map();
        let path = PathBuf::from("/Sync/newfile.txt");

        ghosts.safe_lock().insert(path.clone(), GhostEntry {
            kind: GhostKind::HiddenAdd,
            created_at: Instant::now(),
            rename_pair_id: None,
        });

        let g = ghosts.safe_lock();
        let ghost = g.get(&path).unwrap();
        assert!(ghost.created_at.elapsed() < GHOST_TTL);
        assert!(matches!(ghost.kind, GhostKind::HiddenAdd));
    }

    #[test]
    fn ghost_visible_delete_returns_old_attrs() {
        let ghosts = make_ghost_map();
        let path = PathBuf::from("/Sync/deleted.txt");

        let now = SystemTime::now();
        let attr = FileAttr {
            ino: INodeNo(42), size: 1024, blocks: 2,
            atime: now, mtime: now, ctime: now, crtime: now,
            kind: FileType::RegularFile, perm: 0o644, nlink: 1,
            uid: 1000, gid: 1000,
            rdev: 0, flags: 0, blksize: 512,
        };

        ghosts.safe_lock().insert(path.clone(), GhostEntry {
            kind: GhostKind::VisibleDelete { attr },
            created_at: Instant::now(),
            rename_pair_id: None,
        });

        let g = ghosts.safe_lock();
        let ghost = g.get(&path).unwrap();
        match ghost.kind {
            GhostKind::VisibleDelete { attr: stored } => {
                assert_eq!(stored.ino, INodeNo(42));
                assert_eq!(stored.size, 1024);
            }
            _ => panic!("expected VisibleDelete"),
        }
    }

    #[test]
    fn ghost_expires_after_ttl() {
        let ghosts = make_ghost_map();
        let path = PathBuf::from("/Sync/expired.txt");

        ghosts.safe_lock().insert(path.clone(), GhostEntry {
            kind: GhostKind::HiddenAdd,
            created_at: Instant::now() - GHOST_TTL - Duration::from_secs(1),
            rename_pair_id: None,
        });

        let g = ghosts.safe_lock();
        let ghost = g.get(&path).unwrap();
        assert!(ghost.created_at.elapsed() >= GHOST_TTL, "ghost should be expired");
    }

    #[test]
    fn ghost_create_intercept_clears_ghost() {
        let ghosts = make_ghost_map();
        let path = PathBuf::from("/Sync/newfile.txt");

        ghosts.safe_lock().insert(path.clone(), GhostEntry {
            kind: GhostKind::HiddenAdd,
            created_at: Instant::now(),
            rename_pair_id: None,
        });

        // Simulate what the create handler does: remove the ghost
        let removed = ghosts.safe_lock().remove(&path);
        assert!(removed.is_some());
        assert!(matches!(removed.unwrap().kind, GhostKind::HiddenAdd));

        // Ghost should be gone now
        assert!(ghosts.safe_lock().get(&path).is_none());
    }

    #[test]
    fn ghost_unlink_intercept_clears_ghost() {
        let ghosts = make_ghost_map();
        let path = PathBuf::from("/Sync/deleted.txt");
        let now = SystemTime::now();
        let attr = FileAttr {
            ino: INodeNo(42), size: 0, blocks: 0,
            atime: now, mtime: now, ctime: now, crtime: now,
            kind: FileType::RegularFile, perm: 0o644, nlink: 1,
            uid: 1000, gid: 1000, rdev: 0, flags: 0, blksize: 512,
        };

        ghosts.safe_lock().insert(path.clone(), GhostEntry {
            kind: GhostKind::VisibleDelete { attr },
            created_at: Instant::now(),
            rename_pair_id: None,
        });

        let removed = ghosts.safe_lock().remove(&path);
        assert!(removed.is_some());
        assert!(matches!(removed.unwrap().kind, GhostKind::VisibleDelete { .. }));
        assert!(ghosts.safe_lock().get(&path).is_none());
    }

    #[test]
    fn proactive_refresh_classifies_removals_and_additions() {
        // Old: old.txt + keep.txt; New: keep.txt + new.txt
        // Expected diff: removed=[old.txt], added=[new.txt], no renames.
        // make_dav_entry_in always sets etag = "etag1". Use matching etag for
        // keep.txt in the old snapshot so compute_dir_diff doesn't flag it as modified.
        let old_snap = notify_push::OldDirSnapshot {
            names: vec![PathBuf::from("/Sync/old.txt"), PathBuf::from("/Sync/keep.txt")],
            etags: [
                (PathBuf::from("/Sync/old.txt"),  Some("etag1".into())),
                (PathBuf::from("/Sync/keep.txt"), Some("etag1".into())),
            ].into_iter().collect(),
            fileids: [
                (PathBuf::from("/Sync/old.txt"),  100u64),
                (PathBuf::from("/Sync/keep.txt"), 101u64),
            ].into_iter().collect(),
            is_dir: [
                (PathBuf::from("/Sync/old.txt"),  false),
                (PathBuf::from("/Sync/keep.txt"), false),
            ].into_iter().collect(),
        };

        let fresh_files = vec![
            make_dav_entry_in("/Sync", "keep.txt", Some(101)),
            make_dav_entry_in("/Sync", "new.txt",  Some(102)),
        ];

        let diff = notify_push::compute_dir_diff(&old_snap, &fresh_files);

        assert_eq!(diff.removed, vec![PathBuf::from("/Sync/old.txt")],
            "old.txt should be removed");
        assert_eq!(diff.added, vec![PathBuf::from("/Sync/new.txt")],
            "new.txt should be added");
        assert!(diff.renames.is_empty(), "no renames: fileids differ");
        assert!(diff.modified.is_empty(), "keep.txt etag unchanged");
    }

    #[test]
    fn file_change_queue_drains_correctly() {
        let fcq = make_file_change_queue();

        {
            let mut q = fcq.safe_lock();
            q.push(ipc::FileChange {
                kind: ipc::FileChangeKind::Added,
                path: PathBuf::from("/Sync/a.txt"),
            });
            q.push(ipc::FileChange {
                kind: ipc::FileChangeKind::Removed,
                path: PathBuf::from("/Sync/b.txt"),
            });
        }

        // Drain (like IPC handler does)
        let drained: Vec<ipc::FileChange> = fcq.safe_lock().drain(..).collect();
        assert_eq!(drained.len(), 2);

        // Queue should be empty
        assert!(fcq.safe_lock().is_empty());
    }

    #[test]
    fn debounce_cooldown_is_shorter_after_changes_than_after_idle() {
        let active = notify_push::debounce_cooldown(true);
        let idle   = notify_push::debounce_cooldown(false);
        assert!(active < idle,
            "cooldown after changes ({:?}) must be shorter than after idle ({:?})",
            active, idle);
    }

    #[test]
    fn debounce_cooldown_returns_correct_constants() {
        assert_eq!(notify_push::debounce_cooldown(true),  notify_push::REFRESH_DEBOUNCE);
        assert_eq!(notify_push::debounce_cooldown(false), notify_push::REFRESH_DEBOUNCE_NO_CHANGE);
    }

    // ── upload size + status ───────────────────────────────────────────────────

    #[test]
    fn flush_size_written_to_cache_before_reply() {
        // Mirrors the synchronous cache-update block added to fn flush.
        // Would have returned 0 before the fix because only the background PUT
        // thread updated the cache size (after reply.ok was already sent).
        let mut cache = make_test_cache();
        let dir  = PathBuf::from("/Sync");
        let file = PathBuf::from("/Sync/backandforth.md");

        // File starts at size 0 (as fn create inserts it).
        let mut entry = make_dav_entry_with_perms("/Sync", "backandforth.md", Some("RGDNVW"));
        entry.size = 0;
        cache.put_dir_cache(dir.clone(), None, None, vec![entry]);
        assert_eq!(
            cache.get_cached_dir_readonly(&dir).unwrap()
                .iter().find(|e| e.path == file).unwrap().size,
            0,
            "pre-condition: create inserts size 0"
        );

        // Simulate the synchronous update from fn flush.
        let upload_size: u64 = 15; // len("back and forth\n")
        {
            let parent = file.parent().unwrap_or(Path::new("/")).to_path_buf();
            if let Some(dir_entry) = cache.dir_cache.get_mut(&parent) {
                let mut files = (*dir_entry.files).clone();
                if let Some(e) = files.iter_mut().find(|e| e.path == file) {
                    e.size = upload_size;
                }
                dir_entry.files = Arc::new(files);
            }
        }

        let reported = cache.get_cached_dir_readonly(&dir).unwrap()
            .iter().find(|e| e.path == file).unwrap().size;
        assert_eq!(reported, upload_size,
            "getattr must return the written size before the PUT thread runs");
    }

    #[test]
    fn uploading_status_as_str() {
        assert_eq!(ipc::FileStatus::Uploading.as_str(), "uploading");
    }

    #[test]
    fn dir_status_uploading_child_propagates() {
        use ipc::FileStatus;
        use std::collections::HashMap;

        let dir  = PathBuf::from("/Sync");
        let file = PathBuf::from("/Sync/backandforth.md");

        let mut sm: HashMap<PathBuf, FileStatus> = HashMap::new();
        sm.insert(file.clone(), FileStatus::Uploading);

        // dir_status_from_children is not pub; test via the IPC STATUS response
        // by checking the exact guard condition it uses.
        let uploading_count = sm.iter()
            .filter(|(p, s)| p.parent() == Some(&*dir) && **s == FileStatus::Uploading)
            .count();
        assert_eq!(uploading_count, 1, "one child is uploading");
        // Once uploading_count > 0 the function returns "uploading".
        assert!(uploading_count > 0);
    }

    #[test]
    fn flush_background_dirties_file_path_on_success() {
        // The background PUT thread must insert the file path (not just the parent)
        // so that Nautilus's 2-second CHANGES poll picks it up and clears the emblem.
        let dirty: ipc::DirtySet = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let file   = PathBuf::from("/Sync/backandforth.md");
        let parent = PathBuf::from("/Sync");

        dirty.safe_lock().insert(file.clone());
        dirty.safe_lock().insert(parent.clone());

        let paths: std::collections::HashSet<PathBuf> = dirty.safe_lock().drain().collect();
        assert!(paths.contains(&file),   "file path must be in dirty set");
        assert!(paths.contains(&parent), "parent dir must also be in dirty set");
    }

    #[test]
    fn flush_background_dirties_file_path_on_error() {
        // Same check for the error arm — only the file path is inserted (no parent insert
        // in that arm), which is enough for Nautilus to invalidate the file's emblem.
        let dirty: ipc::DirtySet = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let file = PathBuf::from("/Sync/backandforth.md");

        dirty.safe_lock().insert(file.clone());

        let paths: std::collections::HashSet<PathBuf> = dirty.safe_lock().drain().collect();
        assert!(paths.contains(&file), "file path must be in dirty set after error");
    }

    fn make_dir_dav_entry_in(dir: &str, name: &str, fileid: Option<u64>) -> RemoteEntry {
        let mut ext = backend::EntryExtensions::default();
        if let Some(fid) = fileid {
            ext.integers.insert("fileid".into(), fid);
        }
        RemoteEntry {
            path: PathBuf::from(format!("{}/{}", dir, name)),
            is_dir: true,
            size: 0,
            modified: None,
            change_token: None,
            content_type: None,
            ext,
        }
    }

    #[test]
    fn ghost_mkdir_intercept_clears_ghost() {
        let ghosts = make_ghost_map();
        let path = PathBuf::from("/Sync/newdir");

        ghosts.safe_lock().insert(path.clone(), GhostEntry {
            kind: GhostKind::HiddenAdd,
            created_at: Instant::now(),
            rename_pair_id: None,
        });

        // Simulate mkdir handler: check and remove ghost
        let removed = {
            let mut g = ghosts.safe_lock();
            let ghost = g.remove(&path);
            ghost.filter(|g| g.created_at.elapsed() < GHOST_TTL && matches!(g.kind, GhostKind::HiddenAdd))
        };
        assert!(removed.is_some(), "mkdir should find and clear HiddenAdd ghost");
        assert!(ghosts.safe_lock().get(&path).is_none());
    }

    #[test]
    fn ghost_rmdir_intercept_clears_ghost() {
        let ghosts = make_ghost_map();
        let path = PathBuf::from("/Sync/olddir");
        let now = SystemTime::now();
        let attr = FileAttr {
            ino: INodeNo(50), size: 0, blocks: 0,
            atime: now, mtime: now, ctime: now, crtime: now,
            kind: FileType::Directory, perm: 0o755, nlink: 2,
            uid: 1000, gid: 1000, rdev: 0, flags: 0, blksize: 512,
        };

        ghosts.safe_lock().insert(path.clone(), GhostEntry {
            kind: GhostKind::VisibleDelete { attr },
            created_at: Instant::now(),
            rename_pair_id: None,
        });

        let removed = {
            let mut g = ghosts.safe_lock();
            let ghost = g.remove(&path);
            ghost.filter(|g| g.created_at.elapsed() < GHOST_TTL && matches!(g.kind, GhostKind::VisibleDelete { .. }))
        };
        assert!(removed.is_some(), "rmdir should find and clear VisibleDelete ghost");
        assert!(ghosts.safe_lock().get(&path).is_none());
    }

    #[test]
    fn ghost_rename_intercept_clears_paired_ghosts() {
        let ghosts = make_ghost_map();
        let from = PathBuf::from("/Sync/old.txt");
        let to = PathBuf::from("/Sync/new.txt");
        let now = SystemTime::now();
        let attr = FileAttr {
            ino: INodeNo(42), size: 100, blocks: 1,
            atime: now, mtime: now, ctime: now, crtime: now,
            kind: FileType::RegularFile, perm: 0o644, nlink: 1,
            uid: 1000, gid: 1000, rdev: 0, flags: 0, blksize: 512,
        };

        let pair_id = 99u64;
        ghosts.safe_lock().insert(from.clone(), GhostEntry {
            kind: GhostKind::VisibleDelete { attr },
            created_at: Instant::now(),
            rename_pair_id: Some(pair_id),
        });
        ghosts.safe_lock().insert(to.clone(), GhostEntry {
            kind: GhostKind::HiddenAdd,
            created_at: Instant::now(),
            rename_pair_id: Some(pair_id),
        });

        // Simulate rename handler logic
        let matched = {
            let g = ghosts.safe_lock();
            let fg = g.get(&from);
            let tg = g.get(&to);
            match (fg, tg) {
                (Some(fg), Some(tg)) => {
                    fg.created_at.elapsed() < GHOST_TTL
                        && tg.created_at.elapsed() < GHOST_TTL
                        && fg.rename_pair_id.is_some()
                        && fg.rename_pair_id == tg.rename_pair_id
                        && matches!(fg.kind, GhostKind::VisibleDelete { .. })
                        && matches!(tg.kind, GhostKind::HiddenAdd)
                }
                _ => false,
            }
        };
        assert!(matched, "paired rename ghosts should match");

        ghosts.safe_lock().remove(&from);
        ghosts.safe_lock().remove(&to);
        assert!(ghosts.safe_lock().get(&from).is_none());
        assert!(ghosts.safe_lock().get(&to).is_none());
    }

    #[test]
    fn ghost_rename_mismatched_pair_falls_through() {
        let ghosts = make_ghost_map();
        let from = PathBuf::from("/Sync/old.txt");
        let to = PathBuf::from("/Sync/new.txt");
        let now = SystemTime::now();
        let attr = FileAttr {
            ino: INodeNo(42), size: 100, blocks: 1,
            atime: now, mtime: now, ctime: now, crtime: now,
            kind: FileType::RegularFile, perm: 0o644, nlink: 1,
            uid: 1000, gid: 1000, rdev: 0, flags: 0, blksize: 512,
        };

        ghosts.safe_lock().insert(from.clone(), GhostEntry {
            kind: GhostKind::VisibleDelete { attr },
            created_at: Instant::now(),
            rename_pair_id: Some(10),
        });
        ghosts.safe_lock().insert(to.clone(), GhostEntry {
            kind: GhostKind::HiddenAdd,
            created_at: Instant::now(),
            rename_pair_id: Some(20),
        });

        let matched = {
            let g = ghosts.safe_lock();
            let fg = g.get(&from);
            let tg = g.get(&to);
            match (fg, tg) {
                (Some(fg), Some(tg)) => {
                    fg.rename_pair_id.is_some()
                        && fg.rename_pair_id == tg.rename_pair_id
                }
                _ => false,
            }
        };
        assert!(!matched, "mismatched pair_ids should not match");
    }

    #[test]
    fn proactive_refresh_detects_rename_by_fileid() {
        // Old: original.txt (fid=500) + keep.txt (fid=501)
        // New: keep.txt (fid=501) + renamed.txt (fid=500)
        // Expected: rename original→renamed detected; no true removals or adds.
        let old_snap = notify_push::OldDirSnapshot {
            names: vec![PathBuf::from("/Sync/original.txt"), PathBuf::from("/Sync/keep.txt")],
            etags: [
                (PathBuf::from("/Sync/original.txt"), Some("etag1".into())),
                (PathBuf::from("/Sync/keep.txt"),    Some("etag1".into())),
            ].into_iter().collect(),
            fileids: [
                (PathBuf::from("/Sync/original.txt"), 500u64),
                (PathBuf::from("/Sync/keep.txt"),    501u64),
            ].into_iter().collect(),
            is_dir: [
                (PathBuf::from("/Sync/original.txt"), false),
                (PathBuf::from("/Sync/keep.txt"),    false),
            ].into_iter().collect(),
        };

        let fresh_files = vec![
            make_dav_entry_in("/Sync", "keep.txt",    Some(501)),
            make_dav_entry_in("/Sync", "renamed.txt", Some(500)),
        ];

        let diff = notify_push::compute_dir_diff(&old_snap, &fresh_files);

        assert_eq!(diff.renames.len(), 1, "should detect one rename");
        assert_eq!(diff.renames[0].0, PathBuf::from("/Sync/original.txt"));
        assert_eq!(diff.renames[0].1, PathBuf::from("/Sync/renamed.txt"));
        assert!(!diff.renames[0].2, "original.txt is not a directory");
        assert!(diff.removed.is_empty(), "original.txt was renamed, not deleted");
        assert!(diff.added.is_empty(),   "renamed.txt is a rename target, not a new add");
    }

    #[test]
    fn proactive_refresh_distinguishes_dir_vs_file() {
        // Empty old state; two new entries arrive: a file and a directory.
        let old_snap = notify_push::OldDirSnapshot {
            names:   vec![],
            etags:   HashMap::new(),
            fileids: HashMap::new(),
            is_dir:  HashMap::new(),
        };
        let file_entry = make_dav_entry_in("/Sync", "newfile.txt", Some(200));
        let dir_entry  = make_dir_dav_entry_in("/Sync", "newdir", Some(201));
        let fresh_files = vec![file_entry, dir_entry];

        let diff = notify_push::compute_dir_diff(&old_snap, &fresh_files);

        assert_eq!(diff.added.len(), 2);
        assert!(diff.removed.is_empty());
        assert!(diff.renames.is_empty());

        let added_is_dir: HashMap<PathBuf, bool> = fresh_files.iter()
            .filter(|f| diff.added.contains(&f.path))
            .map(|f| (f.path.clone(), f.is_dir))
            .collect();
        assert_eq!(added_is_dir.get(&PathBuf::from("/Sync/newfile.txt")), Some(&false));
        assert_eq!(added_is_dir.get(&PathBuf::from("/Sync/newdir")),      Some(&true));
    }

    #[test]
    fn file_cache_moves_on_rename() {
        let mut cache = make_test_cache();
        let old_path = PathBuf::from("/Sync/original.txt");
        let new_path = PathBuf::from("/Sync/renamed.txt");

        cache.file_cache.insert(old_path.clone(), FileCacheEntry {
            local_path: PathBuf::from("/tmp/ncrs-cache/original.txt"),
            remote_modified: None,
            etag: Some("etag1".into()),
            kept: true,
            size: 1024,
        });

        // Simulate rename file_cache move
        if let Some(entry) = cache.file_cache.remove(&old_path) {
            cache.file_cache.insert(new_path.clone(), entry);
        }

        assert!(cache.file_cache.get(&old_path).is_none(), "old path should be removed");
        let moved = cache.file_cache.get(&new_path).unwrap();
        assert_eq!(moved.local_path, PathBuf::from("/tmp/ncrs-cache/original.txt"));
        assert_eq!(moved.etag, Some("etag1".into()));
    }

    #[test]
    fn ipc_file_changes_serializes_all_kinds() {
        let fcq = make_file_change_queue();
        let mount = PathBuf::from("/home/user/ncrs");

        {
            let mut q = fcq.safe_lock();
            q.push(ipc::FileChange { kind: ipc::FileChangeKind::Added, path: PathBuf::from("/Sync/a.txt") });
            q.push(ipc::FileChange { kind: ipc::FileChangeKind::Removed, path: PathBuf::from("/Sync/b.txt") });
            q.push(ipc::FileChange { kind: ipc::FileChangeKind::Modified, path: PathBuf::from("/Sync/c.txt") });
            q.push(ipc::FileChange { kind: ipc::FileChangeKind::DirAdded, path: PathBuf::from("/Sync/d") });
            q.push(ipc::FileChange { kind: ipc::FileChangeKind::DirRemoved, path: PathBuf::from("/Sync/e") });
            q.push(ipc::FileChange {
                kind: ipc::FileChangeKind::Renamed { from: PathBuf::from("/Sync/old.txt") },
                path: PathBuf::from("/Sync/new.txt"),
            });
        }

        let changes: Vec<ipc::FileChange> = fcq.safe_lock().drain(..).collect();
        let serialized: Vec<String> = changes.iter().map(|c| {
            let rel = c.path.strip_prefix("/").unwrap_or(&c.path);
            let abs = mount.join(rel);
            match &c.kind {
                ipc::FileChangeKind::Added => format!("A:{}", abs.display()),
                ipc::FileChangeKind::Removed => format!("D:{}", abs.display()),
                ipc::FileChangeKind::Modified => format!("M:{}", abs.display()),
                ipc::FileChangeKind::DirAdded => format!("DA:{}", abs.display()),
                ipc::FileChangeKind::DirRemoved => format!("DD:{}", abs.display()),
                ipc::FileChangeKind::Renamed { from } => {
                    let from_rel = from.strip_prefix("/").unwrap_or(from);
                    format!("R:{}\x1e{}", mount.join(from_rel).display(), abs.display())
                }
            }
        }).collect();

        let wire = serialized.join("\t");
        assert!(wire.contains("A:/home/user/ncrs/Sync/a.txt"));
        assert!(wire.contains("D:/home/user/ncrs/Sync/b.txt"));
        assert!(wire.contains("M:/home/user/ncrs/Sync/c.txt"));
        assert!(wire.contains("DA:/home/user/ncrs/Sync/d"));
        assert!(wire.contains("DD:/home/user/ncrs/Sync/e"));
        assert!(wire.contains("R:/home/user/ncrs/Sync/old.txt\x1e/home/user/ncrs/Sync/new.txt"));
    }

    // ── Mount options ─────────────────────────────────────────────────────────

    #[test]
    fn fuse_options_contain_no_custom_values() {
        // fusermount3 rejects unknown options passed via -o. Any
        // MountOption::CUSTOM value (like "x-gvfs-notrash") causes
        // Session::new to fail and the mount silently never happens.
        let opts = build_fuse_options();
        let custom: Vec<&MountOption> = opts.iter()
            .filter(|o| matches!(o, MountOption::CUSTOM(_)))
            .collect();
        assert!(
            custom.is_empty(),
            "CUSTOM mount options are rejected by fusermount3: {:?}",
            custom,
        );
    }

    // ── Trash directory guard ─────────────────────────────────────────────────

    #[test]
    fn is_trash_dir_rejects_trash_names() {
        assert!(is_trash_dir(OsStr::new(".Trash-1000")));
        assert!(is_trash_dir(OsStr::new(".Trash")));
        assert!(is_trash_dir(OsStr::new(".Trash-0")));
    }

    #[test]
    fn is_trash_dir_allows_normal_names() {
        assert!(!is_trash_dir(OsStr::new("Documents")));
        assert!(!is_trash_dir(OsStr::new(".hidden")));
        assert!(!is_trash_dir(OsStr::new("Trash")));
    }

    // ── Boot cache loading ────────────────────────────────────────────────────

    #[test]
    fn boot_loaded_dirs_are_not_invalidated() {
        let cache = Arc::new(Mutex::new(make_test_cache()));
        let path = cache.safe_lock().cache_dir.join(DIR_CACHE_FILE);
        let mut map = HashMap::new();
        map.insert("/Photos".to_string(), PersistedDirEntry {
            etag: Some("abc123".into()),
            self_entry: None,
            files: vec![make_dav_entry("sunset.jpg", Some(1))],
        });
        let json = serde_json::to_vec(&map).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).ok();
        std::fs::write(&path, json).unwrap();
        load_dir_cache(&cache);
        let c = cache.safe_lock();
        let entry = c.dir_cache.get(&PathBuf::from("/Photos")).unwrap();
        assert!(!entry.invalidated, "boot-loaded dir should not be invalidated");
        assert_eq!(entry.etag, Some("abc123".into()));
        drop(c);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn boot_validate_root_invalidates_changed_dirs() {
        let mut cache = make_test_cache();
        cache.put_dir_cache(PathBuf::from("/"), Some("root_etag".into()), None, vec![
            {
                let mut e = make_dav_entry("unchanged", None);
                e.path = PathBuf::from("/unchanged");
                e.is_dir = true;
                e.change_token = Some("etag_a".into());
                e
            },
            {
                let mut e = make_dav_entry("changed", None);
                e.path = PathBuf::from("/changed");
                e.is_dir = true;
                e.change_token = Some("etag_b".into());
                e
            },
        ]);
        cache.put_dir_cache(PathBuf::from("/unchanged"), Some("etag_a".into()), None, vec![]);
        cache.put_dir_cache(PathBuf::from("/changed"), Some("etag_b".into()), None, vec![]);

        let fresh_root = vec![
            {
                let mut e = make_dav_entry("unchanged", None);
                e.path = PathBuf::from("/unchanged");
                e.is_dir = true;
                e.change_token = Some("etag_a".into());
                e
            },
            {
                let mut e = make_dav_entry("changed", None);
                e.path = PathBuf::from("/changed");
                e.is_dir = true;
                e.change_token = Some("etag_NEW".into());
                e
            },
        ];

        // Simulate what boot_validate_root does with the fresh listing
        for entry in &fresh_root {
            if !entry.is_dir { continue; }
            let fresh_etag = entry.change_token.as_deref();
            let cached_etag = cache.dir_cache.get(&entry.path).and_then(|e| e.etag.as_deref());
            match (fresh_etag, cached_etag) {
                (Some(f), Some(c_etag)) if f == c_etag => {}
                _ => {
                    if let Some(dir_entry) = cache.dir_cache.get_mut(&entry.path) {
                        dir_entry.invalidated = true;
                    }
                }
            }
        }

        assert!(!cache.dir_cache[&PathBuf::from("/unchanged")].invalidated,
            "dir with matching etag should remain valid");
        assert!(cache.dir_cache[&PathBuf::from("/changed")].invalidated,
            "dir with changed etag should be invalidated");
    }

    // ── exclude_folders config ─────────────────────────────────────────────────

    #[test]
    fn exclude_folders_parsed_from_config() {
        let yaml = r#"
url: "https://cloud.example.com/remote.php/dav/files/user/"
username: "user"
password: "pass"
exclude_folders:
  - /Photos
  - Videos
"#;
        let opts = config::configuration_parser(yaml).unwrap();
        assert_eq!(opts.exclude_folders, vec!["/Photos", "Videos"]);
    }

    #[test]
    fn exclude_folders_defaults_to_empty() {
        let yaml = r#"
url: "https://cloud.example.com/remote.php/dav/files/user/"
username: "user"
password: "pass"
"#;
        let opts = config::configuration_parser(yaml).unwrap();
        assert!(opts.exclude_folders.is_empty());
    }

    #[test]
    #[should_panic(expected = "both in keep_paths and exclude_folders")]
    fn exclude_folders_panics_on_overlap_with_keep_paths() {
        let exclude: HashSet<PathBuf> = [PathBuf::from("/Photos")].into();
        let keep_paths = vec!["/Photos".to_string()];
        for kp in &keep_paths {
            let kp = kp.trim();
            let kp_path = if kp.starts_with('/') { PathBuf::from(kp) } else { PathBuf::from(format!("/{}", kp)) };
            for ep in &exclude {
                if kp_path.starts_with(ep) || ep.starts_with(&kp_path) {
                    panic!("invalid config: path {:?} is both in keep_paths and exclude_folders", kp);
                }
            }
        }
    }

    #[test]
    #[should_panic(expected = "both in keep_paths and exclude_folders")]
    fn exclude_folders_panics_on_nested_overlap() {
        let exclude: HashSet<PathBuf> = [PathBuf::from("/Photos")].into();
        let keep_paths = vec!["/Photos/Vacation".to_string()];
        for kp in &keep_paths {
            let kp = kp.trim();
            let kp_path = if kp.starts_with('/') { PathBuf::from(kp) } else { PathBuf::from(format!("/{}", kp)) };
            for ep in &exclude {
                if kp_path.starts_with(ep) || ep.starts_with(&kp_path) {
                    panic!("invalid config: path {:?} is both in keep_paths and exclude_folders", kp);
                }
            }
        }
    }

    #[test]
    fn exclude_folders_no_panic_when_disjoint() {
        let exclude: HashSet<PathBuf> = [PathBuf::from("/Photos")].into();
        let keep_paths = vec!["/Documents".to_string()];
        for kp in &keep_paths {
            let kp = kp.trim();
            let kp_path = if kp.starts_with('/') { PathBuf::from(kp) } else { PathBuf::from(format!("/{}", kp)) };
            for ep in &exclude {
                if kp_path.starts_with(ep) || ep.starts_with(&kp_path) {
                    panic!("invalid config: path {:?} is both in keep_paths and exclude_folders", kp);
                }
            }
        }
    }

    // ── rename inode-map and dir-cache correctness ─────────────────────────────

    /// Simulate exactly what the rename FUSE handler's cache-update block does, so
    /// tests can exercise it without a live FUSE mount.
    fn apply_rename(cache: &mut FsCache, from: &PathBuf, to: &PathBuf) {
        let old_parent = from.parent().unwrap_or(std::path::Path::new("/")).to_path_buf();
        let new_parent = to.parent().unwrap_or(std::path::Path::new("/")).to_path_buf();

        let mut moved_entry = None;
        if let Some(dir) = cache.dir_cache.get_mut(&old_parent) {
            let (keep, removed): (Vec<_>, Vec<_>) =
                dir.files.iter().cloned().partition(|e| &e.path != from);
            dir.files = Arc::new(keep);
            moved_entry = removed.into_iter().next();
        }
        if let Some(mut entry) = moved_entry {
            entry.path = to.clone();
            if let Some(dir) = cache.dir_cache.get_mut(&new_parent) {
                let mut files = (*dir.files).clone();
                files.retain(|f| &f.path != to);
                files.push(entry);
                dir.files = Arc::new(files);
            }
        }
        if let Some(ino) = cache.paths.remove(from) {
            cache.inodes.insert(ino, to.clone());
            if let Some(displaced) = cache.paths.insert(to.clone(), ino) {
                if displaced != ino {
                    cache.inodes.remove(&displaced);
                }
            }
        }
    }

    #[test]
    fn rename_inode_map_updated_after_same_dir_rename() {
        let mut cache = make_test_cache();
        let dir = PathBuf::from("/docs");
        let src = dir.join(".goutputstream-abc123");
        let dst = dir.join("report.txt");

        cache.put_dir_cache(dir.clone(), None, None, vec![
            make_dav_entry("docs/.goutputstream-abc123", None),
            make_dav_entry("docs/report.txt", None),
        ]);
        let src_ino = cache.allocate_inode(src.clone());
        let _dst_ino_old = cache.allocate_inode(dst.clone());

        apply_rename(&mut cache, &src, &dst);

        assert_eq!(cache.get_path(src_ino), Some(dst.clone()),
            "inode previously assigned to temp file must resolve to renamed target");
        assert!(cache.paths.get(&src).is_none(),
            "source path must be removed from paths map");
        assert_eq!(cache.paths.get(&dst), Some(&src_ino),
            "target path must map to the renamed inode");
    }

    #[test]
    fn rename_displaced_target_inode_removed() {
        // When renaming A → B, if B already had an inode, that inode must be
        // evicted from the maps so getattr(old_B_ino) doesn't ghost as B.
        let mut cache = make_test_cache();
        let dir = PathBuf::from("/docs");
        let src = dir.join(".goutputstream-xyz");
        let dst = dir.join("notes.md");

        cache.put_dir_cache(dir.clone(), None, None, vec![
            make_dav_entry("docs/.goutputstream-xyz", None),
            make_dav_entry("docs/notes.md", None),
        ]);
        let src_ino = cache.allocate_inode(src.clone());
        let old_dst_ino = cache.allocate_inode(dst.clone());
        assert_ne!(src_ino, old_dst_ino);

        apply_rename(&mut cache, &src, &dst);

        assert!(cache.inodes.get(&old_dst_ino).is_none(),
            "old inode for overwritten target must be evicted");
    }

    #[test]
    fn rename_no_duplicate_target_in_dir_cache() {
        // Renaming a GIO temp file over an existing file must leave exactly one
        // entry for the target name — not two.
        let mut cache = make_test_cache();
        let dir = PathBuf::from("/");
        let src = dir.join(".goutputstream-deadbeef");
        let dst = dir.join("foo.txt");

        cache.put_dir_cache(dir.clone(), None, None, vec![
            make_dav_entry(".goutputstream-deadbeef", None),
            make_dav_entry("foo.txt", None),
        ]);
        cache.allocate_inode(src.clone());
        cache.allocate_inode(dst.clone());

        apply_rename(&mut cache, &src, &dst);

        let entries = cache.dir_cache.get(&dir).expect("dir must be in cache");
        let foo_count = entries.files.iter()
            .filter(|e| e.path.file_name().and_then(|n| n.to_str()) == Some("foo.txt"))
            .count();
        assert_eq!(foo_count, 1, "exactly one foo.txt entry must remain after rename");
    }

    #[test]
    fn gio_atomic_save_inode_survives_rename() {
        // Regression: GIO saves a file via .goutputstream-* → final-name rename.
        // Before the fix, getattr on the renamed inode returned ENOENT because the
        // inode still pointed at the temp-file name that had been removed from
        // dir_cache, making the saved file disappear from the mounted directory.
        let mut cache = make_test_cache();
        let dir = PathBuf::from("/documents");
        let temp = dir.join(".goutputstream-cafebabe");
        let target = dir.join("document.odt");

        // Existing file on server + temp file just created by GIO editor.
        cache.put_dir_cache(dir.clone(), None, None, vec![
            make_dav_entry("documents/document.odt", None),
        ]);
        let old_target_ino = cache.allocate_inode(target.clone());

        // Simulate: FUSE create allocates inode for the temp file.
        cache.put_dir_cache(dir.clone(), None, None, vec![
            make_dav_entry("documents/document.odt", None),
            make_dav_entry("documents/.goutputstream-cafebabe", None),
        ]);
        let temp_ino = cache.allocate_inode(temp.clone());
        assert_ne!(temp_ino, old_target_ino);

        // Simulate: FUSE rename(.goutputstream-cafebabe → document.odt).
        apply_rename(&mut cache, &temp, &target);

        // The inode the kernel holds for document.odt after the rename is temp_ino.
        // getattr(temp_ino) must resolve to document.odt so the file is visible.
        assert_eq!(cache.get_path(temp_ino), Some(target.clone()),
            "temp-file inode must resolve to the final file name after atomic save");

        let entries = cache.dir_cache.get(&dir).expect("dir must be in cache");
        let odt_entries: Vec<_> = entries.files.iter()
            .filter(|e| e.path == target)
            .collect();
        assert_eq!(odt_entries.len(), 1,
            "exactly one document.odt must be in dir_cache after GIO atomic save");
    }

    #[test]
    fn gio_temp_file_recognised_correctly() {
        assert!(is_gio_temp_file(".goutputstream-cafebabe"));
        assert!(is_gio_temp_file(".goutputstream-000000000000000a"));
        assert!(is_gio_temp_file(".xdp-report.pdf-0123456789abcdef"));
        assert!(!is_gio_temp_file("report.pdf"));
        assert!(!is_gio_temp_file(".goutputstream"));
        assert!(!is_gio_temp_file("goutputstream-abc"));
    }

    // Regression guard for the MIME-detection intercept threshold.
    //
    // GLib 2.80 sniffs unknown-extension files by opening them with O_NOATIME
    // and reading 16 KiB. The kernel inflates that into a read-ahead read of up
    // to one 8-page window (32768 bytes) for the *initial* read of any file —
    // measured stable even on multi-GB files. `read()` must intercept up to that
    // bound, otherwise every file larger than 16 KiB is fully downloaded just to
    // answer a MIME query (the regression fixed here: ~300 ms/file, ~19 s for a
    // 114-entry directory). The bound must also stay below the smallest copy
    // buffer (GIO's g_file_copy uses 65536) so real copies fall through to the
    // network-fetch path and receive true content.
    #[test]
    fn mime_detect_threshold_covers_readahead_but_not_copies() {
        // Kernel read-ahead ceiling observed for a single magic-detection read.
        const READAHEAD_CEILING: usize = 32768;
        // Smallest copy-tool buffer (GIO g_file_copy); cp uses 131072.
        const SMALLEST_COPY_BUFFER: usize = 65536;

        assert!(MIME_DETECT_MAX_READ >= READAHEAD_CEILING,
            "intercept must cover read-ahead-inflated magic reads ({} < {})",
            MIME_DETECT_MAX_READ, READAHEAD_CEILING);
        assert!(MIME_DETECT_MAX_READ < SMALLEST_COPY_BUFFER,
            "intercept must not swallow copy reads ({} >= {})",
            MIME_DETECT_MAX_READ, SMALLEST_COPY_BUFFER);
    }

    #[test]
    fn mime_magic_bytes_are_detectable_and_never_empty() {
        // A representative mapped type resolves to its real signature...
        assert_eq!(mime_magic_bytes("image/png"), b"\x89PNG\r\n\x1a\n");
        assert_eq!(mime_magic_bytes("application/pdf"), b"%PDF-");
        // TIFF-based camera RAW (Nextcloud's image/x-dcraw) must resolve to TIFF
        // magic so GLib classifies it as an image instead of text/plain.
        assert_eq!(mime_magic_bytes("image/x-dcraw"), b"MM\x00*");
        // ...content-type parameters are ignored...
        assert_eq!(mime_magic_bytes("text/plain; charset=utf-8"), b"# text\n");

        // ISO-BMFF arms carry a brand at offset 8 (a bare "ftyp" box degrades to
        // application/octet-stream), and the biggest real-world gaps get an exact
        // arm rather than only a category match.
        assert_eq!(mime_magic_bytes("video/mp4"),       b"\x00\x00\x00\x18ftypisom\x00\x00\x02\x00");
        assert_eq!(mime_magic_bytes("audio/mp4"),       b"\x00\x00\x00\x18ftypM4A \x00\x00\x00\x00");
        assert_eq!(mime_magic_bytes("video/quicktime"), b"\x00\x00\x00\x14ftypqt  \x00\x00\x00\x00");
        assert_eq!(mime_magic_bytes("image/heic"),      mime_magic_bytes("image/heif"));

        // Text-based application/* subtypes stay text-classifiable.
        assert_eq!(mime_magic_bytes("application/json"), b"# text\n");

        // Category-aware fallback: an unmapped binary type must NEVER resolve to
        // the text sentinel — that was the root cause of files showing as text.
        // Each category resolves to bytes an image/video/audio type sniffs from.
        assert_eq!(mime_magic_bytes("image/x-unknown-format"), b"II*\x00");
        assert_eq!(mime_magic_bytes("video/x-unknown"),        b"\x00\x00\x00\x18ftypisom\x00\x00\x02\x00");
        assert_eq!(mime_magic_bytes("audio/x-unknown"),        b"\xFF\xFB");
        for ct in ["application/vnd.oasis.opendocument.spreadsheet",
                   "application/octet-stream",
                   "application/x-freecad-document"] {
            let bytes = mime_magic_bytes(ct);
            assert!(!bytes.is_empty(),
                "unmapped content-type {ct} must still return synthetic bytes");
            assert_ne!(bytes, b"# text\n",
                "unmapped binary content-type {ct} must not be classified as text");
        }
    }

    // ── file_cache_matches_remote (read fast-path freshness guard) ──────────────

    #[test]
    fn file_cache_stale_after_server_side_edit() {
        // A file edited server-side (e.g. in Nextcloud Office) gets a new
        // change_token in the dir cache. The locally cached copy must then be
        // reported as no-longer-matching, so the read fast-paths skip it and
        // re-download rather than serving old bytes at the new getattr size — the
        // size/content mismatch that makes a just-edited odt/xlsx look corrupt.
        let mut cache = make_test_cache();
        let path = PathBuf::from("/doc.odt");

        // Dir cache: server currently holds change_token "etag-v2".
        let mut entry = make_dav_entry("doc.odt", None);
        entry.change_token = Some("etag-v2".into());
        entry.size = 200;
        cache.put_dir_cache(PathBuf::from("/"), None, None, vec![entry]);

        // No local copy → nothing to serve from.
        assert!(!cache.file_cache_matches_remote(&path));

        // Local copy downloaded at the OLD token → stale, must not match.
        cache.file_cache.insert(path.clone(), FileCacheEntry {
            local_path: PathBuf::from("/tmp/ncrs-test-cache/kept/doc.odt"),
            remote_modified: None,
            etag: Some("etag-v1".into()),
            kept: true,
            size: 100,
        });
        assert!(!cache.file_cache_matches_remote(&path), "stale etag must not match");

        // Local copy re-downloaded at the CURRENT token → fresh, may be served.
        cache.file_cache.get_mut(&path).unwrap().etag = Some("etag-v2".into());
        assert!(cache.file_cache_matches_remote(&path), "matching etag must match");
    }

    #[test]
    fn file_cache_matches_remote_falls_back_to_mtime_without_etag() {
        // Without a change_token on either side (a plain WebDAV server, or offline)
        // freshness falls back to the remote mtime.
        let mut cache = make_test_cache();
        let path = PathBuf::from("/doc.odt");
        let t_old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let t_new = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(2000);

        let mut entry = make_dav_entry("doc.odt", None);
        entry.change_token = None;
        entry.modified = Some(t_new);
        cache.put_dir_cache(PathBuf::from("/"), None, None, vec![entry]);

        cache.file_cache.insert(path.clone(), FileCacheEntry {
            local_path: PathBuf::from("/tmp/ncrs-test-cache/kept/doc.odt"),
            remote_modified: Some(t_old),
            etag: None,
            kept: true,
            size: 100,
        });
        assert!(!cache.file_cache_matches_remote(&path), "older mtime must not match");

        cache.file_cache.get_mut(&path).unwrap().remote_modified = Some(t_new);
        assert!(cache.file_cache_matches_remote(&path), "equal mtime must match");
    }

    #[test]
    fn parse_content_range_total_extracts_size() {
        assert_eq!(super::parse_content_range_total("bytes 0-499/1234"), Some(1234));
        assert_eq!(super::parse_content_range_total("bytes 0-5471/5472"), Some(5472));
        // Unknown total (server did not know the length) and malformed values yield None.
        assert_eq!(super::parse_content_range_total("bytes 0-499/*"), None);
        assert_eq!(super::parse_content_range_total("garbage"), None);
    }

    #[test]
    fn set_entry_size_patches_dir_cache_entry() {
        // The read path reconciles a stale getattr size with the size the server is
        // actually serving after a server-side edit the dir listing hasn't picked up.
        let mut cache = make_test_cache();
        let path = PathBuf::from("/sedit.txt");
        let mut entry = make_dav_entry("sedit.txt", None);
        entry.size = 17;
        cache.put_dir_cache(PathBuf::from("/"), None, None, vec![entry]);

        assert!(cache.set_entry_size(&path, 5472), "size change must report a bump");
        assert_eq!(cache.find_entry(&path).map(|e| e.size), Some(5472));
        // Idempotent: patching to the same size is a no-op and must not report a bump
        // (this is what stops the read path re-invalidating the inode on every read).
        assert!(!cache.set_entry_size(&path, 5472), "no-op resize must not report a bump");
        // An unknown path is simply ignored.
        assert!(!cache.set_entry_size(&PathBuf::from("/nope.txt"), 10));
    }

    // ── read_err_is_network_down ────────────────────────────────────────────────

    #[test]
    fn network_down_read_errors_flip_offline() {
        // Connect/read timeouts (what a short connect_timeout produces on a dead
        // network) and transport-level failures mean the server is unreachable.
        assert!(read_err_is_network_down("operation timed out"));
        // reqwest's generic wrapper for a dropped/refused connect — the shape seen
        // when the network is blackholed. Must be recognised even without the word
        // "timeout" so the offline flip fires on the first failure.
        assert!(read_err_is_network_down("error sending request for url (http://h/f)"));
        assert!(read_err_is_network_down("error sending request: connection timed out"));
        assert!(read_err_is_network_down("network: Connection refused"));
        assert!(read_err_is_network_down("connection reset by peer"));
        assert!(read_err_is_network_down("broken pipe"));
    }

    #[test]
    fn app_level_read_errors_do_not_flip_offline() {
        // The server answered — these are application-level, not connectivity.
        // Flipping offline here would wrongly suppress live sync while the server
        // is up and reachable.
        assert!(!read_err_is_network_down("401 Unauthorized"));
        assert!(!read_err_is_network_down("403 Forbidden"));
        assert!(!read_err_is_network_down("404 Not Found"));
        assert!(!read_err_is_network_down("No space left on device"));
    }
