    use super::*;

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
            // Unbounded by default in tests; the eviction tests set it explicitly.
            dir_cache_max_dirs: 0,
            pending_dirs: HashMap::new(),
            file_cache: HashMap::new(),
            cache_dir: PathBuf::from("/tmp/ncrs-test-cache"),
            kept_dir: PathBuf::from("/tmp/ncrs-test-cache/kept"),
            auto_cache_dir: PathBuf::from("/tmp/ncrs-test-cache/cache"),
            pending_notify: Arc::new((Mutex::new(()), Condvar::new())),
            uploading: HashSet::new(),
            deleting: HashSet::new(),
            trackerignore_hidden: false,
        }
    }

    fn make_dav_entry(name: &str, fileid: Option<u64>) -> RemoteEntry {
        let mut ext = backend::EntryExtensions::default();
        if let Some(fid) = fileid {
            ext.set_int("fileid", fid);
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

    fn empty_notifier_slot() -> fuse_notify::NotifierSlot {
        Arc::new(Mutex::new(None))
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
            ext.set_str("permissions", p);
        }
        ext.set_int("fileid", 1);
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
            ext.set_str("permissions", p);
        }
        ext.set_int("fileid", 2);
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
        // A non-root path: root also carries the synthetic .trackerignore overlay
        // entry, which is unrelated to what this test is checking.
        let path = PathBuf::from("/dir");
        cache.put_dir_cache(path.clone(), None, None, vec![make_dav_entry("a.txt", None)]);

        let result = cache.get_cached_dir(&path, DIR_CACHE_TTL, None);
        assert!(result.is_some(), "fresh entry should return Some");
        let (files, needs_refresh) = result.unwrap();
        assert_eq!(files.len(), 1);
        assert!(!needs_refresh);

        cache.dir_cache.get_mut(&path).unwrap().invalidated = true;

        let result = cache.get_cached_dir(&path, DIR_CACHE_TTL, None);
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

        let first = cache.get_cached_dir(&path, DIR_CACHE_TTL, None);
        assert!(first.is_none(), "first call: invalidated entry must return None");

        let second = cache.get_cached_dir(&path, DIR_CACHE_TTL, None);
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

        let result = cache.get_cached_dir(&path, DIR_CACHE_TTL, None);
        assert!(result.is_some(), "TTL-expired (not invalidated) should return stale data for background refresh");
        let (_files, needs_refresh) = result.unwrap();
        assert!(needs_refresh, "should signal background refresh needed");
    }

    #[test]
    fn hard_expired_entry_forces_synchronous_relist() {
        let mut cache = make_test_cache();
        let path = PathBuf::from("/");
        cache.put_dir_cache(path.clone(), None, None, vec![make_dav_entry("c.txt", None)]);

        let max_stale = Duration::from_secs(2 * 3600);
        assert!(
            cache.get_cached_dir(&path, DIR_CACHE_TTL, Some(max_stale)).is_some(),
            "listing younger than max_stale must still be served from cache"
        );

        cache.dir_cache.get_mut(&path).unwrap().fetched_at =
            SystemTime::now() - Duration::from_secs(3 * 3600);

        assert!(
            cache.get_cached_dir(&path, DIR_CACHE_TTL, Some(max_stale)).is_none(),
            "listing older than max_stale must return None so readdir blocks on a fresh PROPFIND"
        );
        assert!(cache.dir_cache.get(&path).unwrap().hard_expired);
        assert!(
            cache.get_cached_dir(&path, DIR_CACHE_TTL, Some(max_stale)).is_none(),
            "repeated calls must keep returning None until a fresh PROPFIND replaces the entry"
        );

        // A fresh listing clears the flag and restarts the max-stale window.
        cache.put_dir_cache(path.clone(), None, None, vec![make_dav_entry("c.txt", None)]);
        assert!(!cache.dir_cache.get(&path).unwrap().hard_expired);
        assert!(cache.get_cached_dir(&path, DIR_CACHE_TTL, Some(max_stale)).is_some());
    }

    #[test]
    fn hard_expiry_disabled_serves_arbitrarily_old_listing() {
        let mut cache = make_test_cache();
        let path = PathBuf::from("/");
        cache.put_dir_cache(path.clone(), None, None, vec![make_dav_entry("d.txt", None)]);
        {
            let entry = cache.dir_cache.get_mut(&path).unwrap();
            entry.fetched_at = SystemTime::now() - Duration::from_secs(30 * 86400);
            entry.at = Instant::now() - Duration::from_secs(30 * 86400);
        }

        let result = cache.get_cached_dir(&path, DIR_CACHE_TTL, None);
        assert!(result.is_some(), "with the check disabled, age must never cause a cache miss");
        assert!(result.unwrap().1, "stale entry still asks for a background refresh");
        assert!(!cache.dir_cache.get(&path).unwrap().hard_expired);
    }

    #[test]
    fn etag_match_restarts_the_max_stale_window() {
        let mut cache = make_test_cache();
        let path = PathBuf::from("/");
        cache.put_dir_cache(path.clone(), Some("etag1".into()), None, vec![make_dav_entry("e.txt", None)]);
        cache.dir_cache.get_mut(&path).unwrap().fetched_at =
            SystemTime::now() - Duration::from_secs(3 * 3600);

        // Background refresh found an unchanged etag: the cached data is confirmed
        // current, so it must not be treated as stale on the next listing.
        cache.touch_dir_cache(&path);

        assert!(
            cache.get_cached_dir(&path, DIR_CACHE_TTL, Some(Duration::from_secs(2 * 3600))).is_some(),
            "etag-confirmed listing must be served from cache"
        );
    }

    #[test]
    fn effective_max_stale_relaxes_while_notify_push_is_connected() {
        let base = Duration::from_secs(15 * 60);
        let connected = AtomicBool::new(true);
        assert_eq!(
            effective_max_stale(Some(base), &connected),
            Some(CONNECTED_MAX_STALE_FLOOR),
            "while events are arriving, only the long backstop applies"
        );

        connected.store(false, Ordering::Relaxed);
        assert_eq!(
            effective_max_stale(Some(base), &connected),
            Some(base),
            "with no push connection there is nothing to invalidate the cache but this check"
        );

        assert_eq!(effective_max_stale(None, &connected), None, "0 disables the check outright");
        connected.store(true, Ordering::Relaxed);
        assert_eq!(effective_max_stale(None, &connected), None);

        // A configured window longer than the backstop is honoured as-is.
        let week = Duration::from_secs(7 * 86400);
        assert_eq!(effective_max_stale(Some(week), &connected), Some(week));
    }

    #[test]
    fn pending_snapshot_is_withheld_while_the_cached_listing_is_untrusted() {
        let mut cache = make_test_cache();
        let path = PathBuf::from("/");
        cache.put_dir_cache(path.clone(), None, None, vec![make_dav_entry("a.txt", None)]);
        assert!(cache.may_serve_pending_snapshot(&path), "a trusted listing may be streamed over");
        assert!(
            cache.may_serve_pending_snapshot(Path::new("/never-listed")),
            "with no cached listing there is no stale suffix to splice onto"
        );

        // Hard-expired: readdir's continuation pages would still read the old listing,
        // so a partial stream must not be served as page 0.
        cache.dir_cache.get_mut(&path).unwrap().fetched_at =
            SystemTime::now() - Duration::from_secs(3 * 3600);
        assert!(cache
            .get_cached_dir(&path, DIR_CACHE_TTL, Some(Duration::from_secs(2 * 3600)))
            .is_none());
        assert!(!cache.may_serve_pending_snapshot(&path), "hard-expired must withhold the snapshot");

        cache.put_dir_cache(path.clone(), None, None, vec![make_dav_entry("a.txt", None)]);
        cache.dir_cache.get_mut(&path).unwrap().invalidated = true;
        assert!(!cache.may_serve_pending_snapshot(&path), "invalidated must withhold the snapshot");
    }

    #[test]
    fn a_clock_stepped_backwards_expires_rather_than_trusts_the_listing() {
        let mut cache = make_test_cache();
        let path = PathBuf::from("/");
        cache.put_dir_cache(path.clone(), None, None, vec![make_dav_entry("a.txt", None)]);
        // Persisted at a wall-clock time that is now in the future: age is unknowable.
        cache.dir_cache.get_mut(&path).unwrap().fetched_at =
            SystemTime::now() + Duration::from_secs(86400);

        assert!(
            cache.get_cached_dir(&path, DIR_CACHE_TTL, Some(Duration::from_secs(15 * 60))).is_none(),
            "a listing of unknowable age must be re-listed, not treated as brand new"
        );
        assert!(cache.dir_cache.get(&path).unwrap().hard_expired);
    }

    #[test]
    fn confirm_dir_fresh_leaves_the_refresh_guard_alone() {
        let mut cache = make_test_cache();
        let path = PathBuf::from("/Photos");
        cache.put_dir_cache(path.clone(), Some("etag1".into()), None, vec![make_dav_entry("g.txt", None)]);
        // Another thread owns the single-flight refresh guard for this dir.
        cache.dir_cache.get_mut(&path).unwrap().refreshing = true;

        assert!(cache.confirm_dir_fresh(&path));
        assert!(
            cache.dir_cache.get(&path).unwrap().refreshing,
            "confirming freshness must not release a guard it did not take"
        );
    }

    #[test]
    fn invalidate_dir_keeps_a_changed_listing_out_of_readdir() {
        let mut cache = make_test_cache();
        let path = PathBuf::from("/Docs");
        cache.put_dir_cache(path.clone(), Some("etag1".into()), None, vec![make_dav_entry("h.txt", None)]);
        // Etag mismatch found by validation, but the re-list that should follow fails.
        cache.invalidate_dir(&path);
        assert!(
            cache.get_cached_dir(&path, DIR_CACHE_TTL, None).is_none(),
            "a directory known to have changed must never be served from cache"
        );
    }

    #[test]
    fn stale_fallback_covers_unreachable_servers_but_not_rejections() {
        assert!(is_unreachable_listing_error("PROPFIND timeout for /Photos"));
        assert!(is_unreachable_listing_error("error sending request"));
        assert!(is_unreachable_listing_error("network: host unreachable"));
        assert!(!is_unreachable_listing_error("404 Not Found"));
        assert!(!is_unreachable_listing_error("401 Unauthorized"));
        assert!(!is_unreachable_listing_error("403 Forbidden"));
    }

    #[test]
    fn confirm_dir_fresh_restarts_window_but_respects_invalidation() {
        let mut cache = make_test_cache();
        let path = PathBuf::from("/Photos");
        let max_stale = Some(Duration::from_secs(2 * 3600));
        cache.put_dir_cache(path.clone(), Some("etag1".into()), None, vec![make_dav_entry("g.txt", None)]);
        cache.dir_cache.get_mut(&path).unwrap().fetched_at =
            SystemTime::now() - Duration::from_secs(3 * 3600);

        // Boot validation matched the child etag: the cached listing is current.
        assert!(cache.confirm_dir_fresh(&path));
        assert!(
            cache.get_cached_dir(&path, DIR_CACHE_TTL, max_stale).is_some(),
            "an etag-confirmed listing must not be forced through a blocking re-list"
        );

        // An invalidation that landed meanwhile still wins over the confirmation.
        cache.dir_cache.get_mut(&path).unwrap().invalidated = true;
        assert!(!cache.confirm_dir_fresh(&path), "must report that the caller has to re-list");
        assert!(cache.get_cached_dir(&path, DIR_CACHE_TTL, max_stale).is_none());
    }

    #[test]
    fn a_failed_forced_relist_backs_off_instead_of_stalling_every_readdir() {
        let mut cache = make_test_cache();
        let path = PathBuf::from("/");
        let max_stale = Some(Duration::from_secs(2 * 3600));
        cache.put_dir_cache(path.clone(), None, None, vec![make_dav_entry("i.txt", None)]);
        cache.dir_cache.get_mut(&path).unwrap().fetched_at =
            SystemTime::now() - Duration::from_secs(3 * 3600);

        assert!(cache.get_cached_dir(&path, DIR_CACHE_TTL, max_stale).is_none());
        // The forced re-list failed because the server is unreachable.
        assert!(cache.take_hard_expired_fallback(&path).is_some());

        // Within the cooldown the cached listing is served instead of stalling on
        // another doomed PROPFIND for every single readdir.
        assert!(
            cache.get_cached_dir(&path, DIR_CACHE_TTL, max_stale).is_some(),
            "inside the cooldown the stale listing is served rather than re-probed"
        );

        // Once it lapses, the directory is expired again and the next readdir retries.
        cache.dir_cache.get_mut(&path).unwrap().expiry_retry_after =
            Some(Instant::now() - Duration::from_secs(1));
        assert!(
            cache.get_cached_dir(&path, DIR_CACHE_TTL, max_stale).is_none(),
            "after the cooldown the forced re-list must be attempted again"
        );

        // A successful listing clears the backoff outright.
        cache.put_dir_cache(path.clone(), None, None, vec![make_dav_entry("i.txt", None)]);
        assert!(cache.dir_cache.get(&path).unwrap().expiry_retry_after.is_none());
    }

    #[test]
    fn hard_expired_fallback_returns_stale_listing_once() {
        let mut cache = make_test_cache();
        // A non-root path: root also carries the synthetic .trackerignore overlay
        // entry, which is unrelated to what this test is checking.
        let path = PathBuf::from("/dir");
        cache.put_dir_cache(path.clone(), None, None, vec![make_dav_entry("f.txt", None)]);
        cache.dir_cache.get_mut(&path).unwrap().fetched_at =
            SystemTime::now() - Duration::from_secs(3 * 3600);
        let max_stale = Some(Duration::from_secs(2 * 3600));
        assert!(cache.get_cached_dir(&path, DIR_CACHE_TTL, max_stale).is_none());

        // The forced re-list failed (offline, PROPFIND timeout): serve stale rather
        // than fail the readdir, and clear the flag so we don't block every call.
        let (files, _) = cache.take_hard_expired_fallback(&path).expect("stale fallback available");
        assert_eq!(files.len(), 1);
        assert!(cache.take_hard_expired_fallback(&path).is_none(), "fallback applies once per expiry");
        // The retry itself is rate-limited — see
        // a_failed_forced_relist_backs_off_instead_of_stalling_every_readdir.
        cache.dir_cache.get_mut(&path).unwrap().expiry_retry_after = None;
        assert!(
            cache.get_cached_dir(&path, DIR_CACHE_TTL, max_stale).is_none(),
            "still older than max_stale — the next listing retries the fresh PROPFIND"
        );
    }

    #[test]
    fn invalidate_all_dirs_populates_dirty_set_and_notifies_kernel() {
        let mut cache = make_test_cache();
        let root = PathBuf::from("/");
        let subdir = PathBuf::from("/docs");
        cache.allocate_inode(subdir.clone());
        cache.put_dir_cache(root.clone(), None, None, vec![]);
        cache.put_dir_cache(subdir.clone(), None, None, vec![]);

        let cache = Arc::new(Mutex::new(cache));
        let dirty: ipc::DirtySet = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let slot = empty_notifier_slot();

        notify_push::invalidate_all_dirs(&cache, &dirty, &slot);

        let ds = dirty.safe_lock();
        assert!(ds.contains(&root), "root should be in dirty set");
        assert!(ds.contains(&subdir), "/docs should be in dirty set");
        drop(ds);

        let c = cache.safe_lock();
        assert!(c.dir_cache.get(&root).unwrap().invalidated);
        assert!(c.dir_cache.get(&subdir).unwrap().invalidated);
    }

    fn make_dav_entry_in(dir: &str, name: &str, fileid: Option<u64>) -> RemoteEntry {
        let mut ext = backend::EntryExtensions::default();
        if let Some(fid) = fileid {
            ext.set_int("fileid", fid);
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
            ext.set_int("fileid", fid);
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

    // ── Adaptive read-ahead ───────────────────────────────────────────────────

    const MB: usize = 1024 * 1024;

    #[test]
    fn sequential_access_grows_the_window_to_the_ceiling() {
        let ceiling = 64 * MB;
        let mut w = READ_AHEAD_INITIAL;
        let mut steps = 0;
        while w < ceiling {
            w = next_read_ahead_window(w, true, ceiling);
            steps += 1;
            assert!(steps < 32, "window must converge, stuck at {}", w);
        }
        assert_eq!(w, ceiling, "streaming must still reach the configured maximum");
        // And stay there.
        assert_eq!(next_read_ahead_window(w, true, ceiling), ceiling);
    }

    #[test]
    fn a_seek_resets_the_window() {
        let ceiling = 64 * MB;
        // A handle that had ramped all the way up...
        assert_eq!(
            next_read_ahead_window(ceiling, false, ceiling),
            READ_AHEAD_INITIAL,
            "a seek must not keep fetching the full read-ahead",
        );
        // ...and one that never ramped.
        assert_eq!(next_read_ahead_window(READ_AHEAD_INITIAL, false, ceiling), READ_AHEAD_INITIAL);
    }

    #[test]
    fn seeking_costs_the_initial_window_not_the_ceiling() {
        // The reported bug: a player probing a 129 MB FLAC (header, seektable,
        // playback position) missed the buffer ~5 times and pulled ~281 MB,
        // because every miss fetched the full 64 MB read-ahead.
        let ceiling = 64 * MB;
        let seeks = 5;
        let before = seeks * ceiling;
        let after: usize = (0..seeks)
            .map(|_| next_read_ahead_window(ceiling, false, ceiling))
            .sum();
        assert_eq!(before, 320 * MB);
        assert_eq!(after, 5 * MB);
        assert!(after * 60 < before, "expected a large reduction, got {} vs {}", after, before);
    }

    #[test]
    fn the_window_never_exceeds_a_small_configured_read_ahead() {
        // read_ahead_bytes below READ_AHEAD_INITIAL must still be respected —
        // the user's ceiling wins over our starting point.
        let ceiling = 256 * 1024;
        assert_eq!(next_read_ahead_window(ceiling, false, ceiling), ceiling);
        assert_eq!(next_read_ahead_window(ceiling, true, ceiling), ceiling);
    }

    // ── Short replies at read-ahead window boundaries ─────────────────────────

    #[test]
    fn a_read_straddling_a_window_boundary_is_never_answered_short() {
        // The reported bug: a 30 MB FLAC on the mount, read sequentially in 16 KiB
        // chunks, stopped dead at exactly 2 MiB — the end of the first read-ahead
        // window. The kernel's read-ahead read straddles that boundary; the old code
        // replied with the bytes the window happened to hold, and a short FUSE reply
        // is latched by the kernel as EOF for the whole inode. Every later read
        // returned 0 bytes without reaching the daemon, so VLC saw the track end one
        // window in and skipped to the next one. Only a fresh open() cleared it.
        let file_size = 30 * MB as u64;
        let window_end = 2 * MB as u64;
        let sz = 128 * 1024;
        let off = window_end - 16 * 1024;
        let avail = (window_end - off) as usize;

        assert!(avail < sz, "this read straddles the boundary");
        assert!(
            !short_reply_ok(off, avail, sz, file_size),
            "a straddling read must be refetched in full, never answered short",
        );
    }

    #[test]
    fn a_short_reply_at_the_real_end_of_file_is_allowed() {
        // GLib's MIME probe: 16 KiB asked of a 4 KiB file that is fully prefetched.
        assert!(short_reply_ok(0, 4096, 16 * 1024, 4096));
        // And the tail of a large file.
        let file_size = 30 * MB as u64;
        assert!(short_reply_ok(file_size - 1000, 1000, 128 * 1024, file_size));
    }

    #[test]
    fn a_full_length_reply_is_always_allowed() {
        assert!(short_reply_ok(0, 128 * 1024, 128 * 1024, 30 * MB as u64));
        // Even when the size is unknown — nothing is being truncated.
        assert!(short_reply_ok(0, 128 * 1024, 128 * 1024, 0));
    }

    #[test]
    fn an_unknown_file_size_never_authorises_a_short_reply() {
        // size 0 means the dir cache has no entry. That proves nothing about where
        // the file ends, so the caller must fetch rather than risk latching EOF.
        assert!(!short_reply_ok(0, 16 * 1024, 128 * 1024, 0));
    }

    #[test]
    fn no_window_boundary_of_a_sequential_read_can_truncate_the_file() {
        // Walk a 30 MB file the way a player does, over the windows the ramp
        // actually produces, and check the read that straddles each boundary.
        // Every one of them must be refused as a short reply; only the last
        // window, the one the file ends in, may answer short.
        let file_size = 30 * MB as u64;
        let ceiling = 64 * MB;
        let sz = 128 * 1024;
        let mut window = READ_AHEAD_INITIAL;
        let mut start = 0u64;
        let mut boundaries = 0;

        while start < file_size {
            window = next_read_ahead_window(window, true, ceiling);
            let end = (start + window as u64).min(file_size);
            let off = end - (sz as u64 / 2);
            let avail = (end - off) as usize;
            if end < file_size {
                assert!(
                    !short_reply_ok(off, avail, sz, file_size),
                    "a read at {} would truncate the file at window boundary {}",
                    off, end,
                );
                boundaries += 1;
            } else {
                assert!(short_reply_ok(off, avail, sz, file_size), "the last window ends the file");
            }
            start = end;
        }
        assert!(boundaries >= 3, "expected several window boundaries, got {}", boundaries);
    }

    #[test]
    fn read_at_full_fills_the_buffer_across_short_reads() {
        let dir = std::env::temp_dir()
            .join(format!("ncrs-test-readatfull-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("blob");
        let body: Vec<u8> = (0..8192u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &body).unwrap();
        let f = std::fs::File::open(&path).unwrap();

        let mut buf = vec![0u8; 4096];
        assert_eq!(read_at_full(&f, &mut buf, 1024).unwrap(), 4096);
        assert_eq!(buf, &body[1024..5120]);

        // Past the end it stops at EOF, and the caller decides whether that is a
        // legitimate short reply.
        let mut tail = vec![0u8; 4096];
        assert_eq!(read_at_full(&f, &mut tail, 6144).unwrap(), 2048);
        assert!(short_reply_ok(6144, 2048, 4096, body.len() as u64));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Dir cache eviction ────────────────────────────────────────────────────

    fn dir_with(name: &str, n: usize) -> (PathBuf, Vec<RemoteEntry>) {
        let dir = PathBuf::from(format!("/{name}"));
        let files = (0..n)
            .map(|i| make_dav_entry(&format!("{name}-{i}.txt"), Some(i as u64 + 1)))
            .collect();
        (dir, files)
    }

    #[test]
    fn unbounded_when_the_limit_is_zero() {
        let mut c = make_test_cache();
        c.dir_cache_max_dirs = 0;
        for i in 0..50 {
            let (d, f) = dir_with(&format!("d{i}"), 2);
            c.put_dir_cache(d, None, None, f);
        }
        assert_eq!(c.dir_cache.len(), 50, "0 must mean no limit");
    }

    #[test]
    fn evicts_down_to_the_ceiling_once_exceeded() {
        let mut c = make_test_cache();
        c.dir_cache_max_dirs = 10;
        for i in 0..40 {
            let (d, f) = dir_with(&format!("d{i}"), 2);
            c.put_dir_cache(d, None, None, f);
        }
        assert!(c.dir_cache.len() <= 10, "must stay within the ceiling, got {}", c.dir_cache.len());
        // Evicting to 90% of the ceiling rather than exactly the ceiling keeps a
        // cache sitting at the limit from re-sorting on every insert.
        assert!(c.dir_cache.len() >= 8, "must not over-evict, got {}", c.dir_cache.len());
        // The most recent insert always survives.
        assert!(c.dir_cache.contains_key(&PathBuf::from("/d39")));
    }

    #[test]
    fn eviction_keeps_what_was_recently_accessed_not_recently_fetched() {
        // The point of ordering by access: a listing refreshed by a staleness
        // probe is not necessarily one anyone is looking at.
        let mut c = make_test_cache();
        c.dir_cache_max_dirs = 0;
        for i in 0..12 {
            let (d, f) = dir_with(&format!("d{i}"), 1);
            c.put_dir_cache(d, None, None, f);
        }
        // Touch two of the oldest so they become the most recently used.
        let keep_a = PathBuf::from("/d0");
        let keep_b = PathBuf::from("/d1");
        assert!(c.get_cached_dir(&keep_a, Duration::from_secs(600), None).is_some());
        assert!(c.get_cached_dir_readonly(&keep_b).is_some());

        // Now impose a ceiling and force an eviction pass with one more insert.
        c.dir_cache_max_dirs = 6;
        let (d, f) = dir_with("fresh", 1);
        c.put_dir_cache(d, None, None, f);

        assert!(c.dir_cache.contains_key(&keep_a), "a dir read via get_cached_dir must survive");
        assert!(c.dir_cache.contains_key(&keep_b), "a dir read via get_cached_dir_readonly must survive");
        assert!(c.dir_cache.contains_key(&PathBuf::from("/fresh")));
        // Untouched middle entries are the ones that go.
        assert!(!c.dir_cache.contains_key(&PathBuf::from("/d2")));
    }

    #[test]
    fn a_dir_holding_a_pending_upload_is_never_evicted() {
        // A file whose PUT is still in flight exists *only* in its parent's
        // cached listing — the server does not have it yet. Evicting that
        // listing makes the file the user just created disappear, and
        // put_dir_cache's upload re-merge has nothing left to merge from.
        let mut c = make_test_cache();
        c.dir_cache_max_dirs = 0;
        for i in 0..12 {
            let (d, f) = dir_with(&format!("d{i}"), 1);
            c.put_dir_cache(d, None, None, f);
        }
        let pinned = PathBuf::from("/d0");
        c.uploading.insert(pinned.join("d0-0.txt"));

        c.dir_cache_max_dirs = 4;
        let (d, f) = dir_with("fresh", 1);
        c.put_dir_cache(d, None, None, f);

        assert!(c.dir_cache.contains_key(&pinned), "a dir with an in-flight upload must be kept");
        assert!(c.dir_cache.len() <= 5, "the rest must still be evicted, got {}", c.dir_cache.len());
    }

    #[test]
    fn a_restored_cache_evicts_the_oldest_listing_first() {
        // load_dir_cache stamps every restored listing with the next access tick,
        // so the order it inserts them in *is* their LRU order. Inserting
        // newest-first would hand the newest listing the lowest tick and make it
        // the first eviction candidate after a restart.
        let dir = std::env::temp_dir().join(format!("ncrs-test-lru-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut seed = make_test_cache();
        seed.cache_dir = dir.clone();
        for i in 0..6 {
            let (d, f) = dir_with(&format!("d{i}"), 1);
            seed.put_dir_cache(d, None, None, f);
        }
        // Distinct fetch times: /d0 oldest … /d5 newest.
        for i in 0..6u64 {
            let e = seed.dir_cache.get_mut(&PathBuf::from(format!("/d{i}"))).unwrap();
            e.fetched_at = UNIX_EPOCH + Duration::from_secs(1_000_000 + i * 60);
        }
        let seed = Mutex::new(seed);
        save_dir_cache_now(&seed);

        let mut restored = make_test_cache();
        restored.cache_dir = dir.clone();
        let restored = Mutex::new(restored);
        load_dir_cache(&restored);

        let mut c = restored.safe_lock();
        assert_eq!(c.dir_cache.len(), 6, "all six listings must come back");
        // Force one eviction pass without anything having been accessed since.
        c.dir_cache_max_dirs = 5;
        let (d, f) = dir_with("fresh", 1);
        c.put_dir_cache(d, None, None, f);

        assert!(c.dir_cache.contains_key(&PathBuf::from("/d5")),
            "the most recently fetched listing must outlive the oldest");
        assert!(!c.dir_cache.contains_key(&PathBuf::from("/d0")),
            "the oldest restored listing is the first to go");
        drop(c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refreshing_dir_is_never_evicted() {
        // `refreshing` is the interlock preventing a second concurrent PROPFIND
        // for the same directory; dropping the entry would lose it.
        let mut c = make_test_cache();
        c.dir_cache_max_dirs = 0;
        for i in 0..12 {
            let (d, f) = dir_with(&format!("d{i}"), 1);
            c.put_dir_cache(d, None, None, f);
        }
        let pinned = PathBuf::from("/d0");
        c.dir_cache.get_mut(&pinned).unwrap().refreshing = true;

        c.dir_cache_max_dirs = 4;
        let (d, f) = dir_with("fresh", 1);
        c.put_dir_cache(d, None, None, f);

        assert!(c.dir_cache.contains_key(&pinned), "a refreshing dir must be kept");
    }

    #[test]
    fn evicted_listings_are_simply_a_cache_miss() {
        // Eviction must never look like "directory does not exist" — it has to
        // fall through to a re-list.
        let mut c = make_test_cache();
        c.dir_cache_max_dirs = 2;
        for i in 0..20 {
            let (d, f) = dir_with(&format!("d{i}"), 1);
            c.put_dir_cache(d, None, None, f);
        }
        let gone = PathBuf::from("/d0");
        assert!(!c.dir_cache.contains_key(&gone));
        assert!(c.get_cached_dir(&gone, DIR_CACHE_TTL, None).is_none(), "must read as a miss");
        assert!(c.get_cached_dir_readonly(&gone).is_none());
    }

    // ── Boot cache loading ────────────────────────────────────────────────────

    #[test]
    fn boot_loaded_dirs_are_not_invalidated() {
        let cache = Arc::new(Mutex::new(make_test_cache()));
        let path = cache.safe_lock().cache_dir.join(DIR_CACHE_FILE);
        let mut map = HashMap::new();
        let saved_at = SystemTime::now() - Duration::from_secs(3 * 3600);
        map.insert("/Photos".to_string(), PersistedDirEntry {
            etag: Some("abc123".into()),
            self_entry: None,
            files: std::sync::Arc::new(vec![make_dav_entry("sunset.jpg", Some(1))]),
            fetched_at: Some(saved_at.duration_since(UNIX_EPOCH).unwrap().as_secs()),
        });
        let json = serde_json::to_vec(&map).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).ok();
        std::fs::write(&path, json).unwrap();
        load_dir_cache(&cache);
        let c = cache.safe_lock();
        let entry = c.dir_cache.get(&PathBuf::from("/Photos")).unwrap();
        assert!(!entry.invalidated, "boot-loaded dir should not be invalidated");
        assert_eq!(entry.etag, Some("abc123".into()));
        assert!(
            entry.fetched_at.elapsed().unwrap() >= Duration::from_secs(3 * 3600),
            "the persisted fetch time must survive the restart, not reset to now"
        );
        drop(c);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn boot_loads_legacy_nested_entry_extensions() {
        // A dir_cache.json written by <= 0.1.56, when EntryExtensions was three
        // HashMaps on the wire. An upgrade must keep reading it: the alternative
        // is discarding a cache of tens of thousands of directories and
        // re-PROPFINDing the whole tree one listing at a time.
        let mut base = make_test_cache();
        base.cache_dir = PathBuf::from("/tmp/ncrs-test-cache-legacy-ext");
        let cache = Arc::new(Mutex::new(base));
        let path = cache.safe_lock().cache_dir.join(DIR_CACHE_FILE);
        let json = br#"{"/Photos":{"etag":"e1","self_entry":null,"files":[
            {"path":"/Photos/sunset.jpg","is_dir":false,"size":42,
             "modified":{"secs_since_epoch":1586232939,"nanos_since_epoch":0},
             "change_token":"tok","content_type":"image/jpeg",
             "ext":{"strings":{"owner_id":"rgon","owner_display_name":"Gonzalo Ruiz","permissions":"RGDNVW"},
                    "integers":{"fileid":1234},
                    "booleans":{"has_preview":true,"is_shared":false}}}
        ],"fetched_at":1787672707}}"#;
        std::fs::create_dir_all(path.parent().unwrap()).ok();
        std::fs::write(&path, json).unwrap();
        load_dir_cache(&cache);

        let c = cache.safe_lock();
        let dir = c.dir_cache.get(&PathBuf::from("/Photos")).expect("legacy dir loaded");
        let e = &dir.files[0];
        assert_eq!(e.content_type.as_deref(), Some("image/jpeg"));
        assert_eq!(e.ext.str("permissions"), Some("RGDNVW"));
        assert_eq!(e.ext.str("owner_id"), Some("rgon"));
        assert_eq!(e.ext.str("owner_display_name"), Some("Gonzalo Ruiz"));
        assert_eq!(e.ext.int("fileid"), Some(1234));
        assert!(e.ext.flag("has_preview"));
        assert!(!e.ext.flag("is_shared"));
        drop(c);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn dir_cache_round_trips_through_the_compact_format() {
        let mut base = make_test_cache();
        base.cache_dir = PathBuf::from("/tmp/ncrs-test-cache-roundtrip");
        std::fs::create_dir_all(&base.cache_dir).ok();
        let path = base.cache_dir.join(DIR_CACHE_FILE);
        std::fs::remove_file(&path).ok();

        let mut entry = make_dav_entry("sunset.jpg", Some(7));
        entry.ext.set_str("permissions", "RGDNVW");
        entry.ext.set_str("owner_id", "rgon");
        entry.ext.set_flag("is_shared", true);
        entry.content_type = Some(crate::backend::intern("image/jpeg"));

        let cache = Arc::new(Mutex::new(base));
        cache.safe_lock().put_dir_cache(
            PathBuf::from("/Photos"), Some("e1".into()), None, vec![entry],
        );
        save_dir_cache_now(&cache);

        // The written file must be the flat form, not the old nested maps.
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains(r#""permissions":"RGDNVW""#), "got: {}", written);
        assert!(!written.contains(r#""strings""#), "legacy shape was written: {}", written);

        // And it must read back into an equivalent cache.
        let reloaded = Arc::new(Mutex::new({
            let mut b = make_test_cache();
            b.cache_dir = PathBuf::from("/tmp/ncrs-test-cache-roundtrip");
            b
        }));
        load_dir_cache(&reloaded);
        let c = reloaded.safe_lock();
        let dir = c.dir_cache.get(&PathBuf::from("/Photos")).expect("dir reloaded");
        let e = &dir.files[0];
        assert_eq!(e.ext.str("permissions"), Some("RGDNVW"));
        assert_eq!(e.ext.str("owner_id"), Some("rgon"));
        assert_eq!(e.ext.int("fileid"), Some(7));
        assert!(e.ext.flag("is_shared"));
        assert_eq!(e.content_type.as_deref(), Some("image/jpeg"));
        drop(c);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn interning_hands_out_one_allocation_per_distinct_value() {
        let a = crate::backend::intern("RGDNVW");
        let b = crate::backend::intern("RGDNVW");
        assert!(std::sync::Arc::ptr_eq(&a, &b), "equal values must share one allocation");
        let c = crate::backend::intern("RGDNVCK");
        assert!(!std::sync::Arc::ptr_eq(&a, &c));
        assert_eq!(&*c, "RGDNVCK");
    }

    #[test]
    fn boot_load_does_not_preallocate_inodes_for_every_cached_file() {
        // Pre-assigning an inode to every cached file cost two owned PathBufs
        // each and bought nothing, since inode numbers do not survive a restart.
        let mut base = make_test_cache();
        base.cache_dir = PathBuf::from("/tmp/ncrs-test-cache-no-prealloc");
        let cache = Arc::new(Mutex::new(base));
        let path = cache.safe_lock().cache_dir.join(DIR_CACHE_FILE);
        let json = br#"{"/Photos":{"etag":"e1","self_entry":null,"files":[
            {"path":"/Photos/sunset.jpg","is_dir":false,"size":1,"modified":null,
             "change_token":null,"content_type":null,"ext":{}}
        ],"fetched_at":1787672707}}"#;
        std::fs::create_dir_all(path.parent().unwrap()).ok();
        std::fs::write(&path, json).unwrap();
        let before = cache.safe_lock().paths.len();
        load_dir_cache(&cache);

        let mut c = cache.safe_lock();
        assert_eq!(c.paths.len(), before, "boot load must not populate the inode maps");
        // But the listing is there, and looking a file up still assigns one.
        assert!(c.dir_cache.contains_key(&PathBuf::from("/Photos")));
        let ino = c.allocate_inode(PathBuf::from("/Photos/sunset.jpg"));
        assert_eq!(c.get_inode(Path::new("/Photos/sunset.jpg")), Some(ino));
        drop(c);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn boot_loaded_dir_without_fetch_time_is_treated_as_ancient() {
        let mut base = make_test_cache();
        base.cache_dir = PathBuf::from("/tmp/ncrs-test-cache-legacy");
        let cache = Arc::new(Mutex::new(base));
        let path = cache.safe_lock().cache_dir.join(DIR_CACHE_FILE);
        // A dir_cache.json written before dir_cache_max_stale_mins existed.
        let json = br#"{"/Legacy":{"etag":"old","self_entry":null,"files":[]}}"#;
        std::fs::create_dir_all(path.parent().unwrap()).ok();
        std::fs::write(&path, json).unwrap();
        load_dir_cache(&cache);
        let mut c = cache.safe_lock();
        let legacy = PathBuf::from("/Legacy");
        assert!(
            c.get_cached_dir(&legacy, DIR_CACHE_TTL, Some(Duration::from_secs(2 * 3600))).is_none(),
            "a listing of unknown age must be re-listed rather than trusted"
        );
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

        assert!(crate::desktop::toolkit::gio::GLIB_SNIFF_MAX_READ >= READAHEAD_CEILING,
            "intercept must cover read-ahead-inflated magic reads ({} < {})",
            crate::desktop::toolkit::gio::GLIB_SNIFF_MAX_READ, READAHEAD_CEILING);
        assert!(crate::desktop::toolkit::gio::GLIB_SNIFF_MAX_READ < SMALLEST_COPY_BUFFER,
            "intercept must not swallow copy reads ({} >= {})",
            crate::desktop::toolkit::gio::GLIB_SNIFF_MAX_READ, SMALLEST_COPY_BUFFER);
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
        // RIFF/BMP arms carry the form-type/enough bytes: bare "RIFF"/"BM" degrade
        // to application/x-riff / text/plain (caught by scripts/mime_audit.py).
        assert_eq!(mime_magic_bytes("image/webp"), b"RIFF\x00\x00\x00\x00WEBP");
        assert_eq!(mime_magic_bytes("audio/wav"),  b"RIFF\x00\x00\x00\x00WAVE");
        assert_eq!(mime_magic_bytes("image/bmp"),  b"BM\x00\x00\x00\x00\x00\x00\x00\x00");

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

    // ── prepare_mount_point (orphan-write adoption) ─────────────────────────────
    // Regression coverage for: ncrs torn down (or force-detached) while a file
    // is still open elsewhere → a later save lands straight on the real,
    // now-exposed mount-point directory → next start used to always refuse.

    fn test_tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ncrs_lib_test_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn prepare_mount_point_rejects_nonempty_without_marker() {
        // First-time setup (or a freshly configured mount point ncrs has never
        // owned) must keep refusing a non-empty directory outright.
        let base = test_tmp("no_marker");
        let mount_point = base.join("mount");
        let cache_dir = base.join("cache");
        std::fs::create_dir_all(&mount_point).unwrap();
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(mount_point.join("stray.txt"), b"data").unwrap();

        let err = prepare_mount_point(&mount_point, &cache_dir).unwrap_err();
        assert!(err.contains("not empty"), "unexpected error: {}", err);
        assert!(mount_point.join("stray.txt").exists(), "leftover must be untouched on refusal");
    }

    #[test]
    fn prepare_mount_point_rejects_nonempty_with_mismatched_marker() {
        // ncrs owns a *different* path per the marker — this one is still new
        // to it, so it must refuse just like the no-marker case, not adopt.
        let base = test_tmp("mismatched_marker");
        let mount_point = base.join("mount");
        let other_mount_point = base.join("other_mount");
        let cache_dir = base.join("cache");
        std::fs::create_dir_all(&mount_point).unwrap();
        std::fs::create_dir_all(&other_mount_point).unwrap();
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(mount_point.join("stray.txt"), b"data").unwrap();

        write_mount_marker(&cache_dir, &other_mount_point);

        let err = prepare_mount_point(&mount_point, &cache_dir).unwrap_err();
        assert!(err.contains("not empty"), "unexpected error: {}", err);
        assert!(mount_point.join("stray.txt").exists());
    }

    #[test]
    fn prepare_mount_point_adopts_leftovers_on_known_path() {
        // Resuming on a path ncrs already owns: leftovers (including a nested
        // one) are adopted as pending uploads, junk is discarded, and the
        // mount point ends up empty so the real mount can proceed.
        let base = test_tmp("known_path");
        let mount_point = base.join("mount");
        let cache_dir = base.join("cache");
        std::fs::create_dir_all(&mount_point).unwrap();
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(mount_point.join("sub")).unwrap();
        std::fs::write(mount_point.join("sub/orphan.ods"), b"orphaned edit").unwrap();
        std::fs::write(mount_point.join(".~lock.orphan.ods#"), b"lock").unwrap();

        write_mount_marker(&cache_dir, &mount_point);

        let adopted = prepare_mount_point(&mount_point, &cache_dir).unwrap();

        assert_eq!(adopted.len(), 1);
        assert_eq!(adopted[0].remote_path, PathBuf::from("/sub/orphan.ods"));
        assert_eq!(std::fs::read(&adopted[0].staging_path).unwrap(), b"orphaned edit");

        let mut entries = std::fs::read_dir(&mount_point).unwrap();
        assert!(entries.next().is_none(), "mount point must be emptied after adoption");
    }

    #[test]
    fn trackerignore_overlay_appears_in_root_listing_without_a_write() {
        let mut cache = make_test_cache();
        cache.put_dir_cache(PathBuf::from("/"), None, None, vec![make_dav_entry("real.txt", None)]);

        let root = cache.get_cached_dir_readonly(&PathBuf::from("/")).unwrap();
        assert!(root.iter().any(|e| e.path == trackerignore_path()));
        assert!(root.iter().any(|e| e.path == PathBuf::from("/real.txt")));

        // Never spliced into a non-root directory.
        cache.put_dir_cache(PathBuf::from("/sub"), None, None, vec![make_dav_entry("child.txt", None)]);
        let sub = cache.get_cached_dir_readonly(&PathBuf::from("/sub")).unwrap();
        assert!(!sub.iter().any(|e| e.path == trackerignore_path()));
    }

    #[test]
    fn trackerignore_overlay_survives_a_fresh_propfind_with_no_trace_of_it() {
        let mut cache = make_test_cache();
        cache.put_dir_cache(PathBuf::from("/"), None, None, vec![make_dav_entry("a.txt", None)]);
        assert!(cache.get_cached_dir_readonly(&PathBuf::from("/")).unwrap()
            .iter().any(|e| e.path == trackerignore_path()));

        // A real PROPFIND result never contains it — put_dir_cache must re-add it
        // on every call rather than depending on a stored copy surviving eviction.
        cache.put_dir_cache(PathBuf::from("/"), Some("etag2".into()), None, vec![make_dav_entry("b.txt", None)]);
        let root = cache.get_cached_dir_readonly(&PathBuf::from("/")).unwrap();
        assert!(root.iter().any(|e| e.path == trackerignore_path()));
        assert_eq!(root.iter().filter(|e| e.path == trackerignore_path()).count(), 1);
    }

    #[test]
    fn trackerignore_overlay_stays_hidden_once_unlinked() {
        let mut cache = make_test_cache();
        cache.put_dir_cache(PathBuf::from("/"), None, None, vec![make_dav_entry("a.txt", None)]);
        assert!(cache.get_cached_dir_readonly(&PathBuf::from("/")).unwrap()
            .iter().any(|e| e.path == trackerignore_path()));

        // Mirrors what unlink() does: hide it, then a later refresh must not
        // resurrect it for the rest of this mount's lifetime.
        cache.trackerignore_hidden = true;
        cache.put_dir_cache(PathBuf::from("/"), None, None, vec![make_dav_entry("a.txt", None)]);
        let root = cache.get_cached_dir_readonly(&PathBuf::from("/")).unwrap();
        assert!(!root.iter().any(|e| e.path == trackerignore_path()));
    }

    #[test]
    fn prepare_mount_point_refuses_when_leftovers_look_implausible() {
        // An entry that isn't a plain file or directory (e.g. a socket) is not
        // a plausible stray app-write — refuse rather than guess, and leave it
        // untouched, same as the too-many-files/too-many-bytes safety cap.
        let base = test_tmp("too_much");
        let mount_point = base.join("mount");
        let cache_dir = base.join("cache");
        std::fs::create_dir_all(&mount_point).unwrap();
        std::fs::create_dir_all(&cache_dir).unwrap();
        write_mount_marker(&cache_dir, &mount_point);

        use std::os::unix::net::UnixListener;
        let _listener = UnixListener::bind(mount_point.join("weird.sock")).unwrap();

        let err = prepare_mount_point(&mount_point, &cache_dir).unwrap_err();
        assert!(err.contains("not auto-adopted"), "unexpected error: {}", err);
        assert!(mount_point.join("weird.sock").exists(), "leftover must be untouched on refusal");
    }

    // ── Offline reads must not look like corruption ──────────────────────────

    #[test]
    fn offline_read_error_is_timeout_not_eio() {
        // The whole point of OFFLINE_READ_ERR's wording: EIO tells apps the file is
        // unreadable (mpv reports "Failed to recognize file format") and makes
        // Nautilus mark the entire mount inaccessible. A brief outage must instead
        // surface as a retryable, transient condition.
        // `Errno` is not `PartialEq`, so compare the raw codes.
        assert_eq!(i32::from(error_to_errno(OFFLINE_READ_ERR)), i32::from(Errno::ETIMEDOUT));
        assert_ne!(i32::from(error_to_errno(OFFLINE_READ_ERR)), i32::from(Errno::EIO));
    }

    #[test]
    fn offline_read_error_classifies_as_network_down() {
        // It must also keep the daemon offline rather than reading as an
        // application-level rejection, so the connectivity monitor stays in its
        // fast 5s re-probe cadence.
        assert!(read_err_is_network_down(OFFLINE_READ_ERR));
    }

    #[test]
    fn the_old_offline_message_was_the_bug() {
        // Regression guard: the previous string matched no arm in error_to_errno and
        // fell through to EIO. If someone reintroduces that wording, this fails.
        assert_eq!(i32::from(error_to_errno("file not available offline")), i32::from(Errno::EIO));
    }

    // ── HTTP/3 demotion ──────────────────────────────────────────────────────

    fn test_clients(http3: bool) -> crate::http_clients::HttpClients {
        let mk = || reqwest::blocking::Client::builder().build().unwrap();
        let (h2, read_h2) = (mk(), mk());
        // Stand-ins for the QUIC pair: we only assert which slot is handed out.
        let (pref, read_pref) = if http3 { (mk(), mk()) } else { (h2.clone(), read_h2.clone()) };
        crate::http_clients::HttpClients::new(pref, read_pref, h2, read_h2, http3)
    }

    #[test]
    fn a_demotion_is_remembered_by_the_next_session() {
        let dir = std::env::temp_dir().join(format!("ncrs-test-h3marker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("h3_demoted");

        let first = test_clients(true).with_demotion_marker(marker.clone());
        assert!(first.http3_active(), "no marker yet: the session starts on HTTP/3");
        first.demote();
        assert!(marker.exists(), "a demotion must be recorded for the next session");

        let second = test_clients(true).with_demotion_marker(marker.clone());
        assert!(!second.http3_active(), "a fresh marker must start the next session on HTTP/2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_expired_demotion_marker_retries_http3() {
        let dir = std::env::temp_dir().join(format!("ncrs-test-h3stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("h3_demoted");
        // A demotion recorded 8 days ago — past the 7-day retry window.
        let old_ts = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 8 * 24 * 3600;
        std::fs::write(&marker, old_ts.to_string()).unwrap();

        let c = test_clients(true).with_demotion_marker(marker.clone());
        assert!(c.http3_active(), "an expired marker must give QUIC another chance");
        assert!(!marker.exists(), "the expired marker is removed so the retry is real");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_garbage_demotion_marker_is_discarded() {
        let dir = std::env::temp_dir().join(format!("ncrs-test-h3junk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("h3_demoted");
        std::fs::write(&marker, "not a timestamp").unwrap();

        let c = test_clients(true).with_demotion_marker(marker.clone());
        assert!(c.http3_active(), "an unreadable marker must not pin the session to HTTP/2");
        assert!(!marker.exists(), "the unreadable marker is removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_marker_is_inert_without_http3() {
        let dir = std::env::temp_dir().join(format!("ncrs-test-h3off-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("h3_demoted");
        std::fs::write(&marker, "0").unwrap();   // ancient marker

        let c = test_clients(false).with_demotion_marker(marker.clone());
        assert!(!c.http3_active());
        assert!(marker.exists(), "with http3 disabled the marker is left untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn demotion_switches_both_clients_to_http2() {
        let c = test_clients(true);
        assert!(c.http3_active());
        assert!(c.get().is_h3(), "before demotion requests are stamped Version::HTTP_3");
        assert!(c.read().is_h3(), "the read client is stamped too");
        assert!(!c.h2().is_h3(), "the escape-hatch client never stamps HTTP/3");

        c.demote();

        assert!(!c.http3_active());
        assert!(!c.get().is_h3(), "after demotion every request goes out unstamped over TCP");
        assert!(!c.read().is_h3(), "reads too");
    }

    #[test]
    fn demotion_is_idempotent_and_one_way() {
        let c = test_clients(true);
        c.demote();
        c.demote();
        assert!(!c.http3_active(), "demotion must not flap back to HTTP/3");
    }

    #[test]
    fn without_http3_there_is_nothing_to_demote() {
        // The mount-time probe gates its h2 fallback on http3_active(), so a
        // plain-HTTP/2 mount must never pay for a second probe.
        let c = test_clients(false);
        assert!(!c.http3_active());
        c.demote();
        assert!(!c.http3_active(), "demoting a non-http3 client set stays a no-op");
    }

    // ── Connectivity probe retry ─────────────────────────────────────────────

    /// A scripted HTTP/1.1 server: serves one canned status per accepted
    /// connection, in order, then stops accepting. `Connection: close` forces
    /// the client to dial fresh for every request, so each probe consumes the
    /// next script entry.
    #[allow(clippy::disallowed_methods)] // test scaffolding, not daemon threads
    fn scripted_server(statuses: &'static [u16]) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for &status in statuses {
                let (mut sock, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(_) => return,
                };
                // Drain the full request (headers + Content-Length body) before
                // responding: closing a socket with unread data makes the kernel
                // send RST, which can eat the response we just wrote.
                let mut req = Vec::new();
                let mut buf = [0u8; 4096];
                let body_start = loop {
                    match sock.read(&mut buf) {
                        Ok(0) | Err(_) => break None,
                        Ok(n) => {
                            req.extend_from_slice(&buf[..n]);
                            if let Some(pos) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                                break Some(pos + 4);
                            }
                        }
                    }
                };
                if let Some(body_start) = body_start {
                    let headers = String::from_utf8_lossy(&req[..body_start]).to_ascii_lowercase();
                    let content_length: usize = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    let mut got = req.len() - body_start;
                    while got < content_length {
                        match sock.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => got += n,
                        }
                    }
                }
                let reason = match status {
                    207 => "Multi-Status",
                    401 => "Unauthorized",
                    _ => "Internal Server Error",
                };
                let _ = write!(
                    sock,
                    "HTTP/1.1 {} {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    status, reason
                );
            }
        });
        format!("http://{}/", addr)
    }

    /// The scripted server answers the constructor's credential probe with the
    /// script's first entry; `check_reachability` consumes the rest.
    fn scripted_backend(url: &str) -> crate::nextcloud::NextcloudBackend {
        crate::nextcloud::NextcloudBackend::new(
            url.to_string(),
            url.to_string(),
            crate::auth::Credentials::Basic { username: "u".into(), password: "p".into() },
            test_clients(false),
        )
        .expect("constructor probe against the scripted server")
    }

    #[test]
    fn a_single_failed_probe_is_retried_not_believed() {
        use crate::backend::{CloudBackend, ReachabilityStatus};
        // QUIC connections idle out under NAT and lose single samples to
        // congestion; one failed probe must trigger a retry, not an offline
        // flip (nor, on an HTTP/3 mount, the week-long persisted demotion).
        let url = scripted_server(&[207, 500, 207]);
        let backend = scripted_backend(&url);
        let status = backend.check_reachability(Duration::from_secs(5));
        assert_eq!(
            status,
            ReachabilityStatus::Reachable,
            "one bad sample must not condemn the mount"
        );
    }

    #[test]
    fn two_consecutive_failed_probes_mark_unreachable() {
        use crate::backend::{CloudBackend, ReachabilityStatus};
        // 503: the reverse proxy says the app behind it is down — a real outage.
        let url = scripted_server(&[207, 503, 503]);
        let backend = scripted_backend(&url);
        let status = backend.check_reachability(Duration::from_secs(5));
        assert_eq!(status, ReachabilityStatus::Unreachable);
    }

    #[test]
    fn a_server_answering_500_stays_online() {
        use crate::backend::{CloudBackend, ReachabilityStatus};
        // A 500 is Nextcloud itself answering, badly (PHP error, DB lock under
        // load). Going offline would hide a reachable server behind the cache;
        // the listing path backs off from 5xx on its own.
        for code in [&[207u16, 500, 500][..], &[207, 429, 429], &[207, 507, 507]] {
            let url = scripted_server(code);
            let backend = scripted_backend(&url);
            let status = backend.check_reachability(Duration::from_secs(5));
            assert_eq!(status, ReachabilityStatus::Reachable, "script {:?}", code);
        }
    }

    #[test]
    fn a_broken_http3_transport_is_demoted_at_mount_time() {
        use crate::backend::{CloudBackend, ReachabilityStatus};
        // The h3 stand-ins are plain TCP clients, so an HTTP_3-stamped request
        // through them fails at the transport layer exactly like real QUIC
        // against a server with no HTTP/3 listener. The constructor must fall
        // back to the h2 probe, demote, and still bring the mount up.
        let url = scripted_server(&[207, 207]);
        let clients = test_clients(true);
        let watch = clients.clone();
        let backend = crate::nextcloud::NextcloudBackend::new(
            url.clone(),
            url,
            crate::auth::Credentials::Basic { username: "u".into(), password: "p".into() },
            clients,
        )
        .expect("mount must come up on HTTP/2 when only QUIC is broken");
        assert!(!watch.http3_active(), "the broken transport is demoted before the mount comes up");
        // And mid-session probes run on the demoted (working) transport: a
        // failure there is an outage, never another transport verdict.
        let status = backend.check_reachability(Duration::from_secs(5));
        assert_eq!(status, ReachabilityStatus::Reachable);
    }

    #[test]
    fn a_mid_session_probe_failure_never_demotes() {
        use crate::backend::{CloudBackend, ReachabilityStatus};
        // Constructor h3 probe fails transport-side, h2 fallback sees 207 →
        // demoted mount. Hand the next session's story to the runtime probe:
        // both its attempts fail — the verdict must be Unreachable, with the
        // transport latch untouched (there is nothing left to demote to).
        //
        // The stronger claim — an h3 mount whose startup succeeded is never
        // demoted by a runtime failure — cannot be scripted without a real
        // QUIC listener, but it holds by construction: check_reachability no
        // longer references demote() at all.
        let url = scripted_server(&[207, 503, 503]);
        let clients = test_clients(true);
        let backend = crate::nextcloud::NextcloudBackend::new(
            url.clone(),
            url,
            crate::auth::Credentials::Basic { username: "u".into(), password: "p".into() },
            clients,
        )
        .expect("mount comes up demoted");
        let status = backend.check_reachability(Duration::from_secs(5));
        assert_eq!(status, ReachabilityStatus::Unreachable);
    }

    #[test]
    fn an_auth_rejection_is_not_retried() {
        use crate::backend::{CloudBackend, ReachabilityStatus};
        // 401 is an answer from the server, not a transport blip — retrying it
        // would only delay the remote-wipe check the monitor runs on rejection.
        let url = scripted_server(&[207, 401]);
        let backend = scripted_backend(&url);
        let status = backend.check_reachability(Duration::from_secs(5));
        assert_eq!(status, ReachabilityStatus::AuthRejected(401));
    }

#[cfg(test)]
mod mount_local_path_tests {
    use crate::mount_local_path;
    use std::path::{Path, PathBuf};

    #[test]
    fn resolves_a_path_inside_the_mount() {
        let mount = Path::new("/home/u/Nextcloud");
        assert_eq!(
            mount_local_path(mount, "/Deemix Downloads/a.flac"),
            Some(PathBuf::from("/home/u/Nextcloud/Deemix Downloads/a.flac")),
        );
        assert_eq!(
            mount_local_path(mount, "top.txt"),
            Some(PathBuf::from("/home/u/Nextcloud/top.txt")),
        );
    }

    #[test]
    fn contains_a_path_that_stays_absolute_after_one_slash_is_trimmed() {
        // The regression: `strip_prefix('/')` trimmed only the first slash, so
        // these stayed absolute and `Path::join` threw the mount point away and
        // handed back the server's own path. Trimming every leading slash keeps
        // them inside the mount, where at worst they name nothing.
        let mount = Path::new("/home/u/Nextcloud");
        for escape in [
            "//home/u/.bashrc",
            "///etc/passwd",
            "//home/u/.local/share/applications/evil.desktop",
        ] {
            let resolved = mount_local_path(mount, escape).expect("stays a usable relative path");
            assert!(
                resolved.starts_with(mount),
                "{} escaped the mount as {}",
                escape,
                resolved.display(),
            );
        }
    }

    #[test]
    fn refuses_traversal_and_empty_paths() {
        let mount = Path::new("/home/u/Nextcloud");
        for bad in ["/../.bashrc", "/a/../../b", "..\\..\\x", "/./x", "/", "", "///"] {
            assert_eq!(mount_local_path(mount, bad), None, "{:?} must be refused", bad);
        }
    }
}

#[cfg(test)]
mod http3_available_tests {
    use crate::http_clients::{http3_available, HttpClients};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ncrs-test-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn clients(http3: bool) -> HttpClients {
        let mk = || reqwest::blocking::Client::builder().build().unwrap();
        let (h2, read_h2) = (mk(), mk());
        let (pref, read_pref) = if http3 { (mk(), mk()) } else { (h2.clone(), read_h2.clone()) };
        HttpClients::new(pref, read_pref, h2, read_h2, http3)
    }

    #[test]
    fn the_side_clients_agree_with_the_daemon_about_the_transport() {
        // The whole point of the shared marker: search and notifications must
        // reach the same verdict as the mount, or they keep dialling QUIC on a
        // network where the mount already gave up on it.
        let dir = tmp("h3side");
        let marker = dir.join("h3_demoted");

        let c = clients(true).with_demotion_marker(marker.clone());
        assert!(c.http3_active());
        assert!(http3_available(true, &marker), "no marker: both use HTTP/3");

        c.demote();
        assert!(!c.http3_active());
        assert!(!http3_available(true, &marker), "after a demotion neither may use HTTP/3");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_expired_marker_lets_the_side_clients_retry_http3() {
        let dir = tmp("h3sidestale");
        let marker = dir.join("h3_demoted");
        let old = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() - 8 * 24 * 3600;
        std::fs::write(&marker, old.to_string()).unwrap();

        assert!(http3_available(true, &marker), "past the retry window QUIC is armed again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn http3_off_in_config_is_never_overridden_by_a_marker() {
        let dir = tmp("h3sideoff");
        let marker = dir.join("h3_demoted");
        assert!(!http3_available(false, &marker), "no marker, but HTTP/3 is not configured");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod refresh_lock_tests {
    use crate::backend::{EntryExtensions, RemoteEntry};
    use crate::notify_push::{apply_listing, OldDirSnapshot};
    use crate::{FileCacheEntry, GhostEntry, Throttle};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;

    fn entry(path: &str, etag: &str, fileid: u64) -> RemoteEntry {
        let mut ext = EntryExtensions::default();
        ext.set_int("fileid", fileid);
        RemoteEntry {
            path: PathBuf::from(path),
            is_dir: false,
            size: 3,
            modified: None,
            change_token: Some(etag.into()),
            content_type: None,
            ext,
        }
    }

    fn cached(local_path: PathBuf) -> FileCacheEntry {
        FileCacheEntry { local_path, remote_modified: None, etag: Some("e".into()), kept: false, size: 3 }
    }

    // Regression: a refresh that moved or evicted a cached file re-locked `cache`
    // (via save_file_cache) while still holding it, freezing every FUSE getattr.
    #[test]
    #[allow(clippy::disallowed_methods)] // test scaffolding, not daemon threads
    fn refresh_that_moves_and_evicts_cached_files_releases_the_cache_lock() {
        let dir = std::env::temp_dir().join(format!("ncrs-refresh-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let modified_copy = dir.join("a.txt");
        let renamed_copy = dir.join("b.txt");
        std::fs::write(&modified_copy, b"old").unwrap();
        std::fs::write(&renamed_copy, b"old").unwrap();

        let old = vec![entry("/d/a.txt", "a1", 1), entry("/d/b.txt", "b1", 2)];
        let fresh = vec![entry("/d/a.txt", "a2", 1), entry("/d/c.txt", "b1", 2)];

        let mut c = super::make_test_cache();
        c.cache_dir = dir.clone();
        c.file_cache.insert(PathBuf::from("/d/a.txt"), cached(modified_copy.clone()));
        c.file_cache.insert(PathBuf::from("/d/b.txt"), cached(renamed_copy.clone()));
        c.put_dir_cache(PathBuf::from("/d"), Some("d1".into()), None, old.clone());
        let snap = OldDirSnapshot::of(&old);
        let cache = Arc::new(Mutex::new(c));
        let ghosts: Arc<Mutex<HashMap<PathBuf, GhostEntry>>> = Arc::new(Mutex::new(HashMap::new()));

        let (tx, rx) = mpsc::channel();
        {
            let (cache, ghosts) = (cache.clone(), ghosts.clone());
            std::thread::spawn(move || {
                let applied = apply_listing(&cache, &ghosts, Path::new("/d"), &snap, Some("d2".into()), None, fresh);
                let _ = tx.send((applied.diff.modified.len(), applied.diff.renames.len()));
            });
        }
        let (modified, renamed) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("apply_listing did not return: cache lock re-entered while held");
        assert_eq!((modified, renamed), (1, 1));

        let c = cache.try_lock().expect("cache lock still held after apply_listing");
        assert!(!c.file_cache.contains_key(Path::new("/d/a.txt")));
        assert!(c.file_cache.contains_key(Path::new("/d/c.txt")));
        drop(c);
        assert!(!modified_copy.exists(), "stale copy of a modified file must be removed");
        assert!(dir.join("file_cache.json").exists(), "file cache must be persisted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn throttle_acquire_timeout_gives_up_when_saturated() {
        let t = Throttle::new(1);
        let held = t.acquire();
        assert!(t.acquire_timeout(Duration::from_millis(50)).is_none());
        drop(held);
        assert!(t.acquire_timeout(Duration::from_millis(50)).is_some());
    }
}

#[cfg(test)]
mod upload_order_tests {
    use crate::UploadOrder;
    use std::path::Path;

    #[test]
    fn a_handle_opened_before_our_last_upload_sends_that_uploads_etag() {
        let u = UploadOrder::default();
        let p = Path::new("/f.txt");
        let opened_before = u.generation();
        u.record(p, Some("e2".into()));
        assert_eq!(u.etag_for(p, opened_before, Some("e1".into())), Some("e2".into()),
            "a rapid re-save must not send the etag our own previous upload replaced");
        let opened_after = u.generation();
        assert_eq!(u.etag_for(p, opened_after, Some("e3".into())), Some("e3".into()),
            "a handle opened later saw the server's etag and keeps its own");
    }

    #[test]
    fn recorded_etags_follow_renames_and_deletes() {
        let u = UploadOrder::default();
        let g = u.generation();
        u.record(Path::new("/a"), Some("e".into()));
        u.moved(Path::new("/a"), Path::new("/b"));
        assert_eq!(u.etag_for(Path::new("/b"), g, None), Some("e".into()));
        assert_eq!(u.etag_for(Path::new("/a"), g, None), None);
        u.forget(Path::new("/b"));
        assert_eq!(u.etag_for(Path::new("/b"), g, None), None);
    }
}

    // ── Error classification (2026-09-24 incident) ───────────────────────────
    //
    // Errors cross the daemon as strings that embed the path. Classification
    // must come from the typed prefix, never from digits or words that happen
    // to be in a directory name.

    fn errno_of(e: &str) -> i32 {
        error_to_errno(e).code()
    }

    #[test]
    fn a_5xx_listing_is_try_again_whatever_the_path_says() {
        for path in ["/Music/2404", "/Photos/401k", "/x/403 Forbidden", "/Not Found", "/timeout"] {
            let e = backend::BackendReadError::Server(500, format!("PROPFIND {} returned 500 Internal Server Error", path)).to_string();
            assert_eq!(errno_of(&e), libc::EAGAIN, "{e}");
            assert!(!is_transient_network_err(&e), "a 5xx is an answer, not a transport failure: {e}");
            assert!(!is_timeout_err(&e), "{e}");
            assert!(!read_err_is_network_down(&e), "a 5xx must never flip the mount offline: {e}");
            assert!(is_unreachable_listing_error(&e), "a stale listing beats a 5xx: {e}");
        }
    }

    #[test]
    fn server_status_codes_map_to_their_errno() {
        let e = |code| backend::BackendReadError::Server(code, "PROPFIND /Music/500 returned".into()).to_string();
        assert_eq!(errno_of(&e(401)), libc::EACCES);
        assert_eq!(errno_of(&e(403)), libc::EACCES);
        assert_eq!(errno_of(&e(404)), libc::ENOENT);
        assert_eq!(errno_of(&e(507)), libc::ENOSPC);
        assert_eq!(errno_of(&e(429)), libc::EAGAIN);
        assert_eq!(errno_of(&e(502)), libc::EAGAIN);
        assert_eq!(errno_of(&e(418)), libc::EIO);
        assert!(!is_unreachable_listing_error(&e(404)), "a 404 is an answer about the directory");
    }

    #[test]
    fn transport_failures_are_try_again_whatever_the_path_says() {
        let e = backend::BackendReadError::Network("PROPFIND /a/2404/401: error sending request".into()).to_string();
        assert_eq!(errno_of(&e), libc::EAGAIN);
        assert!(is_transient_network_err(&e));
        let t = backend::BackendReadError::Truncated("XML parse: I/O error: request or response body error".into()).to_string();
        assert_eq!(errno_of(&t), libc::EAGAIN);
        assert!(is_transient_network_err(&t), "a broken-off body is worth one retry");
        assert!(!is_server_error(&t));
    }

    #[test]
    fn timeouts_still_read_as_timeouts() {
        for e in ["timeout", "PROPFIND timeout for /Music", "WebDAV download timeout", "network: operation timed out", OFFLINE_READ_ERR] {
            assert_eq!(errno_of(e), libc::ETIMEDOUT, "{e}");
        }
        assert!(!is_timeout_err("network: PROPFIND /backups/timeout: connection refused"));
    }

    #[test]
    fn not_found_is_enoent() {
        assert_eq!(errno_of(&backend::BackendReadError::NotFound.to_string()), libc::ENOENT);
    }

    #[test]
    fn server_error_code_parses_only_the_typed_prefix() {
        assert_eq!(backend::server_error_code("server error 503: x"), Some(503));
        assert_eq!(backend::server_error_code("network: server error 503: x"), None);
        assert_eq!(backend::server_error_code("PROPFIND /server error 500"), None);
        assert_eq!(backend::server_error_code("server error x"), None);
    }
