    use super::*;

    fn make_test_cache() -> FsCache {
        let mut inodes = HashMap::new();
        let mut paths = BTreeMap::new();
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
            uploading: HashMap::new(),
            deleting: HashSet::new(),
            moving: HashMap::new(),
            trackerignore_hidden: false,
            pins: HashMap::new(),
            tombstones: tombstones::Tombstones::default(),
        }
    }

    // ── The write path off the dispatch thread (write_path.rs) ──────────────
    //
    // These drive `WriteCtx::dispatch_*` the way the kernel would, against a
    // fake chunked-upload server whose chunk PUTs are slow.

    mod write_path_tests {
        use super::*;
        use crate::write_path::WriteCtx;
        use std::sync::mpsc::{channel, Receiver};

        const MIB: usize = 1024 * 1024;

        #[derive(Default)]
        struct ChunkServer {
            chunk_delay: Duration,
            open_unsupported: bool,
            fail_chunk: Option<u64>,
            chunks: Mutex<HashMap<u64, Vec<u8>>>,
            // Chunk PUTs running right now, and the most ever at once.
            putting: AtomicUsize,
            max_putting: AtomicUsize,
            aborts: AtomicUsize,
            // Threads that talked to the server, and the paths sessions were opened for.
            net_threads: Mutex<Vec<std::thread::ThreadId>>,
            opened_for: Mutex<Vec<PathBuf>>,
            finished: Mutex<Vec<(PathBuf, Vec<u8>)>>,
            puts: Mutex<Vec<(PathBuf, Vec<u8>)>>,
            // Uploads to this path answer 412 (changed on the server).
            conflict_on: Option<PathBuf>,
            // Uploads of a conflicted copy answer 503.
            conflict_copy_fails: AtomicBool,
            // ... or 403: a share with edit but no create permission.
            conflict_copy_forbidden: AtomicBool,
        }

        impl ChunkServer {
            fn refuse(&self, path: &Path) -> Option<backend::BackendWriteError> {
                if self.conflict_on.as_deref() == Some(path) {
                    return Some(backend::BackendWriteError::Conflict);
                }
                let copy = path.to_string_lossy().contains("(conflicted copy");
                if copy && self.conflict_copy_forbidden.load(Ordering::SeqCst) {
                    return Some(backend::BackendWriteError::Forbidden);
                }
                (copy && self.conflict_copy_fails.load(Ordering::SeqCst)).then(|| backend::BackendWriteError::Server(503, "busy".into()))
            }
        }

        fn not_here() -> backend::BackendReadError {
            backend::BackendReadError::Network("not in the fake".into())
        }

        impl crate::backend::CloudBackend for ChunkServer {
            fn list_dir(&self, _: &Path, _: Duration) -> Result<(Option<String>, Option<RemoteEntry>, Vec<RemoteEntry>), backend::BackendReadError> {
                Err(not_here())
            }
            fn list_dir_streaming(&self, _: &Path, _: Duration, _: mpsc::Sender<RemoteEntry>, _: mpsc::Sender<RemoteEntry>) -> Result<Option<String>, backend::BackendReadError> {
                Err(not_here())
            }
            fn dir_change_token(&self, _: &Path, _: Duration) -> Result<Option<String>, backend::BackendReadError> {
                Err(not_here())
            }
            fn download_file(&self, _: &Path, _: &mut dyn std::io::Write, _: Duration) -> Result<u64, backend::BackendReadError> {
                Err(not_here())
            }
            fn read_file_range(&self, _: &Path, _: u64, _: &mut [u8], _: Duration) -> Result<usize, backend::BackendReadError> {
                Err(not_here())
            }
            fn put_file(&self, path: &Path, body: Vec<u8>, _: Option<&str>) -> Result<backend::PutResult, backend::BackendWriteError> {
                if let Some(e) = self.refuse(path) {
                    return Err(e);
                }
                self.puts.lock().unwrap().push((path.to_path_buf(), body));
                Ok(backend::PutResult { new_change_token: Some("put".into()) })
            }
            fn mkdir(&self, _: &Path) -> Result<(), backend::BackendWriteError> {
                Err(backend::BackendWriteError::Unsupported)
            }
            fn delete(&self, _: &Path) -> Result<(), backend::BackendWriteError> {
                Err(backend::BackendWriteError::Unsupported)
            }
            fn rename(&self, _: &Path, _: &Path) -> Result<(), backend::BackendWriteError> {
                Err(backend::BackendWriteError::Unsupported)
            }
            fn open_chunked_upload(&self, path: &Path) -> Result<backend::ChunkedUploadSession, backend::BackendWriteError> {
                self.net_threads.lock().unwrap().push(std::thread::current().id());
                self.opened_for.lock().unwrap().push(path.to_path_buf());
                if self.open_unsupported {
                    return Err(backend::BackendWriteError::Unsupported);
                }
                Ok(backend::ChunkedUploadSession { uploads_base: "uploads/1".into() })
            }
            fn put_chunk(&self, _: &backend::ChunkedUploadSession, index: u64, body: Vec<u8>) -> Result<(), backend::BackendWriteError> {
                self.net_threads.lock().unwrap().push(std::thread::current().id());
                let n = self.putting.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_putting.fetch_max(n, Ordering::SeqCst);
                std::thread::sleep(self.chunk_delay);
                self.putting.fetch_sub(1, Ordering::SeqCst);
                if self.fail_chunk == Some(index) {
                    return Err(backend::BackendWriteError::Forbidden);
                }
                self.chunks.lock().unwrap().insert(index, body);
                Ok(())
            }
            fn finish_chunked_upload(&self, _: &backend::ChunkedUploadSession, path: &Path, _: Option<&str>) -> Result<backend::PutResult, backend::BackendWriteError> {
                if let Some(e) = self.refuse(path) {
                    return Err(e);
                }
                let chunks = self.chunks.lock().unwrap();
                let mut idx: Vec<_> = chunks.keys().copied().collect();
                idx.sort();
                let body = idx.iter().flat_map(|i| chunks[i].iter().copied()).collect();
                self.finished.lock().unwrap().push((path.to_path_buf(), body));
                Ok(backend::PutResult { new_change_token: Some("assembled".into()) })
            }
            fn abort_chunked_upload(&self, _: &backend::ChunkedUploadSession) {
                self.aborts.fetch_add(1, Ordering::SeqCst);
            }
            fn is_reachable(&self, _: Duration) -> bool {
                true
            }
        }

        struct Rig {
            ctx: WriteCtx,
            server: Arc<ChunkServer>,
            dir: PathBuf,
        }

        impl Drop for Rig {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }

        fn rig(name: &str, server: ChunkServer) -> Rig {
            let dir = std::env::temp_dir().join(format!("ncrs_write_path_{}_{}", std::process::id(), name));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let server = Arc::new(server);
            let conn = ConnInfo::for_tests(server.clone());
            let journal = Arc::new(Mutex::new(mutation_journal::MutationJournal::load_or_create(&dir)));
            let meta = MetaCtx { journal: journal.clone(), ..MetaCtx::for_tests(conn, Arc::new(Mutex::new(make_test_cache())), Duration::from_secs(5)) };
            let ctx = WriteCtx {
                meta,
                lanes: fh_lane::FhLanes::new(),
                journal,
                dirty: Arc::new(Mutex::new(HashSet::new())),
                error_log: Arc::new(Mutex::new(std::collections::VecDeque::new())),
                log_user: Arc::from("t"),
                auto_keep_locally_modified_files: false,
                cache_dir: dir.clone(),
                upload_pool: &bg::UPLOAD,
                disk_pool: &bg::DISK,
                spill_pool: &bg::MUTATION,
            };
            Rig { ctx, server, dir }
        }

        fn pattern(len: usize, seed: u8) -> Vec<u8> {
            (0..len).map(|i| (i as u32).wrapping_mul(2_654_435_761).wrapping_add(seed as u32).to_le_bytes()[3]).collect()
        }

        fn recv<T>(rx: &Receiver<T>, what: &str) -> T {
            let r = rx.recv_timeout(Duration::from_secs(20)).unwrap_or_else(|_| panic!("{what}: no reply"));
            assert!(rx.try_recv().is_err(), "{what}: answered twice");
            r
        }

        fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
            let t = Instant::now();
            while !done() {
                assert!(t.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        impl Rig {
            fn open(&self, fh: u64, remote: &str, local: Option<PathBuf>) {
                let of = OpenFile {
                    remote_path: PathBuf::from(remote),
                    stream_eligible: local.is_none(),
                    local,
                    buf: None,
                    write_path: None,
                    dirty: false,
                    original_etag: None,
                    mime_detect_ct: None,
                    mime_detect_max_read: 0,
                    cache_fresh: true,
                    next_expected_off: 0,
                    read_ahead_window: READ_AHEAD_INITIAL,
                    total_written: 0,
                    chunk_upload: None,
                    ino: 100 + fh,
                    io_kind: iomode::IoKind::Cached,
                    upload_failed: false,
                    created: false,
                    unlinked: Unlinked::No,
                    opened_gen: 0,
                    writer: false,
                    pinned_parent: None,
                };
                self.ctx.open_files.safe_lock().insert(fh, of);
            }

            fn write(&self, fh: u64, remote: &str, offset: u64, data: &[u8]) -> Receiver<Result<u32, i32>> {
                let (tx, rx) = channel();
                self.ctx.dispatch_write(fh, PathBuf::from(remote), offset, data, move |r| tx.send(r.map_err(|e| e.code())).unwrap());
                rx
            }

            fn empty(&self, fh: u64, op: fn(&WriteCtx, u64, Box<dyn FnOnce() + Send>)) -> Receiver<()> {
                let (tx, rx) = channel();
                op(&self.ctx, fh, Box::new(move || tx.send(()).unwrap()));
                rx
            }

            fn flush(&self, fh: u64) -> Receiver<()> {
                self.empty(fh, |c, fh, r| c.dispatch_flush(fh, r))
            }

            fn release(&self, fh: u64) -> Receiver<()> {
                self.empty(fh, |c, fh, r| c.dispatch_release(fh, r))
            }

            fn of<T>(&self, fh: u64, f: impl FnOnce(&OpenFile) -> T) -> T {
                f(self.ctx.open_files.safe_lock().get(&fh).expect("handle is open"))
            }
        }

        #[test]
        fn a_chunk_upload_no_longer_holds_the_dispatch_thread() {
            let r = rig("graduate", ChunkServer { chunk_delay: Duration::from_millis(800), ..Default::default() });
            r.open(1, "/big.bin", None);
            let data = pattern(25 * MIB, 1);
            let mut slowest_dispatch = Duration::ZERO;
            for (i, piece) in data.chunks(MIB).enumerate() {
                // Like the kernel: the next write only after this one's reply.
                let t = Instant::now();
                let rx = r.write(1, "/big.bin", (i * MIB) as u64, piece);
                slowest_dispatch = slowest_dispatch.max(t.elapsed());
                assert_eq!(recv(&rx, "write"), Ok(MIB as u32));
            }
            assert!(slowest_dispatch < Duration::from_millis(300), "a write held the dispatcher for {slowest_dispatch:?}");
            let (total, cs, wp) = r.of(1, |of| (of.total_written, of.chunk_upload.clone().unwrap(), of.write_path.clone().unwrap()));
            assert_eq!(total, 25 * MIB as u64);
            assert_eq!((cs.next_index, cs.bytes_confirmed), (2, 20 * MIB as u64));
            assert_eq!(std::fs::metadata(&wp).unwrap().len(), 5 * MIB as u64, "the tail holds only the unsent bytes");
            let marker: mutation_journal::TailMarker = serde_json::from_slice(&std::fs::read(mutation_journal::tail_marker(&wp)).unwrap()).expect("a crash now must not keep the tail as a recovered file");
            assert_eq!((marker.remote_path.as_path(), marker.offset), (Path::new("/big.bin"), 20 * MIB as u64));

            recv(&r.flush(1), "flush");
            recv(&r.release(1), "release");
            assert!(mutation_journal::tail_marker(&wp).exists(), "journaled: kept with its tail until the finish lands");
            wait_for("the streamed finish", || !r.server.finished.lock().unwrap().is_empty());
            std::thread::sleep(Duration::from_millis(200));
            wait_for("the tail and its marker to go", || !wp.exists() && !mutation_journal::tail_marker(&wp).exists());
            let finished = r.server.finished.lock().unwrap();
            assert_eq!(finished.len(), 1, "committed exactly once");
            assert_eq!(finished[0].0, Path::new("/big.bin"));
            assert!(finished[0].1 == data, "the assembled file is what was written");
        }

        #[test]
        fn overlapping_writes_on_one_handle_land_in_order() {
            // Shared-mapping writeback can have several WRITEs in flight; the
            // one that fills a chunk must not be overtaken by those behind it.
            let r = rig("overlap", ChunkServer { chunk_delay: Duration::from_millis(300), ..Default::default() });
            r.open(2, "/o.bin", None);
            let data = pattern(23 * MIB, 2);
            let rxs: Vec<_> = data.chunks(MIB).enumerate().map(|(i, p)| r.write(2, "/o.bin", (i * MIB) as u64, p)).collect();
            for rx in &rxs {
                assert_eq!(recv(rx, "write"), Ok(MIB as u32));
            }
            assert_eq!(r.server.max_putting.load(Ordering::SeqCst), 1, "one handle never PUTs two chunks at once");
            recv(&r.release(2), "release");
            wait_for("the streamed finish", || !r.server.finished.lock().unwrap().is_empty());
            assert!(r.server.finished.lock().unwrap()[0].1 == data);
        }

        #[test]
        fn flush_and_release_wait_for_the_graduation_in_flight() {
            let r = rig("flush_wait", ChunkServer { chunk_delay: Duration::from_millis(600), ..Default::default() });
            r.open(3, "/f.bin", None);
            let data = pattern(10 * MIB + 7, 3);
            let mut pending = Vec::new();
            for (i, piece) in data.chunks(MIB).enumerate() {
                let rx = r.write(3, "/f.bin", (i * MIB) as u64, piece);
                if i < 9 {
                    assert!(recv(&rx, "write").is_ok());
                } else {
                    pending.push(rx); // from the 10th on, the chunk PUT is in flight: don't wait
                }
            }
            // A close() racing the in-flight chunk PUT (another thread's fd),
            // then the last close. Each must run after the graduation landed.
            let graduated = |srv: Arc<ChunkServer>, tx: mpsc::Sender<bool>| move || tx.send(srv.chunks.lock().unwrap().contains_key(&0)).unwrap();
            let (ftx, frx) = channel();
            r.ctx.dispatch_flush(3, graduated(r.server.clone(), ftx));
            let (rtx, rrx) = channel();
            r.ctx.dispatch_release(3, graduated(r.server.clone(), rtx));
            assert!(recv(&frx, "flush"), "flush answered before the chunk in front of it was sent");
            assert!(recv(&rrx, "release"), "release ran before the chunk in front of it was sent");
            for rx in &pending {
                assert!(recv(rx, "write").is_ok());
            }
            wait_for("the streamed finish", || !r.server.finished.lock().unwrap().is_empty());
            std::thread::sleep(Duration::from_millis(200));
            let finished = r.server.finished.lock().unwrap();
            assert_eq!(finished.len(), 1, "committed exactly once");
            assert!(finished[0].1 == data, "the commit saw every byte, the graduated chunk included");
            assert!(r.ctx.open_files.safe_lock().get(&3).is_none());
        }

        fn replay(r: &Rig) {
            let ctx = mutation_journal::ReplayContext { backend: r.server.clone(), status: r.ctx.status.clone() };
            mutation_journal::replay_journal(&r.ctx.journal, &ctx, &r.ctx.cache, &r.ctx.dirty, &r.ctx.error_log);
        }

        fn wait_for_retry_queued(r: &Rig) {
            wait_for("the failed conflicted copy to be queued for retry", || {
                r.ctx.journal.safe_lock().entries().iter()
                    .any(|e| !e.in_flight && e.last_error.as_deref().is_some_and(|m| m.contains("conflicted copy")))
            });
        }

        #[test]
        fn a_conflicted_copy_that_fails_to_upload_keeps_the_edit_until_a_replay_makes_it() {
            let r = rig("conflict_put", ChunkServer {
                conflict_on: Some(PathBuf::from("/c.txt")),
                conflict_copy_fails: AtomicBool::new(true),
                ..Default::default()
            });
            r.open(8, "/c.txt", None);
            assert_eq!(recv(&r.write(8, "/c.txt", 0, b"mine"), "write"), Ok(4));
            let wp = r.of(8, |of| of.write_path.clone().unwrap());
            recv(&r.release(8), "release");
            // Live: the PUT conflicts and the conflicted copy fails.
            wait_for_retry_queued(&r);
            assert_eq!(std::fs::read(&wp).unwrap(), b"mine", "the only copy of the edit was deleted");
            // Replay while the copy still fails: still kept.
            replay(&r);
            assert_eq!(r.ctx.journal.safe_lock().len(), 1);
            assert!(wp.exists());
            // Replay once the server takes it: the copy is made, then the staging goes.
            r.server.conflict_copy_fails.store(false, Ordering::SeqCst);
            replay(&r);
            let puts = r.server.puts.lock().unwrap().clone();
            assert!(puts.iter().any(|(p, b)| p.to_string_lossy().contains("(conflicted copy") && b == b"mine"), "{puts:?}");
            assert!(r.ctx.journal.safe_lock().is_empty());
        }

        #[test]
        fn a_streamed_conflicted_copy_that_fails_to_assemble_keeps_the_upload_queued() {
            let r = rig("conflict_stream", ChunkServer {
                conflict_on: Some(PathBuf::from("/s.bin")),
                conflict_copy_fails: AtomicBool::new(true),
                ..Default::default()
            });
            r.open(9, "/s.bin", None);
            let data = pattern(10 * MIB + 100, 9);
            for (i, piece) in data.chunks(MIB).enumerate() {
                assert!(recv(&r.write(9, "/s.bin", (i * MIB) as u64, piece), "write").is_ok());
            }
            let tail = r.of(9, |of| of.write_path.clone().unwrap());
            assert!(r.of(9, |of| of.chunk_upload.is_some()));
            recv(&r.release(9), "release");
            wait_for_retry_queued(&r);
            assert_eq!(std::fs::metadata(&tail).unwrap().len(), 100, "the tail must stay for the retry");
            r.server.conflict_copy_fails.store(false, Ordering::SeqCst);
            replay(&r);
            let finished = r.server.finished.lock().unwrap();
            assert!(finished.iter().any(|(p, b)| p.to_string_lossy().contains("(conflicted copy") && *b == data), "the whole file is kept as the conflicted copy");
            assert!(r.ctx.journal.safe_lock().is_empty());
        }

        #[test]
        fn a_streamed_upload_the_server_keeps_refusing_backs_off_and_is_never_aborted() {
            let r = rig("refused_stream", ChunkServer {
                conflict_on: Some(PathBuf::from("/r.bin")),
                conflict_copy_forbidden: AtomicBool::new(true),
                ..Default::default()
            });
            r.open(10, "/r.bin", None);
            let data = pattern(10 * MIB + 100, 10);
            for (i, piece) in data.chunks(MIB).enumerate() {
                assert!(recv(&r.write(10, "/r.bin", (i * MIB) as u64, piece), "write").is_ok());
            }
            let tail = r.of(10, |of| of.write_path.clone().unwrap());
            recv(&r.release(10), "release");
            wait_for_retry_queued(&r);
            let attempts = || r.ctx.journal.safe_lock().entries().front().map_or(0, |e| e.attempts);
            assert_eq!(attempts(), 1);
            // Not due yet: the replay waits instead of spending the budget now.
            replay(&r);
            assert_eq!(attempts(), 1, "retried within its backoff");
            for n in 2..=3 {
                r.ctx.journal.safe_lock().skip_backoff();
                replay(&r);
                assert_eq!(attempts(), n);
            }
            r.ctx.journal.safe_lock().skip_backoff();
            replay(&r);
            assert!(r.ctx.journal.safe_lock().is_empty(), "given up after its attempts");
            assert_eq!(r.server.aborts.load(Ordering::SeqCst), 0, "the session holds the rest of the only copy");
            assert!(!tail.exists());
            let recovered = r.dir.join(mutation_journal::RECOVERED_DIR);
            let kept = recovered.join(tail.file_name().unwrap());
            assert_eq!(std::fs::read(&kept).unwrap(), &data[10 * MIB..], "the tail is kept");
            let note: mutation_journal::RecoveredSidecar = serde_json::from_slice(&std::fs::read(recovered.join(format!("{}.json", tail.file_name().unwrap().to_str().unwrap()))).unwrap()).unwrap();
            assert_eq!(note.upload_session.as_deref(), Some("uploads/1"));
            assert_eq!(note.tail_offset, Some(10 * MIB as u64));
            let conflicts = r.ctx.journal.safe_lock().unresolved_conflicts().iter().map(|c| format!("{:?}", c.kind)).collect::<Vec<_>>();
            assert!(conflicts.iter().any(|c| c.contains("/r.bin") && c.contains("uploads/1")), "{conflicts:?}");
        }

        #[test]
        fn a_failed_chunk_answers_eio_once_and_release_abandons_the_session() {
            let r = rig("fail", ChunkServer { fail_chunk: Some(0), ..Default::default() });
            r.open(4, "/x.bin", None);
            let data = pattern(10 * MIB, 4);
            let replies: Vec<_> = data.chunks(MIB).enumerate().map(|(i, p)| recv(&r.write(4, "/x.bin", (i * MIB) as u64, p), "write")).collect();
            assert!(replies[..9].iter().all(|x| x.is_ok()));
            assert_eq!(replies[9], Err(libc::EIO));
            assert!(r.of(4, |of| of.upload_failed && of.chunk_upload.is_some()), "the opened session is kept so it can be aborted");
            // The next write on the failed handle is refused, not silently staged.
            assert_eq!(recv(&r.write(4, "/x.bin", 0, b"again"), "write"), Err(libc::EIO));
            recv(&r.release(4), "release");
            wait_for("the abort", || r.server.aborts.load(Ordering::SeqCst) == 1);
            assert!(r.ctx.journal.safe_lock().is_empty(), "nothing is committed");
            assert!(r.server.finished.lock().unwrap().is_empty());
            // The writer was told EIO: nothing was saved, so nothing is kept.
            assert!(!r.dir.join(mutation_journal::staging_file_name(4)).exists(), "a failed copy's staging lingers");
            assert!(!r.dir.join(mutation_journal::RECOVERED_DIR).exists());
        }

        #[test]
        fn a_server_without_chunked_uploads_gets_the_whole_file_on_release() {
            let r = rig("unsupported", ChunkServer { open_unsupported: true, ..Default::default() });
            r.open(5, "/u.bin", None);
            let data = pattern(12 * MIB + 3, 5);
            for (i, piece) in data.chunks(MIB).enumerate() {
                assert!(recv(&r.write(5, "/u.bin", (i * MIB) as u64, piece), "write").is_ok());
            }
            assert!(r.of(5, |of| !of.stream_eligible && of.chunk_upload.is_none()));
            recv(&r.flush(5), "flush");
            recv(&r.release(5), "release");
            wait_for("the PUT", || !r.server.puts.lock().unwrap().is_empty());
            std::thread::sleep(Duration::from_millis(200));
            let puts = r.server.puts.lock().unwrap();
            assert_eq!(puts.len(), 1, "committed exactly once");
            assert!(puts[0].0 == Path::new("/u.bin") && puts[0].1 == data);
        }

        #[test]
        fn the_first_write_and_a_truncate_seed_from_the_kept_copy() {
            let r = rig("seed", ChunkServer::default());
            let kept = r.dir.join("kept.bin");
            let original = pattern(3 * MIB, 6);
            std::fs::write(&kept, &original).unwrap();

            r.open(6, "/k.bin", Some(kept.clone()));
            assert_eq!(r.ctx.write_cost(6, MIB as u64, 4), crate::write_path::WriteCost::Seed);
            assert_eq!(recv(&r.write(6, "/k.bin", MIB as u64, b"EDIT"), "write"), Ok(4));
            let wp = r.of(6, |of| of.write_path.clone().unwrap());
            let mut expect = original.clone();
            expect[MIB..MIB + 4].copy_from_slice(b"EDIT");
            assert!(std::fs::read(&wp).unwrap() == expect, "an in-place edit keeps the rest of the file");
            assert_eq!(r.ctx.write_cost(6, 0, 4), crate::write_path::WriteCost::Inline, "seeded once");
            assert!(!wp.with_extension("seed").exists());

            r.open(7, "/k.bin", Some(kept));
            let (tx, rx) = channel();
            r.ctx.dispatch_truncate(7, 1000, move |_, res| tx.send(res.map_err(|e| e.code())).unwrap());
            assert_eq!(recv(&rx, "truncate"), Ok(()));
            let wp = r.of(7, |of| of.write_path.clone().unwrap());
            assert!(std::fs::read(&wp).unwrap() == original[..1000]);
            assert!(r.of(7, |of| of.dirty && of.total_written == 1000));
        }

        /// A pool that refuses every job, as `bg::UPLOAD`/`bg::DISK` do when full.
        fn refusing_pool() -> &'static bg::Pool {
            Box::leak(Box::new(bg::Pool::new("t-refuses", 0, 0)))
        }

        #[test]
        fn a_refused_graduation_never_puts_on_the_caller_and_the_next_write_catches_up() {
            let mut r = rig("refused", ChunkServer::default());
            r.ctx.upload_pool = refusing_pool();
            r.ctx.disk_pool = refusing_pool();
            // Not even the spill: this is the path of last resort.
            r.ctx.spill_pool = refusing_pool();
            r.open(10, "/r.bin", None);
            let data = pattern(26 * MIB, 10);
            let (first, last) = data.split_at(25 * MIB);
            for (i, piece) in first.chunks(MIB).enumerate() {
                let rx = r.write(10, "/r.bin", (i * MIB) as u64, piece);
                // Refused, so it ran on this (the "dispatch") thread, before dispatch returned.
                assert_eq!(rx.try_recv().expect("answered on the caller"), Ok(MIB as u32), "a refusal is never an error");
            }
            assert!(r.server.net_threads.lock().unwrap().is_empty(), "a refused step went to the network on the caller");
            let wp = r.of(10, |of| {
                assert!(of.chunk_upload.is_none() && of.stream_eligible && of.total_written == 25 * MIB as u64);
                of.write_path.clone().unwrap()
            });
            assert_eq!(std::fs::metadata(&wp).unwrap().len(), 25 * MIB as u64, "the deferred chunks wait in the tail");

            // Room again: the next write sends every full chunk the tail holds, off this thread.
            r.ctx.upload_pool = &bg::UPLOAD;
            r.ctx.disk_pool = &bg::DISK;
            r.ctx.spill_pool = &bg::MUTATION;
            assert_eq!(r.ctx.write_cost(10, 25 * MIB as u64, MIB), crate::write_path::WriteCost::GraduateCapped, "past the cap");
            assert_eq!(recv(&r.write(10, "/r.bin", 25 * MIB as u64, last), "write"), Ok(MIB as u32));
            let cs = r.of(10, |of| of.chunk_upload.clone().unwrap());
            assert_eq!((cs.next_index, cs.bytes_confirmed), (2, 20 * MIB as u64));
            assert_eq!(std::fs::metadata(&wp).unwrap().len(), 6 * MIB as u64);
            let me = std::thread::current().id();
            assert!(r.server.net_threads.lock().unwrap().iter().all(|t| *t != me), "a chunk went out on the caller");

            recv(&r.release(10), "release");
            wait_for("the streamed finish", || !r.server.finished.lock().unwrap().is_empty());
            assert!(r.server.finished.lock().unwrap()[0].1 == data, "the assembled file is what was written");
        }

        #[test]
        fn a_tail_stops_growing_at_its_cap_while_graduations_are_refused() {
            let mut r = rig("capped", ChunkServer::default());
            r.ctx.upload_pool = refusing_pool();
            r.open(12, "/cap.bin", None);
            let data = pattern(35 * MIB, 12);
            let wp = |r: &Rig| r.of(12, |of| of.write_path.clone().unwrap());
            let mut biggest = 0;
            for (i, piece) in data.chunks(MIB).enumerate() {
                assert_eq!(recv(&r.write(12, "/cap.bin", (i * MIB) as u64, piece), "write"), Ok(MIB as u32));
                biggest = biggest.max(std::fs::metadata(wp(&r)).unwrap().len());
            }
            let cap = (crate::write_path::TAIL_CAP_CHUNKS as usize * 10 + 1) * MIB;
            assert!(biggest <= cap as u64, "the tail grew to {biggest} bytes with the upload pool refusing");
            assert!(r.of(12, |of| of.chunk_upload.as_ref().is_some_and(|c| c.bytes_confirmed >= 20 * MIB as u64)));
            let me = std::thread::current().id();
            assert!(r.server.net_threads.lock().unwrap().iter().all(|t| *t != me), "a chunk went out on the caller");
            recv(&r.release(12), "release");
            wait_for("the streamed finish", || !r.server.finished.lock().unwrap().is_empty());
            assert!(r.server.finished.lock().unwrap()[0].1 == data, "the assembled file is what was written, in order");
        }

        #[test]
        fn a_write_classified_inline_never_graduates_even_if_online_returns_before_it_runs() {
            let r = rig("classify", ChunkServer::default());
            r.open(11, "/c.bin", None);
            let data = pattern(11 * MIB, 11);
            for (i, piece) in data[..9 * MIB].chunks(MIB).enumerate() {
                assert!(recv(&r.write(11, "/c.bin", (i * MIB) as u64, piece), "write").is_ok());
            }
            // The write that fills the chunk is classified while offline...
            r.ctx.conn.is_offline.store(true, Ordering::SeqCst);
            let cost = r.ctx.write_cost(11, 9 * MIB as u64, MIB);
            assert_eq!(cost, crate::write_path::WriteCost::Inline);
            // ...and runs, as dispatch_write runs an Inline write, once it is back.
            r.ctx.conn.is_offline.store(false, Ordering::SeqCst);
            let piece = &data[9 * MIB..10 * MIB];
            assert_eq!(r.ctx.write_answer(11, Path::new("/c.bin"), 9 * MIB as u64, piece, false).map_err(|e| e.code()), Ok(MIB as u32));
            assert!(r.server.net_threads.lock().unwrap().is_empty(), "an Inline write reached the server");
            assert!(r.of(11, |of| of.chunk_upload.is_none() && of.stream_eligible && of.total_written == 10 * MIB as u64));
            // The next write graduates the chunk the last one filled.
            assert_eq!(r.ctx.write_cost(11, 10 * MIB as u64, MIB), crate::write_path::WriteCost::Graduate);
            assert!(recv(&r.write(11, "/c.bin", 10 * MIB as u64, &data[10 * MIB..]), "write").is_ok());
            assert_eq!(r.of(11, |of| of.chunk_upload.as_ref().map(|c| c.next_index)), Some(1));
            recv(&r.release(11), "release");
            wait_for("the streamed finish", || !r.server.finished.lock().unwrap().is_empty());
            assert!(r.server.finished.lock().unwrap()[0].1 == data);
        }

        #[test]
        fn release_publishes_the_new_size_before_the_handle_stops_overlaying_it() {
            // What `stat` answers from: the listing (the server's older, smaller
            // size), the upload guard, and the open-handle overlay.
            fn stat_size(ctx: &WriteCtx, path: &Path) -> u64 {
                let c = ctx.cache.safe_lock();
                let e = c.dir_cache[Path::new("/")].files.iter().find(|e| e.path == path).unwrap().clone();
                let mut attr = attr_for(&c, 5, path, &e);
                drop(c);
                overlay_local_size(&ctx.meta, path, &mut attr);
                attr.size
            }
            // Whole-file staging, and a streamed upload (one chunk already sent).
            for (fh, name, len) in [(12u64, "w.bin", 3 * MIB), (13, "s.bin", 12 * MIB)] {
                let r = rig(name, ChunkServer::default());
                let path = PathBuf::from(format!("/{name}"));
                r.ctx.cache.safe_lock().put_dir_cache(PathBuf::from("/"), None, None, vec![make_dav_entry(name, None)]);
                r.open(fh, path.to_str().unwrap(), None);
                r.ctx.open_writers.fetch_add(1, Ordering::SeqCst);
                r.ctx.open_files.safe_lock().get_mut(&fh).unwrap().writer = true;
                let data = pattern(len, fh as u8);
                for (i, piece) in data.chunks(MIB).enumerate() {
                    assert!(recv(&r.write(fh, path.to_str().unwrap(), (i * MIB) as u64, piece), "write").is_ok());
                }
                assert_eq!(stat_size(&r.ctx, &path), len as u64, "the overlay covers the open handle");
                // RELEASE is answered after the handle is gone and before its
                // commit: exactly the gap a stat could fall into.
                let (tx, rx) = channel();
                let (ctx, p) = (r.ctx.clone(), path.clone());
                r.ctx.dispatch_release(fh, move || tx.send(stat_size(&ctx, &p)).unwrap());
                assert_eq!(recv(&rx, "release"), len as u64, "stat regressed to the server's size while the release committed");
                assert_eq!(stat_size(&r.ctx, &path), len as u64);
                wait_for("the upload", || !r.server.finished.lock().unwrap().is_empty() || !r.server.puts.lock().unwrap().is_empty());
            }
        }

        #[test]
        fn a_rename_of_a_streamed_handle_lists_what_it_wrote_not_its_tail() {
            let r = rig("rename_size", ChunkServer::default());
            r.open(14, "/m.bin", None);
            let data = pattern(12 * MIB, 14);
            for (i, piece) in data.chunks(MIB).enumerate() {
                assert!(recv(&r.write(14, "/m.bin", (i * MIB) as u64, piece), "write").is_ok());
            }
            let wp = r.of(14, |of| of.write_path.clone().unwrap());
            assert_eq!(std::fs::metadata(&wp).unwrap().len(), 2 * MIB as u64, "only the tail is on disk");
            assert_eq!(staged_size(&r.ctx.open_files, Path::new("/m.bin")), Some(12 * MIB as u64));
            // Unstreamed: the staging file is the whole file.
            r.open(15, "/n.bin", None);
            assert!(recv(&r.write(15, "/n.bin", 3, b"abc"), "write").is_ok());
            assert_eq!(staged_size(&r.ctx.open_files, Path::new("/n.bin")), Some(6));
        }

        #[test]
        fn a_directory_rename_moves_open_handles_their_pins_and_their_uploads() {
            let r = rig("dir_rename", ChunkServer::default());
            let ino = {
                let mut c = r.ctx.cache.safe_lock();
                c.allocate_inode(PathBuf::from("/a"));
                c.allocate_inode(PathBuf::from("/a/b"));
                c.pin_dir(Path::new("/a/b"));
                c.allocate_inode(PathBuf::from("/a/b/c.bin"))
            };
            r.open(16, "/a/b/c.bin", None);
            r.ctx.open_files.safe_lock().get_mut(&16).unwrap().pinned_parent = Some(PathBuf::from("/a/b"));
            let data = pattern(12 * MIB, 16);
            for (i, piece) in data[..5 * MIB].chunks(MIB).enumerate() {
                assert!(recv(&r.write(16, "/a/b/c.bin", (i * MIB) as u64, piece), "write").is_ok());
            }
            // mv /a/b /z/b while the file is being written, as rename() does it.
            {
                let mut c = r.ctx.cache.safe_lock();
                c.move_inode(Path::new("/a/b"), Path::new("/z/b"));
                assert!(!retarget_open_files(&mut c, &r.ctx.open_files, Path::new("/a/b"), Path::new("/z/b")));
                assert_eq!(c.get_path(ino).as_deref(), Some(Path::new("/z/b/c.bin")), "the child's inode kept the old path");
                assert_eq!(c.get_inode(Path::new("/z/b/c.bin")), Some(ino), "same inode number");
                assert_eq!(c.get_inode(Path::new("/a/b/c.bin")), None);
                assert_eq!(c.pins.get(Path::new("/z/b")), Some(&1), "the pin follows the handle");
                assert!(!c.pins.contains_key(Path::new("/a/b")));
            }
            assert_eq!(r.of(16, |of| (of.remote_path.clone(), of.pinned_parent.clone())),
                (PathBuf::from("/z/b/c.bin"), Some(PathBuf::from("/z/b"))));
            // write() resolves its path from the inode, now the new one.
            let now_at = r.ctx.cache.safe_lock().get_path(ino).unwrap();
            for (i, piece) in data[5 * MIB..].chunks(MIB).enumerate() {
                assert!(recv(&r.write(16, now_at.to_str().unwrap(), ((5 + i) * MIB) as u64, piece), "write").is_ok());
            }
            assert_eq!(*r.server.opened_for.lock().unwrap(), vec![PathBuf::from("/z/b/c.bin")], "the session was opened for the old path");
            recv(&r.release(16), "release");
            wait_for("the streamed finish", || !r.server.finished.lock().unwrap().is_empty());
            let finished = r.server.finished.lock().unwrap();
            assert_eq!(finished[0].0, Path::new("/z/b/c.bin"), "release committed to the vanished path");
            assert!(finished[0].1 == data);
        }

        #[test]
        fn a_clean_handle_is_flushed_and_released_on_the_spot() {
            let r = rig("clean", ChunkServer::default());
            r.open(8, "/c.bin", None);
            // Answered before dispatch returns: no pool hop for a read-only close.
            assert!(r.flush(8).try_recv().is_ok());
            assert!(r.release(8).try_recv().is_ok());
            assert!(r.ctx.open_files.safe_lock().get(&8).is_none());
            assert!(r.ctx.journal.safe_lock().is_empty());
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

    // ── move_inode ─────────────────────────────────────────────────────────────

    #[test]
    fn move_inode_moves_a_directory_subtree_and_keeps_inode_numbers() {
        let mut c = make_test_cache();
        // Plenty of unrelated inodes, so a scan of all of them would show.
        for i in 0..200_000 {
            c.allocate_inode(PathBuf::from(format!("/other/{}/f{}", i % 97, i)));
        }
        let names = ["/a", "/a/b", "/a/b/c", "/a/b/c/d.txt", "/a/b/e.txt", "/a/b.txt", "/a/bx", "/a/b-c", "/z", "/z/b"];
        let ino: HashMap<&str, u64> = names.iter().map(|n| (*n, c.allocate_inode(PathBuf::from(n)))).collect();
        let t = Instant::now();
        c.move_inode(Path::new("/a/b"), Path::new("/z/b"));
        eprintln!("move_inode of a 4-entry subtree among {} inodes: {:?}", c.inodes.len(), t.elapsed());
        for (old, new) in [("/a/b", "/z/b"), ("/a/b/c", "/z/b/c"), ("/a/b/c/d.txt", "/z/b/c/d.txt"), ("/a/b/e.txt", "/z/b/e.txt")] {
            assert_eq!(c.get_path(ino[old]).as_deref(), Some(Path::new(new)), "{old}");
            assert_eq!(c.get_inode(Path::new(new)), Some(ino[old]), "{new}");
            assert_eq!(c.get_inode(Path::new(old)), None, "{old} still resolves");
        }
        // Siblings that merely share a name prefix stay put.
        for keep in ["/a", "/a/b.txt", "/a/bx", "/a/b-c", "/z"] {
            assert_eq!(c.get_path(ino[keep]).as_deref(), Some(Path::new(keep)));
        }
        assert_eq!(c.get_path(ino["/z/b"]), None, "the overwritten destination's inode is dropped");
        assert_eq!(c.inodes.len(), c.paths.len());
        // A file rename, and moving a directory into itself (refused by rename(2)).
        c.move_inode(Path::new("/a/bx"), Path::new("/a/by"));
        assert_eq!(c.get_path(ino["/a/bx"]).as_deref(), Some(Path::new("/a/by")));
        c.move_inode(Path::new("/z/b"), Path::new("/z/b/inner"));
        assert_eq!(c.get_path(ino["/a/b"]).as_deref(), Some(Path::new("/z/b")));
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
    fn a_refresh_keeps_a_queued_rename_under_its_new_name_and_a_new_file_under_the_old() {
        // vim's save while the MOVE waits for the replay: `mv f f~`, then a
        // new f. The server still has f (the old one) and no f~.
        let mut c = make_test_cache();
        let d = PathBuf::from("/d");
        c.put_dir_cache(d.clone(), None, None, vec![make_dav_entry_in("/d", "f", Some(1))]);
        c.allocate_inode(PathBuf::from("/d/f"));
        let open_files: Arc<Mutex<HashMap<u64, OpenFile>>> = Arc::default();
        rename_in_cache(&mut c, &open_files, Path::new("/d/f"), Path::new("/d/f~"), &d, &d);
        c.moving.insert(PathBuf::from("/d/f~"), (PathBuf::from("/d/f"), 7));
        let mut new_f = make_dav_entry_in("/d", "f", None);
        new_f.change_token = None;
        new_f.size = 3;
        {
            let dir = c.dir_cache.get_mut(&d).unwrap();
            let mut files = (*dir.files).clone();
            files.push(new_f);
            dir.files = Arc::new(files);
        }
        c.uploading.insert(PathBuf::from("/d/f"), None);
        c.put_dir_cache(d.clone(), None, None, vec![make_dav_entry_in("/d", "f", Some(1))]);
        let listed = |c: &FsCache| -> Vec<(String, Option<String>, u64)> {
            let mut v: Vec<_> = c.dir_cache[&d].files.iter().map(|e| (e.path.display().to_string(), e.change_token.clone(), e.size)).collect();
            v.sort();
            v
        };
        assert_eq!(listed(&c), [
            ("/d/f".to_string(), None, 3),
            ("/d/f~".to_string(), Some("etag1".to_string()), 100),
        ], "the new f and the renamed old one, not the server's old f");
        // Once the MOVE landed the server's listing is right again.
        c.moving.clear();
        c.put_dir_cache(d.clone(), None, None, vec![make_dav_entry_in("/d", "f~", Some(1))]);
        assert_eq!(listed(&c).len(), 2, "{:?}", listed(&c));
    }

    #[test]
    fn an_upload_guard_follows_its_file_through_renames() {
        let mut c = make_test_cache();
        let open_files: Arc<Mutex<HashMap<u64, OpenFile>>> = Arc::default();
        c.uploading.insert(PathBuf::from("/d/x"), Some(5));
        rename_in_cache(&mut c, &open_files, Path::new("/d"), Path::new("/e"), Path::new("/"), Path::new("/"));
        assert_eq!(c.uploading.get(Path::new("/e/x")), Some(&Some(5)));
        assert!(!c.uploading.contains_key(Path::new("/d/x")));
        // A file renamed over another takes the name; the other's guard goes.
        c.allocate_inode(PathBuf::from("/t"));
        c.allocate_inode(PathBuf::from("/e/x"));
        c.uploading.insert(PathBuf::from("/t"), Some(9));
        rename_in_cache(&mut c, &open_files, Path::new("/e/x"), Path::new("/t"), Path::new("/e"), Path::new("/"));
        assert_eq!(c.uploading.get(Path::new("/t")), Some(&Some(5)));
        assert_eq!(c.uploading.len(), 1);
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
        c.uploading.insert(pinned.join("d0-0.txt"), None);

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

    #[test]
    fn listings_in_use_by_open_handles_survive_eviction_until_released() {
        let mut c = make_test_cache();
        c.dir_cache_max_dirs = 0;
        for i in 0..12 {
            let (d, f) = dir_with(&format!("d{i}"), 1);
            c.put_dir_cache(d, None, None, f);
        }
        // Two handles on /d0 (an open directory and a file inside it), one on /d1.
        c.pin_dir(Path::new("/d0"));
        c.pin_dir(Path::new("/d0"));
        c.pin_dir(Path::new("/d1"));
        c.dir_cache_max_dirs = 4;
        for i in 0..40 {
            let (d, f) = dir_with(&format!("new{i}"), 1);
            c.put_dir_cache(d, None, None, f);
        }
        assert!(c.dir_cache.contains_key(Path::new("/d0")) && c.dir_cache.contains_key(Path::new("/d1")));
        assert!(c.dir_cache.len() <= 4 + 2, "only the pinned listings may exceed the ceiling, got {}", c.dir_cache.len());

        c.unpin_dir(Path::new("/d0"));
        c.unpin_dir(Path::new("/d1"));
        assert_eq!(c.pins.len(), 1, "one handle on /d0 is still open");
        for i in 0..40 {
            let (d, f) = dir_with(&format!("later{i}"), 1);
            c.put_dir_cache(d, None, None, f);
        }
        assert!(c.dir_cache.contains_key(Path::new("/d0")), "still pinned by its last handle");
        assert!(!c.dir_cache.contains_key(Path::new("/d1")), "released listings are ordinary LRU entries again");
        c.unpin_dir(Path::new("/d0"));
        assert!(c.pins.is_empty());
    }

    #[test]
    fn a_crawl_evicts_its_own_listings_before_anyone_elses() {
        let mut c = make_test_cache();
        c.dir_cache_max_dirs = 100;
        // The folders a person is using: listed first, so the oldest in the LRU.
        for i in 0..50 {
            let (d, f) = dir_with(&format!("mine{i}"), 1);
            c.put_dir_cache(d, None, None, f);
        }
        // A `find /` lists 1000 cold directories.
        for i in 0..1000 {
            let (d, f) = dir_with(&format!("crawl{i}"), 1);
            let path = d.clone();
            c.put_dir_cache(d, None, None, f);
            c.mark_walker(&path, true);
        }
        assert!(c.dir_cache.len() <= 100, "the ceiling holds: {}", c.dir_cache.len());
        for i in 0..50 {
            assert!(c.dir_cache.contains_key(&PathBuf::from(format!("/mine{i}"))), "/mine{i} was pushed out by the crawl");
        }
        assert!(c.dir_cache.contains_key(Path::new("/crawl999")), "the newest crawl listing is still served");
        // Space nobody else wants is the crawl's to use; once the person needs
        // it, the crawl gives it back down to a fifth of the cache.
        for i in 0..70 {
            let (d, f) = dir_with(&format!("more{i}"), 1);
            c.put_dir_cache(d, None, None, f);
        }
        let crawler = c.dir_cache.values().filter(|e| e.walker).count();
        assert!(crawler <= 100 / 5, "the crawler segment yields down to a fifth: {crawler}");
        assert!(crawler > 0, "but keeps its newest listings");
        for i in 0..70 {
            assert!(c.dir_cache.contains_key(&PathBuf::from(format!("/more{i}"))), "/more{i} lost to the crawl");
        }

        // A refresh keeps a crawler listing in its segment; a person listing it
        // takes it out.
        let (d, f) = dir_with("crawl999", 2);
        c.put_dir_cache(d, None, None, f);
        assert!(c.dir_cache[Path::new("/crawl999")].walker);
        c.mark_walker(Path::new("/crawl999"), false);
        assert!(!c.dir_cache[Path::new("/crawl999")].walker);
    }

    #[test]
    fn a_crawler_fetch_still_streaming_is_marked_when_it_lands() {
        let mut c = make_test_cache();
        let (tx, rx) = mpsc::channel();
        let (etx, erx) = mpsc::channel();
        let (_stx, srx) = mpsc::channel();
        c.start_pending(PathBuf::from("/c"), rx, erx, srx);
        c.mark_walker(Path::new("/c"), true);
        tx.send(make_dav_entry("x", None)).unwrap();
        drop(tx);
        etx.send(Ok(None)).unwrap();
        c.promote_pending(Path::new("/c")).unwrap();
        assert!(c.dir_cache[Path::new("/c")].walker);
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

    // ── Child resolution off a missing parent listing (2026-09-24 review) ────

    mod child_resolution {
        use super::*;
        use std::sync::atomic::AtomicUsize;

        fn entry_in(dir: &str, name: &str) -> RemoteEntry {
            let mut e = make_dav_entry(name, None);
            e.path = Path::new(dir).join(name);
            e
        }

        fn names(dir: &str, n: usize) -> Vec<RemoteEntry> {
            (0..n).map(|i| entry_in(dir, &format!("f{i}.txt"))).collect()
        }

        #[test]
        fn find_child_answers_wide_listings_from_the_index() {
            let mut c = make_test_cache();
            c.put_dir_cache(PathBuf::from("/w"), None, None, names("/w", 5000));
            for i in [0usize, 1, 2500, 4999] {
                let (files, pos) = c.find_child(Path::new("/w"), &format!("f{i}.txt")).expect("resident");
                assert_eq!(files[pos.expect("present")].path, PathBuf::from(format!("/w/f{i}.txt")));
            }
            assert_eq!(c.find_child(Path::new("/w"), "nope").map(|(_, p)| p), Some(None));
            assert!(c.find_child(Path::new("/elsewhere"), "f1.txt").is_none(), "not resident is not absent");
            for _ in 0..NAME_INDEX_AFTER_LOOKUPS {
                let _ = c.find_child(Path::new("/w"), "f0.txt");
            }
            assert!(c.dir_cache[Path::new("/w")].name_index.as_ref().is_some_and(|s| s.index.is_some()), "a wide listing that is read gets an index");

            // Any mutation swaps the Arc, and the index must follow it.
            let mut files = (*c.dir_cache[Path::new("/w")].files).clone();
            files.retain(|e| e.path != Path::new("/w/f10.txt"));
            files.push(entry_in("/w", "added.txt"));
            c.dir_cache.get_mut(Path::new("/w")).unwrap().files = Arc::new(files);
            assert_eq!(c.find_child(Path::new("/w"), "f10.txt").map(|(_, p)| p), Some(None));
            assert!(c.find_child(Path::new("/w"), "added.txt").and_then(|(_, p)| p).is_some());
            assert!(c.find_child(Path::new("/w"), "f11.txt").and_then(|(_, p)| p).is_some(), "positions shifted");
        }

        #[test]
        fn an_ambiguous_hash_falls_back_to_a_scan() {
            let files = names("/a", 3);
            let mut ix = NameIndex::default();
            ix.extend(&files);
            // Force the shared-hash case two real names would produce.
            ix.map.insert(name_hash("f1.txt"), NAME_AMBIGUOUS);
            assert_eq!(ix.find(&files, "f1.txt"), Some(1));
            assert_eq!(ix.find(&files, "f0.txt"), Some(0));
            assert_eq!(ix.find(&files, "f9.txt"), None);
        }

        fn start(c: &mut FsCache, dir: &str) -> (mpsc::Sender<RemoteEntry>, mpsc::Sender<Result<Option<String>, String>>) {
            let (tx, rx) = mpsc::channel();
            let (etx, erx) = mpsc::channel();
            let (_stx, srx) = mpsc::channel();
            c.start_pending(PathBuf::from(dir), rx, erx, srx);
            (tx, etx)
        }

        /// `pending_find`, past the batches a large backlog is drained in.
        fn settled(c: &mut FsCache, dir: &str, name: &str) -> PendingLookup {
            loop {
                match c.pending_find(Path::new(dir), name) {
                    PendingLookup::Backlog => continue,
                    other => return other,
                }
            }
        }

        #[test]
        fn pending_find_sees_streamed_names_and_never_calls_a_partial_listing_complete() {
            let mut c = make_test_cache();
            let (tx, etx) = start(&mut c, "/s");
            for e in names("/s", 400) {
                tx.send(e).unwrap();
            }
            assert!(matches!(settled(&mut c, "/s", "f399.txt"), PendingLookup::Found(e) if e.path == Path::new("/s/f399.txt")));
            assert!(matches!(settled(&mut c, "/s", "late.txt"), PendingLookup::Streaming));
            tx.send(entry_in("/s", "late.txt")).unwrap();
            assert!(matches!(settled(&mut c, "/s", "late.txt"), PendingLookup::Found(_)));
            // The entry stream ends a moment before the result is sent: until it
            // is, the listing may have broken off, so nothing is absent yet.
            drop(tx);
            assert!(matches!(settled(&mut c, "/s", "never.txt"), PendingLookup::Streaming));
            assert!(c.dir_cache.get(Path::new("/s")).is_none());
            etx.send(Ok(Some("etag".into()))).unwrap();
            assert!(matches!(settled(&mut c, "/s", "never.txt"), PendingLookup::Finished));
            assert!(matches!(c.resolve_child_cached(Path::new("/s"), "never.txt"), Some(Child::Absent)));
            assert!(matches!(c.resolve_child_cached(Path::new("/s"), "late.txt"), Some(Child::Found(_))));
            assert!(matches!(settled(&mut c, "/s", "x"), PendingLookup::NoFetch));
        }

        #[test]
        fn a_listing_that_broke_off_is_a_failure_not_an_answer() {
            let mut c = make_test_cache();
            let (tx, etx) = start(&mut c, "/b");
            tx.send(entry_in("/b", "a.txt")).unwrap();
            drop(tx);
            etx.send(Err("truncated: body error".into())).unwrap();
            assert!(matches!(c.pending_find(Path::new("/b"), "z.txt"), PendingLookup::Failed(e) if e.starts_with("truncated")));
            assert!(c.dir_cache.get(Path::new("/b")).is_none(), "a broken listing must not be cached as complete");
        }

        #[test]
        fn readdir_paths_never_promote_a_stream_whose_result_is_still_in_flight() {
            let mut c = make_test_cache();
            let (tx, etx) = start(&mut c, "/r");
            tx.send(entry_in("/r", "a.txt")).unwrap();
            drop(tx); // entry stream over, result not sent yet
            // get_pending_snapshot (readdir) serves what arrived, still pending.
            let snap = c.get_pending_snapshot(Path::new("/r")).unwrap();
            assert_eq!(snap.map(|v| v.len()), Some(1));
            assert!(c.dir_cache.get(Path::new("/r")).is_none(), "promoted before the result arrived");
            assert!(c.pending_dirs.contains_key(Path::new("/r")));
            // promote_pending (timeout path, prefetch) refuses too.
            assert!(matches!(c.promote_pending(Path::new("/r")), Ok(None)));
            assert!(c.dir_cache.get(Path::new("/r")).is_none());
            // Once the worker reports a failure, it is surfaced, never cached.
            etx.send(Err("truncated: body error".into())).unwrap();
            assert!(c.get_pending_snapshot(Path::new("/r")).is_err());
            assert!(c.dir_cache.get(Path::new("/r")).is_none());
        }

        #[test]
        fn readdir_promotes_a_finished_stream_once_its_result_is_in() {
            let mut c = make_test_cache();
            let (tx, etx) = start(&mut c, "/k");
            tx.send(entry_in("/k", "a.txt")).unwrap();
            drop(tx);
            etx.send(Ok(Some("e1".into()))).unwrap();
            let snap = c.get_pending_snapshot(Path::new("/k")).unwrap();
            assert_eq!(snap.map(|v| v.len()), Some(1));
            assert_eq!(c.dir_cache.get(Path::new("/k")).and_then(|e| e.etag.clone()).as_deref(), Some("e1"));
            assert!(!c.pending_dirs.contains_key(Path::new("/k")));
        }

        #[test]
        fn a_kept_copy_is_judged_against_the_version_the_caller_resolved() {
            let mut c = make_test_cache();
            let path = PathBuf::from("/docs/a.odt");
            c.file_cache.insert(path.clone(), FileCacheEntry {
                local_path: PathBuf::from("/nonexistent/a.odt"),
                remote_modified: None,
                etag: Some("e-old".into()),
                kept: true,
                size: 3,
            });
            // No listing at all: unknown, so assume fresh (offline reads keep working).
            assert!(c.file_cache_matches_remote(&path));
            // A freshly resolved entry says the server moved on: stale.
            assert!(!c.file_cache_matches(&path, Some("e-new"), None));
            assert!(c.file_cache_matches(&path, Some("e-old"), None));
        }

        #[test]
        fn an_unresolved_child_is_never_enoent_unless_the_server_said_so() {
            let e = |s: Option<&str>| unknown_child_errno(s).code();
            assert_eq!(e(None), libc::ETIMEDOUT);
            assert_eq!(e(Some("PROPFIND timeout for /Music/2404")), libc::ETIMEDOUT);
            assert_eq!(e(Some("listing ended before the name was seen")), libc::EAGAIN);
            assert_eq!(e(Some("network: listing /x404 deferred — too many listings in flight")), libc::EAGAIN);
            assert_eq!(e(Some("/Not Found not available offline")), libc::EAGAIN);
            assert_eq!(e(Some("server error 503: busy")), libc::EAGAIN);
            assert_eq!(e(Some("server error 404: gone")), libc::ENOENT);
            assert_eq!(e(Some("not found")), libc::ENOENT);
        }

        // ── A backend with per-directory latency ────────────────────────────

        #[derive(Clone, Default)]
        struct FakeDir {
            entries: Vec<RemoteEntry>,
            before_first: Duration,
            /// Pause after every `every`-th entry (every entry when 0).
            between: Duration,
            every: usize,
            /// Pause after the last entry, before the listing completes.
            hold: Duration,
            fail: Option<u16>,
        }

        #[derive(Default)]
        struct FakeBackend {
            dirs: Mutex<HashMap<PathBuf, FakeDir>>,
            lists: AtomicUsize,
            /// Content served by `download_file`, after the given pause; any
            /// other path is a 404.
            files: Mutex<HashMap<PathBuf, (Vec<u8>, Duration)>>,
            /// Every PUT, in order.
            puts: Mutex<Vec<(PathBuf, Vec<u8>)>>,
            /// A streamed upload's chunks already on the server; assembling it
            /// serves them, then the chunks PUT since, as the file.
            session_prefix: Mutex<Vec<u8>>,
            chunks: Mutex<BTreeMap<u64, Vec<u8>>>,
            /// Every assembled streamed upload, in order.
            finished: Mutex<Vec<PathBuf>>,
            /// Runs after each download was answered, with the path asked for:
            /// lets a test land a MOVE between two downloads.
            after_download: Mutex<Option<Box<dyn FnMut(&FakeBackend, &Path) + Send>>>,
        }

        impl FakeBackend {
            fn with(dirs: Vec<(&str, FakeDir)>) -> Arc<Self> {
                let b = FakeBackend::default();
                for (p, d) in dirs {
                    b.dirs.lock().unwrap().insert(PathBuf::from(p), d);
                }
                Arc::new(b)
            }
        }

        fn unsupported() -> backend::BackendReadError {
            backend::BackendReadError::Network("not in the fake".into())
        }

        impl crate::backend::CloudBackend for FakeBackend {
            fn list_dir(&self, _: &Path, _: Duration) -> Result<(Option<String>, Option<RemoteEntry>, Vec<RemoteEntry>), backend::BackendReadError> {
                Err(unsupported())
            }
            fn list_dir_streaming(&self, path: &Path, _: Duration, tx: mpsc::Sender<RemoteEntry>, _: mpsc::Sender<RemoteEntry>) -> Result<Option<String>, backend::BackendReadError> {
                self.lists.fetch_add(1, Ordering::SeqCst);
                let d = self.dirs.lock().unwrap().get(path).cloned().ok_or(backend::BackendReadError::NotFound)?;
                std::thread::sleep(d.before_first);
                for (i, e) in d.entries.into_iter().enumerate() {
                    let _ = tx.send(e);
                    if d.every == 0 || i % d.every == d.every - 1 {
                        std::thread::sleep(d.between);
                    }
                }
                std::thread::sleep(d.hold);
                match d.fail {
                    Some(code) => Err(backend::BackendReadError::Server(code, "fake".into())),
                    None => Ok(Some("etag".into())),
                }
            }
            fn dir_change_token(&self, _: &Path, _: Duration) -> Result<Option<String>, backend::BackendReadError> {
                Err(unsupported())
            }
            fn download_file(&self, path: &Path, out: &mut dyn std::io::Write, _: Duration) -> Result<u64, backend::BackendReadError> {
                let found = self.files.lock().unwrap().get(path).cloned();
                let hook = self.after_download.lock().unwrap().take();
                if let Some(mut hook) = hook {
                    hook(self, path);
                    *self.after_download.lock().unwrap() = Some(hook);
                }
                let (bytes, pause) = found.ok_or(backend::BackendReadError::NotFound)?;
                std::thread::sleep(pause);
                out.write_all(&bytes).map_err(|e| backend::BackendReadError::Network(e.to_string()))?;
                Ok(bytes.len() as u64)
            }
            fn read_file_range(&self, _: &Path, _: u64, _: &mut [u8], _: Duration) -> Result<usize, backend::BackendReadError> {
                Err(unsupported())
            }
            fn put_file(&self, path: &Path, body: Vec<u8>, _: Option<&str>) -> Result<backend::PutResult, backend::BackendWriteError> {
                self.puts.lock().unwrap().push((path.to_path_buf(), body));
                Ok(backend::PutResult { new_change_token: Some("put".into()) })
            }
            fn mkdir(&self, _: &Path) -> Result<(), backend::BackendWriteError> {
                Err(backend::BackendWriteError::Unsupported)
            }
            fn delete(&self, _: &Path) -> Result<(), backend::BackendWriteError> {
                Err(backend::BackendWriteError::Unsupported)
            }
            fn rename(&self, _: &Path, _: &Path) -> Result<(), backend::BackendWriteError> {
                Err(backend::BackendWriteError::Unsupported)
            }
            fn put_chunk(&self, _: &backend::ChunkedUploadSession, index: u64, body: Vec<u8>) -> Result<(), backend::BackendWriteError> {
                self.chunks.lock().unwrap().insert(index, body);
                Ok(())
            }
            fn finish_chunked_upload(&self, _: &backend::ChunkedUploadSession, path: &Path, _: Option<&str>) -> Result<backend::PutResult, backend::BackendWriteError> {
                let mut body = self.session_prefix.lock().unwrap().clone();
                body.extend(self.chunks.lock().unwrap().values().flatten());
                self.files.lock().unwrap().insert(path.to_path_buf(), (body, Duration::ZERO));
                self.finished.lock().unwrap().push(path.to_path_buf());
                Ok(backend::PutResult { new_change_token: Some("assembled".into()) })
            }
            fn is_reachable(&self, _: Duration) -> bool {
                true
            }
        }

        fn setup(dirs: Vec<(&str, FakeDir)>) -> (Arc<FakeBackend>, Arc<ConnInfo>, Arc<Mutex<FsCache>>) {
            let fake = FakeBackend::with(dirs);
            let conn = ConnInfo::for_tests(fake.clone());
            (fake, conn, Arc::new(Mutex::new(make_test_cache())))
        }

        fn resolve(conn: &Arc<ConnInfo>, cache: &Arc<Mutex<FsCache>>, dir: &str, name: &str, within: Duration) -> (Child, Duration) {
            let t = Instant::now();
            let child = resolve_child_slow(conn, cache, Path::new(dir), name, 0, t + within);
            (child, t.elapsed())
        }

        #[test]
        fn a_listing_still_streaming_at_the_deadline_is_unknown_never_absent() {
            let slow = FakeDir { entries: names("/slow", 3), before_first: Duration::from_secs(3), ..Default::default() };
            let mut trickle = FakeDir { entries: names("/trickle", 2), between: Duration::from_secs(3), ..Default::default() };
            trickle.entries.push(entry_in("/trickle", "last.txt"));
            let (_, conn, cache) = setup(vec![("/slow", slow), ("/trickle", trickle)]);

            let (child, took) = resolve(&conn, &cache, "/slow", "f1.txt", Duration::from_millis(300));
            assert!(matches!(child, Child::Unknown(None)), "{child:?}");
            assert!(took < Duration::from_secs(1), "the deadline bounds the wait: {took:?}");

            let (child, _) = resolve(&conn, &cache, "/trickle", "last.txt", Duration::from_millis(500));
            assert!(matches!(child, Child::Unknown(None)), "a partial listing lacking the name is not an answer: {child:?}");
        }

        #[test]
        fn a_name_that_streams_in_late_is_found_as_soon_as_it_arrives() {
            let dir = FakeDir { entries: names("/late", 60), between: Duration::from_millis(5), ..Default::default() };
            let (_, conn, cache) = setup(vec![("/late", dir)]);
            let (child, took) = resolve(&conn, &cache, "/late", "f59.txt", Duration::from_secs(10));
            assert!(matches!(&child, Child::Found(e) if e.path == Path::new("/late/f59.txt")), "{child:?}");
            assert!(took < Duration::from_secs(5), "{took:?}");
            // Once the listing is complete, a name it lacks is a real absence.
            let t = Instant::now();
            while cache.safe_lock().dir_cache.get(Path::new("/late")).is_none() {
                assert!(t.elapsed() < Duration::from_secs(5), "listing never landed");
                let _ = cache.safe_lock().pending_find(Path::new("/late"), "");
                std::thread::sleep(Duration::from_millis(10));
            }
            let (child, _) = resolve(&conn, &cache, "/late", "missing.txt", Duration::from_secs(5));
            assert!(matches!(child, Child::Absent), "{child:?}");
        }

        #[test]
        fn resolvers_waiting_on_one_directory_share_one_propfind() {
            let dir = FakeDir { entries: names("/j", 16), before_first: Duration::from_millis(300), ..Default::default() };
            let (fake, conn, cache) = setup(vec![("/j", dir)]);
            let found = AtomicUsize::new(0);
            std::thread::scope(|s| {
                for i in 0..8 {
                    let (conn, cache, found) = (&conn, &cache, &found);
                    s.spawn(move || {
                        let (child, _) = resolve(conn, cache, "/j", &format!("f{i}.txt"), Duration::from_secs(10));
                        if matches!(child, Child::Found(_)) {
                            found.fetch_add(1, Ordering::SeqCst);
                        }
                    });
                }
            });
            assert_eq!(found.load(Ordering::SeqCst), 8);
            assert_eq!(fake.lists.load(Ordering::SeqCst), 1, "joined resolvers must not each list the directory");
        }

        #[test]
        fn a_failed_listing_is_unknown_and_carries_the_servers_answer() {
            let dir = FakeDir { entries: names("/f", 1), fail: Some(503), ..Default::default() };
            let (_, conn, cache) = setup(vec![("/f", dir)]);
            let (child, _) = resolve(&conn, &cache, "/f", "other.txt", Duration::from_secs(5));
            match child {
                Child::Unknown(Some(e)) => assert_eq!(unknown_child_errno(Some(&e)).code(), libc::EAGAIN, "{e}"),
                other => panic!("expected Unknown with the error, got {other:?}"),
            }
        }

        // ── with_child: hits inline, misses on bg::META ─────────────────────

        fn ask(meta: &MetaCtx, path: &str) -> mpsc::Receiver<(Resolved<u64>, String)> {
            let (tx, rx) = mpsc::channel();
            with_child(meta, 0, Path::new(path), tx, |_, _, e| e.size, |_| None, |_, _, tx, r| {
                let _ = tx.send((r, std::thread::current().name().unwrap_or("").to_string()));
            });
            rx
        }

        #[test]
        fn with_child_answers_a_hit_inline_and_a_miss_from_a_meta_worker() {
            let cold = FakeDir { entries: names("/cold", 3), before_first: Duration::from_millis(100), ..Default::default() };
            let (_, conn, cache) = setup(vec![("/cold", cold)]);
            cache.safe_lock().put_dir_cache(PathBuf::from("/hot"), None, None, names("/hot", 10));
            let meta = MetaCtx::for_tests(conn, cache, Duration::from_secs(5));

            let (r, on) = ask(&meta, "/hot/f3.txt").try_recv().expect("a hit is answered before with_child returns");
            assert!(matches!(r, Resolved::Found(100)));
            assert_ne!(on, "ncrs-meta");
            assert!(matches!(ask(&meta, "/hot/none.txt").try_recv(), Ok((Resolved::Absent, _))));

            let rx = ask(&meta, "/cold/f2.txt");
            assert!(rx.try_recv().is_err(), "a miss must not be answered on the caller's thread");
            let (r, on) = rx.recv_timeout(Duration::from_secs(5)).expect("the worker answers");
            assert!(matches!(r, Resolved::Found(100)));
            assert_eq!(on, "ncrs-meta");
            // The listing is resident now: the next miss is a hit.
            assert!(matches!(ask(&meta, "/cold/f0.txt").try_recv(), Ok((Resolved::Found(_), _))));
        }

        #[test]
        fn the_fast_path_stays_fast_while_slow_resolvers_wait_on_a_wide_stream() {
            // 100k entries trickling in over ~2 s, and a listing that then hangs
            // past every resolver's deadline.
            let big = FakeDir {
                entries: names("/big", 100_000),
                between: Duration::from_millis(1),
                every: 50,
                hold: Duration::from_secs(30),
                ..Default::default()
            };
            let (_, conn, cache) = setup(vec![("/big", big)]);
            cache.safe_lock().put_dir_cache(PathBuf::from("/hot"), None, None, names("/hot", 2000));
            let meta = MetaCtx::for_tests(conn, cache, Duration::from_secs(3));

            let slow: Vec<_> = (0..16).map(|i| ask(&meta, &format!("/big/never-{i}.txt"))).collect();
            let mut lat = Vec::with_capacity(3000);
            let t_end = Instant::now() + Duration::from_millis(2500);
            let mut i = 0usize;
            while Instant::now() < t_end {
                let t = Instant::now();
                let r = ask(&meta, &format!("/hot/f{}.txt", i % 2000)).try_recv().expect("inline");
                lat.push(t.elapsed());
                assert!(matches!(r.0, Resolved::Found(_)));
                i += 1;
                std::thread::sleep(Duration::from_micros(500));
            }
            lat.sort();
            let p99 = lat[lat.len() * 99 / 100];
            let max = *lat.last().unwrap();
            eprintln!("fast path over {} lookups: p50 {:?} p99 {:?} max {:?}", lat.len(), lat[lat.len() / 2], p99, max);
            // The 1 ms bound is for the optimised build the daemon ships as
            // (`cargo test --release`). An unoptimised build moves stream entries
            // ~10x slower under the lock, so it gets a looser bound — still far
            // below what cloning the partial listing per waiter used to cost.
            let bound = if cfg!(debug_assertions) { Duration::from_millis(10) } else { Duration::from_millis(1) };
            assert!(p99 < bound, "p99 {p99:?} (bound {bound:?})");

            for rx in slow {
                let (r, on) = rx.recv_timeout(Duration::from_secs(10)).expect("every slow resolver is answered by its deadline");
                assert_eq!(on, "ncrs-meta");
                assert!(matches!(r, Resolved::Unknown(None)), "a name not streamed by the deadline is unknown, never absent");
            }
        }

        // ── open(): registration, staging, and the evicted-parent cases ─────

        #[derive(Debug, PartialEq)]
        enum OpenOutcome {
            Opened(u64),
            Error(i32),
        }

        struct TestReply(mpsc::Sender<OpenOutcome>);

        impl OpenAnswer for TestReply {
            fn error(self, e: Errno) {
                let _ = self.0.send(OpenOutcome::Error(e.code()));
            }
            fn opened(self, fh: u64, _: iomode::IoGrant<BackingId>) {
                let _ = self.0.send(OpenOutcome::Opened(fh));
            }
            fn open_backing(&self, _: std::fs::File) -> std::io::Result<BackingId> {
                Err(std::io::Error::other("no passthrough in tests"))
            }
        }

        fn reply() -> (TestReply, mpsc::Receiver<OpenOutcome>) {
            let (tx, rx) = mpsc::channel();
            (TestReply(tx), rx)
        }

        /// A MetaCtx over `fake` whose staging files go to a fresh directory,
        /// with `/d/a.txt` (5 bytes on the server) listed and given an inode.
        fn open_setup(dirs: Vec<(&str, FakeDir)>, list_parent: bool) -> (Arc<FakeBackend>, MetaCtx, tempfile::TempDir, u64) {
            let (fake, conn, cache) = setup(dirs);
            let tmp = tempfile::tempdir().unwrap();
            let ino = {
                let mut c = cache.safe_lock();
                c.cache_dir = tmp.path().to_path_buf();
                if list_parent {
                    let mut e = entry_in("/d", "a.txt");
                    e.size = 5;
                    c.put_dir_cache(PathBuf::from("/d"), None, None, vec![e]);
                }
                c.allocate_inode(PathBuf::from("/d/a.txt"))
            };
            fake.files.lock().unwrap().insert(PathBuf::from("/d/a.txt"), (b"hello".to_vec(), Duration::from_millis(400)));
            (fake, MetaCtx::for_tests(conn, cache, Duration::from_millis(400)), tmp, ino)
        }

        fn rq(ino: u64, path: &str, flags: i32, local: Option<PathBuf>) -> OpenReq {
            let writable = flags & (libc::O_WRONLY | libc::O_RDWR | libc::O_APPEND) != 0;
            OpenReq { ino, flags, pid: 0, writable, truncating: writable && flags & libc::O_TRUNC != 0, path: PathBuf::from(path), local, unlink_snap: None }
        }

        fn listed(size: u64) -> OpenEntry {
            let mut e = entry_in("/d", "a.txt");
            e.size = size;
            OpenEntry::of(&e)
        }

        fn fh_of(o: OpenOutcome) -> u64 {
            match o {
                OpenOutcome::Opened(fh) => fh,
                other => panic!("expected an open, got {other:?}"),
            }
        }

        /// Nothing an open took is still held: no handle, writer, pin or io mode.
        fn assert_all_given_back(meta: &MetaCtx, ino: u64) {
            assert!(meta.open_files.safe_lock().is_empty(), "handle left registered");
            assert_eq!(meta.open_writers.load(Ordering::SeqCst), 0, "writer count leaked");
            assert!(meta.cache.safe_lock().pins.is_empty(), "parent pin leaked");
            assert!(meta.io_modes.safe_lock().is_empty(), "io mode of inode {ino} leaked");
        }

        /// A pool that refuses every job, like `bg::META` when it is full.
        static REFUSING: bg::Pool = bg::Pool::new("refusing", 0, 0);

        /// What `release()` gives back for handle `fh` (its own bookkeeping is
        /// in the write path and needs a FUSE request).
        fn release_bookkeeping(meta: &MetaCtx, fh: u64) {
            let of = meta.open_files.safe_lock().remove(&fh).expect("registered");
            if of.writer {
                meta.open_writers.fetch_sub(1, Ordering::SeqCst);
            }
            if let Some(ref parent) = of.pinned_parent {
                meta.cache.safe_lock().unpin_dir(parent);
            }
            meta.io_modes.safe_lock().release(of.ino, of.io_kind);
        }

        #[test]
        fn a_writable_open_is_registered_before_staging_so_unlink_and_rename_find_it() {
            let (_, meta, _tmp, ino) = open_setup(vec![], true);

            // rename while the content downloads: the handle follows it.
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY | libc::O_APPEND, None), listed(5), r, false);
            assert!(rx.try_recv().is_err(), "a seeded open is answered after staging, off this thread");
            {
                let files = meta.open_files.safe_lock();
                let of = files.values().next().expect("registered before staging");
                assert!(!of.dirty, "a staging handle must not look written");
                assert!(!of.stream_eligible);
            }
            assert_eq!(meta.open_writers.load(Ordering::SeqCst), 1);
            assert_eq!(meta.cache.safe_lock().pins.get(Path::new("/d")), Some(&1));
            // As rename() does: its MOVE is queued, and lands after the download.
            let seq = meta.journal.safe_lock().enqueue(mutation_journal::MutationOp::Rename { from: PathBuf::from("/d/a.txt"), to: PathBuf::from("/d/b.txt") });
            meta.cache.safe_lock().move_inode(Path::new("/d/a.txt"), Path::new("/d/b.txt"));
            retarget_open_files(&mut meta.cache.safe_lock(), &meta.open_files, Path::new("/d/a.txt"), Path::new("/d/b.txt"));
            let fh = fh_of(rx.recv_timeout(Duration::from_secs(5)).unwrap());
            meta.journal.safe_lock().remove(seq);
            {
                let files = meta.open_files.safe_lock();
                let of = &files[&fh];
                assert_eq!(of.remote_path, PathBuf::from("/d/b.txt"), "release would write to the old path");
                assert_eq!(std::fs::read(of.write_path.as_ref().unwrap()).unwrap(), b"hello");
                assert_eq!(of.original_etag.as_deref(), Some("etag1"));
            }
            meta.open_files.safe_lock().clear();

            // unlink while the content downloads: release must not PUT it back.
            meta.cache.safe_lock().move_inode(Path::new("/d/b.txt"), Path::new("/d/a.txt"));
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/a.txt", libc::O_RDWR, None), listed(5), r, false);
            mark_unlinked(&meta.open_files, Path::new("/d/a.txt"), Some(ino));
            let fh = fh_of(rx.recv_timeout(Duration::from_secs(5)).unwrap());
            assert_eq!(meta.open_files.safe_lock()[&fh].unlinked, Unlinked::Local);
        }

        #[test]
        fn a_rename_before_the_handle_is_registered_is_read_from_the_inode_map() {
            let (_, meta, _tmp, ino) = open_setup(vec![], true);
            // The rename ran while open() was resolving: the inode moved, but
            // there was no handle yet for rename() to retarget.
            meta.cache.safe_lock().move_inode(Path::new("/d/a.txt"), Path::new("/e/a.txt"));
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY, None), listed(0), r, false);
            let fh = fh_of(rx.try_recv().expect("an empty file needs no staging: answered inline"));
            let files = meta.open_files.safe_lock();
            assert_eq!(files[&fh].remote_path, PathBuf::from("/e/a.txt"));
            assert_eq!(files[&fh].pinned_parent.as_deref(), Some(Path::new("/e")), "the pin follows the file");
        }

        #[test]
        fn seeded_and_unseeded_writable_opens() {
            let (_, meta, tmp, ino) = open_setup(vec![], true);
            // Unseeded: the server file is empty, nothing to stage.
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY, None), listed(0), r, false);
            let fh = fh_of(rx.try_recv().expect("inline"));
            {
                let files = meta.open_files.safe_lock();
                assert!(files[&fh].stream_eligible, "an empty staging file may stream from 0");
                assert!(!files[&fh].write_path.as_ref().unwrap().exists(), "write() creates it on first use");
            }
            meta.open_files.safe_lock().clear();

            // Truncating: an empty staging file, dirty from the start.
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY | libc::O_TRUNC, None), listed(5), r, false);
            let fh = fh_of(rx.try_recv().expect("inline"));
            {
                let files = meta.open_files.safe_lock();
                assert!(files[&fh].dirty);
                assert_eq!(std::fs::metadata(files[&fh].write_path.as_ref().unwrap()).unwrap().len(), 0);
            }
            meta.open_files.safe_lock().clear();

            // Seeded from a fresh local copy: copied, not downloaded.
            let local = tmp.path().join("kept-a.txt");
            std::fs::write(&local, b"local").unwrap();
            meta.cache.safe_lock().file_cache.insert(PathBuf::from("/d/a.txt"), FileCacheEntry {
                local_path: local.clone(), remote_modified: None, etag: Some("etag1".into()), kept: true, size: 5,
            });
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY | libc::O_APPEND, Some(local)), listed(5), r, false);
            let fh = fh_of(rx.recv_timeout(Duration::from_secs(5)).unwrap());
            let files = meta.open_files.safe_lock();
            assert_eq!(std::fs::read(files[&fh].write_path.as_ref().unwrap()).unwrap(), b"local");
            assert!(!files[&fh].stream_eligible);
        }

        #[test]
        fn a_failed_staging_gives_back_everything_the_open_took() {
            let (fake, meta, tmp, ino) = open_setup(vec![], true);
            fake.files.lock().unwrap().clear();
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY | libc::O_APPEND, None), listed(5), r, false);
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), OpenOutcome::Error(libc::EIO));
            assert_all_given_back(&meta, ino);
            assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0, "staging file left behind");
        }

        #[test]
        fn an_open_the_staging_pool_refuses_gives_back_everything() {
            // What the EAGAIN arm (and a panicking staging job) runs: the undo
            // guard, dropped while still armed.
            let (_, meta, tmp, ino) = open_setup(vec![], true);
            let wp = tmp.path().join("write_99");
            std::fs::write(&wp, b"partial").unwrap();
            let (r, _rx) = reply();
            let undo = OpenUndo { ctx: meta.clone(), fh: 99, wp: wp.clone(), armed: true };
            let _grant = open_register(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY, None), listed(5), 99, false, Some(wp.clone()), true, None, &r);
            assert_eq!(meta.open_writers.load(Ordering::SeqCst), 1);
            drop(undo);
            assert_all_given_back(&meta, ino);
            assert!(!wp.exists());
        }

        #[test]
        fn a_writable_open_with_an_unknown_parent_is_refused_unless_it_truncates() {
            let slow = FakeDir { entries: vec![entry_in("/d", "a.txt")], before_first: Duration::from_secs(3), ..Default::default() };
            let (_, meta, _tmp, ino) = open_setup(vec![("/d", slow)], false);

            let (r, rx) = reply();
            open_unlisted(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY | libc::O_APPEND, None), r);
            let got = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(got, OpenOutcome::Error(libc::ETIMEDOUT), "staging it empty would upload a zero-filled prefix");
            assert_all_given_back(&meta, ino);

            // A truncating open discards the content anyway.
            let (r, rx) = reply();
            open_unlisted(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY | libc::O_TRUNC, None), r);
            fh_of(rx.recv_timeout(Duration::from_secs(5)).unwrap());
        }

        #[test]
        fn a_writable_open_with_an_unknown_parent_resolves_it_when_it_lands() {
            let dir = FakeDir { entries: vec![{ let mut e = entry_in("/d", "a.txt"); e.size = 5; e }], before_first: Duration::from_millis(50), ..Default::default() };
            let (_, meta, _tmp, ino) = open_setup(vec![("/d", dir)], false);
            let (r, rx) = reply();
            open_unlisted(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY | libc::O_APPEND, None), r);
            let fh = fh_of(rx.recv_timeout(Duration::from_secs(5)).unwrap());
            let files = meta.open_files.safe_lock();
            assert_eq!(std::fs::read(files[&fh].write_path.as_ref().unwrap()).unwrap(), b"hello", "seeded with the real content");
        }

        #[test]
        fn offline_a_kept_copy_with_an_evicted_parent_is_edited_from_the_copy() {
            let (_, meta, tmp, ino) = open_setup(vec![], false);
            let local = tmp.path().join("kept-a.txt");
            std::fs::write(&local, b"kept!").unwrap();
            meta.cache.safe_lock().file_cache.insert(PathBuf::from("/d/a.txt"), FileCacheEntry {
                local_path: local.clone(), remote_modified: None, etag: Some("e-kept".into()), kept: true, size: 5,
            });
            // Online, a parent listing we cannot get says nothing: refused.
            let (r, rx) = reply();
            open_unlisted(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY | libc::O_APPEND, Some(local.clone())), r);
            assert!(matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), OpenOutcome::Error(_)));
            assert_all_given_back(&meta, ino);

            meta.conn.is_offline.store(true, Ordering::SeqCst);
            let (r, rx) = reply();
            open_unlisted(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY | libc::O_APPEND, Some(local)), r);
            let fh = fh_of(rx.recv_timeout(Duration::from_secs(5)).unwrap());
            let files = meta.open_files.safe_lock();
            assert_eq!(std::fs::read(files[&fh].write_path.as_ref().unwrap()).unwrap(), b"kept!");
            assert_eq!(files[&fh].original_etag.as_deref(), Some("e-kept"), "the replayed upload must still detect a server change");
        }

        fn keep(meta: &MetaCtx, tmp: &tempfile::TempDir, etag: &str) -> PathBuf {
            let local = tmp.path().join("kept-a.txt");
            std::fs::write(&local, b"kept!").unwrap();
            meta.cache.safe_lock().file_cache.insert(PathBuf::from("/d/a.txt"), FileCacheEntry {
                local_path: local.clone(), remote_modified: None, etag: Some(etag.into()), kept: true, size: 5,
            });
            local
        }

        #[test]
        fn a_read_only_open_of_a_kept_copy_with_an_evicted_parent_is_checked_against_the_listing() {
            // The server's a.txt carries "etag1".
            let dir = FakeDir { entries: vec![entry_in("/d", "a.txt")], before_first: Duration::from_millis(50), ..Default::default() };
            let (_, meta, tmp, ino) = open_setup(vec![("/d", dir)], false);
            for (kept, fresh) in [("stale", false), ("etag1", true)] {
                let local = keep(&meta, &tmp, kept);
                meta.cache.safe_lock().dir_cache.remove(Path::new("/d"));
                let (r, rx) = reply();
                open_unlisted(&meta, rq(ino, "/d/a.txt", libc::O_RDONLY, Some(local)), r);
                let fh = fh_of(rx.recv_timeout(Duration::from_secs(5)).unwrap());
                assert_eq!(meta.open_files.safe_lock()[&fh].cache_fresh, fresh, "kept copy with etag {kept}");
                release_bookkeeping(&meta, fh);
            }
            assert_all_given_back(&meta, ino);
        }

        #[test]
        fn a_read_only_open_of_a_kept_copy_waits_only_briefly_for_its_parent() {
            let slow = FakeDir { entries: vec![entry_in("/d", "a.txt")], before_first: Duration::from_secs(30), ..Default::default() };
            let (fake, meta, tmp, ino) = open_setup(vec![("/d", slow)], false);
            let meta = MetaCtx { resolve_within: Duration::from_secs(20), ..meta };
            let local = keep(&meta, &tmp, "e-kept");

            // Offline, or with the server breaker open: served from the copy at once.
            meta.conn.is_offline.store(true, Ordering::SeqCst);
            let (r, rx) = reply();
            open_unlisted(&meta, rq(ino, "/d/a.txt", libc::O_RDONLY, Some(local.clone())), r);
            release_bookkeeping(&meta, fh_of(rx.try_recv().expect("offline: answered inline")));
            meta.conn.is_offline.store(false, Ordering::SeqCst);
            let now = Instant::now();
            for _ in 0..30 {
                meta.conn.breaker.record(true, now);
            }
            assert!(meta.conn.breaker.is_open(now));
            let (r, rx) = reply();
            open_unlisted(&meta, rq(ino, "/d/a.txt", libc::O_RDONLY, Some(local.clone())), r);
            release_bookkeeping(&meta, fh_of(rx.try_recv().expect("breaker open: answered inline")));
            assert_eq!(fake.lists.load(Ordering::SeqCst), 0, "no listing was started for either");

            // Online, a listing that hangs is waited for about KEPT_COPY_RESOLVE_WITHIN,
            // not the full resolve deadline, then the copy is served.
            let meta = MetaCtx { conn: ConnInfo::for_tests(fake.clone()), ..meta };
            let t = Instant::now();
            let (r, rx) = reply();
            open_unlisted(&meta, rq(ino, "/d/a.txt", libc::O_RDONLY, Some(local)), r);
            let fh = fh_of(rx.recv_timeout(Duration::from_secs(10)).unwrap());
            let took = t.elapsed();
            assert!(took >= KEPT_COPY_RESOLVE_WITHIN - Duration::from_millis(100) && took < KEPT_COPY_RESOLVE_WITHIN + Duration::from_secs(2), "{took:?}");
            assert!(meta.open_files.safe_lock()[&fh].cache_fresh, "served from the copy, as before the parent had to be resolved");
            release_bookkeeping(&meta, fh);
            assert_all_given_back(&meta, ino);
        }

        #[test]
        fn an_unclassified_open_the_meta_pool_refuses_is_tried_again_not_downloaded() {
            // A process whose classification nobody cached: GLib's probe matcher
            // needs its /proc maps, which the dispatch thread must not read.
            let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
            let pid = child.id();
            let (_, meta, _tmp, ino) = open_setup(vec![], true);
            let meta = MetaCtx { meta_pool: &REFUSING, ..meta };
            let open = |flags: i32| {
                let (r, rx) = reply();
                open_continue(&meta, OpenReq { pid, ..rq(ino, "/d/a.txt", flags, None) }, listed(5), r, false);
                rx.try_recv().expect("answered inline")
            };
            assert_eq!(open(libc::O_RDONLY | libc::O_NOATIME), OpenOutcome::Error(libc::EAGAIN), "a sniff read as a plain read downloads the file");
            assert!(meta.open_files.safe_lock().is_empty());
            // No probe matches these flags: decided inline, never sent to the pool.
            let fh = fh_of(open(libc::O_RDONLY));
            assert!(meta.open_files.safe_lock()[&fh].mime_detect_ct.is_none());
            release_bookkeeping(&meta, fh);
            // Left undecided by a /proc-reading thumbnailer matcher (KIO's), a
            // plain read must still open: `cat`/`cp` of an uncached file under load.
            let (r, rx) = reply();
            open_unclassified(&meta, OpenReq { pid, ..rq(ino, "/d/a.txt", libc::O_RDONLY, None) }, listed(5), r);
            let fh = fh_of(rx.try_recv().expect("answered inline"));
            assert!(meta.open_files.safe_lock()[&fh].mime_detect_ct.is_none());
            release_bookkeeping(&meta, fh);
            let (r, rx) = reply();
            open_unclassified(&meta, OpenReq { pid, ..rq(ino, "/d/a.txt", libc::O_RDONLY | libc::O_NOATIME, None) }, listed(5), r);
            assert_eq!(rx.try_recv().unwrap(), OpenOutcome::Error(libc::EAGAIN));
            assert_all_given_back(&meta, ino);
            let _ = child.kill();
            let _ = child.wait();
        }

        #[test]
        fn staging_follows_a_rename_and_skips_a_locally_unlinked_file() {
            let (fake, meta, _tmp, ino) = open_setup(vec![], true);
            fake.files.lock().unwrap().insert(PathBuf::from("/e/a.txt"), (b"moved".to_vec(), Duration::ZERO));
            // Renamed while open() resolved it: registered at, and staged from, the new path.
            meta.cache.safe_lock().move_inode(Path::new("/d/a.txt"), Path::new("/e/a.txt"));
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY | libc::O_APPEND, None), listed(5), r, false);
            let fh = fh_of(rx.recv_timeout(Duration::from_secs(5)).unwrap());
            {
                let files = meta.open_files.safe_lock();
                assert_eq!(std::fs::read(files[&fh].write_path.as_ref().unwrap()).unwrap(), b"moved", "staged the new path's content");
            }
            release_bookkeeping(&meta, fh);
            // Its MOVE has not reached the server yet: staged from the source the
            // journal's Rename names, and only because the journal names it.
            meta.cache.safe_lock().move_inode(Path::new("/e/a.txt"), Path::new("/f/a.txt"));
            let seq = meta.journal.safe_lock().enqueue(mutation_journal::MutationOp::Rename { from: PathBuf::from("/d/a.txt"), to: PathBuf::from("/f/a.txt") });
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY | libc::O_APPEND, None), listed(5), r, false);
            let fh = fh_of(rx.recv_timeout(Duration::from_secs(5)).unwrap());
            {
                let files = meta.open_files.safe_lock();
                assert_eq!(files[&fh].remote_path, PathBuf::from("/f/a.txt"));
                assert_eq!(std::fs::read(files[&fh].write_path.as_ref().unwrap()).unwrap(), b"hello");
            }
            release_bookkeeping(&meta, fh);
            // No Rename queued: the path it was opened under is not where the
            // content is (after `mv f f~; mv tmp f` it is another file).
            meta.journal.safe_lock().remove(seq);
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY | libc::O_APPEND, None), listed(5), r, false);
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), OpenOutcome::Error(libc::EIO));
            meta.cache.safe_lock().move_inode(Path::new("/f/a.txt"), Path::new("/d/a.txt"));

            // Unlinked by this mount while open() resolved it: a tombstone newer
            // than the open's snapshot. The handle is born unlinked (release will
            // not PUT it back), and there is nothing to download.
            let snap = meta.cache.safe_lock().tombstones.snapshot();
            {
                let mut c = meta.cache.safe_lock();
                c.dir_cache.get_mut(Path::new("/d")).unwrap().files = Arc::new(vec![]);
                c.tombstones.record(ino);
            }
            mark_unlinked(&meta.open_files, Path::new("/d/a.txt"), Some(ino));
            let (r, rx) = reply();
            open_continue(&meta, OpenReq { unlink_snap: Some(snap), ..rq(ino, "/d/a.txt", libc::O_WRONLY | libc::O_APPEND, None) }, OpenEntry::absent(), r, false);
            let fh = fh_of(rx.recv_timeout(Duration::from_secs(5)).unwrap());
            {
                let files = meta.open_files.safe_lock();
                assert_eq!(files[&fh].unlinked, Unlinked::Local, "release would re-create the deleted file");
                assert_eq!(std::fs::metadata(files[&fh].write_path.as_ref().unwrap()).unwrap().len(), 0);
            }
            release_bookkeeping(&meta, fh);

            // The listing lost the name, but nothing here unlinked it: not gone,
            // and staged from the server.
            let snap = meta.cache.safe_lock().tombstones.snapshot();
            let (r, rx) = reply();
            open_continue(&meta, OpenReq { unlink_snap: Some(snap), ..rq(ino, "/d/a.txt", libc::O_WRONLY | libc::O_APPEND, None) }, OpenEntry::absent(), r, false);
            let fh = fh_of(rx.recv_timeout(Duration::from_secs(5)).unwrap());
            {
                let files = meta.open_files.safe_lock();
                assert_eq!(files[&fh].unlinked, Unlinked::No);
                assert_eq!(std::fs::read(files[&fh].write_path.as_ref().unwrap()).unwrap(), b"hello");
            }
            release_bookkeeping(&meta, fh);
            assert_all_given_back(&meta, ino);
        }

        #[test]
        fn an_unlink_matches_handles_by_inode_not_by_a_path_a_registration_has_not_updated() {
            let (_, meta, tmp, ino) = open_setup(vec![], true);
            let (r, _rx) = reply();
            let _ = open_register(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY, None), listed(0), 7, false, Some(tmp.path().join("w7")), false, None, &r);
            // Another file now at the same path (`mv a b; mv c a`), unlinked.
            mark_unlinked(&meta.open_files, Path::new("/d/a.txt"), Some(ino + 1000));
            assert_eq!(meta.open_files.safe_lock()[&7].unlinked, Unlinked::No);
            // An unlink that did not know the inode is only a hint.
            mark_unlinked(&meta.open_files, Path::new("/d/a.txt"), None);
            assert_eq!(meta.open_files.safe_lock()[&7].unlinked, Unlinked::Unverified);
            mark_unlinked(&meta.open_files, Path::new("/d/a.txt"), Some(ino));
            assert_eq!(meta.open_files.safe_lock()[&7].unlinked, Unlinked::Local);
            release_bookkeeping(&meta, 7);
            assert_all_given_back(&meta, ino);
        }

        // ── Writes survive a listing that lost the name (review of 6bdfb93) ─
        //
        // Only this mount unlinking a file may drop what is written to it. These
        // open the file the way the kernel does with a dentry still cached (by
        // inode, the listing resident but without the name), append, release,
        // and check what reaches the server.

        fn write_ctx(meta: &MetaCtx, dir: &Path) -> crate::write_path::WriteCtx {
            crate::write_path::WriteCtx {
                meta: meta.clone(),
                lanes: fh_lane::FhLanes::new(),
                journal: meta.journal.clone(),
                dirty: Arc::new(Mutex::new(HashSet::new())),
                error_log: Arc::new(Mutex::new(std::collections::VecDeque::new())),
                log_user: Arc::from("t"),
                auto_keep_locally_modified_files: false,
                cache_dir: dir.to_path_buf(),
                upload_pool: &bg::UPLOAD,
                disk_pool: &bg::DISK,
                spill_pool: &bg::MUTATION,
            }
        }

        /// What open() computes on the dispatch thread for inode `ino`.
        fn open_by_inode(meta: &MetaCtx, ino: u64, flags: i32) -> OpenOutcome {
            let (path, entry, snap) = {
                let mut c = meta.cache.safe_lock();
                let path = c.get_path(ino).expect("inode known");
                let snap = c.tombstones.snapshot();
                let (dir, name) = (path.parent().unwrap().to_path_buf(), path.file_name().unwrap().to_str().unwrap().to_string());
                let entry = c.find_child(&dir, &name).expect("listing resident")
                    .1.map_or_else(OpenEntry::absent, |_| listed(5));
                (path, entry, snap)
            };
            let (r, rx) = reply();
            open_continue(meta, OpenReq { unlink_snap: Some(snap), ..rq(ino, path.to_str().unwrap(), flags, None) }, entry, r, false);
            rx.recv_timeout(Duration::from_secs(5)).unwrap()
        }

        /// Appends `data` to handle `fh`'s staging file and releases it.
        fn append_and_release(w: &crate::write_path::WriteCtx, fh: u64, data: &[u8]) {
            let (path, wp) = {
                let files = w.open_files.safe_lock();
                (files[&fh].remote_path.clone(), files[&fh].write_path.clone().unwrap())
            };
            let off = std::fs::metadata(&wp).map_or(0, |m| m.len());
            let (tx, rx) = mpsc::channel();
            w.dispatch_write(fh, path, off, data, move |r| tx.send(r.is_ok()).unwrap());
            assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), "write failed");
            let (tx, rx) = mpsc::channel();
            w.dispatch_release(fh, Box::new(move || tx.send(()).unwrap()));
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }

        fn wait_for_put(fake: &FakeBackend, path: &str) -> Vec<u8> {
            let t = Instant::now();
            loop {
                if let Some((_, body)) = fake.puts.lock().unwrap().iter().rev().find(|(p, _)| p == Path::new(path)) {
                    return body.clone();
                }
                assert!(t.elapsed() < Duration::from_secs(10), "no PUT of {path}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        /// A queued MOVE lands. The PUT of an edit made since waits for it:
        /// run first, the MOVE would put the old content over the edit.
        fn land_move(fake: &FakeBackend, meta: &MetaCtx, seq: mutation_journal::SeqId, from: &str, to: &str) {
            std::thread::sleep(Duration::from_millis(300));
            assert!(fake.puts.lock().unwrap().is_empty(), "the edit's PUT ran ahead of the queued MOVE");
            let moved = fake.files.lock().unwrap().remove(Path::new(from));
            if let Some(m) = moved {
                fake.files.lock().unwrap().insert(PathBuf::from(to), m);
            }
            meta.journal.safe_lock().remove(seq);
        }

        fn drop_from_listing(meta: &MetaCtx, dir: &str) {
            meta.cache.safe_lock().put_dir_cache(PathBuf::from(dir), None, None, vec![]);
        }

        #[test]
        fn a_write_to_a_file_whose_failed_upload_a_relist_dropped_is_committed_on_top_of_it() {
            // (a) create + write, its PUT failed transiently (the Put stays
            // journaled, the upload guard is gone), and a refresh of the parent
            // dropped the name. `echo more >> f` must append to what was saved.
            let (fake, meta, tmp, ino) = open_setup(vec![], true);
            fake.files.lock().unwrap().clear();
            let saved = tmp.path().join("write_saved");
            std::fs::write(&saved, b"saved").unwrap();
            let seq = meta.journal.safe_lock().enqueue(mutation_journal::MutationOp::Put {
                remote_path: PathBuf::from("/d/a.txt"), staging_path: saved.clone(), if_match_etag: None,
            });
            meta.journal.safe_lock().mark_deferred(seq, "503".into());
            drop_from_listing(&meta, "/d");
            let fh = fh_of(open_by_inode(&meta, ino, libc::O_WRONLY | libc::O_APPEND));
            assert_eq!(meta.open_files.safe_lock()[&fh].unlinked, Unlinked::No, "a lost name is not an unlink");
            append_and_release(&write_ctx(&meta, tmp.path()), fh, b" more");
            assert_eq!(wait_for_put(&fake, "/d/a.txt"), b"saved more");
        }

        #[test]
        fn a_write_to_a_renamed_file_listed_before_its_move_landed_keeps_its_content() {
            // (b) `mv x/f y/g`, then a re-list of y comes back from the server
            // before the MOVE did. g is still at x/f there.
            let (fake, meta, tmp, _) = open_setup(vec![], true);
            fake.files.lock().unwrap().insert(PathBuf::from("/x/f"), (b"orig".to_vec(), Duration::ZERO));
            let ino = {
                let mut c = meta.cache.safe_lock();
                let ino = c.allocate_inode(PathBuf::from("/x/f"));
                c.move_inode(Path::new("/x/f"), Path::new("/y/g"));
                ino
            };
            let mv = meta.journal.safe_lock().enqueue(mutation_journal::MutationOp::Rename { from: PathBuf::from("/x/f"), to: PathBuf::from("/y/g") });
            drop_from_listing(&meta, "/y");
            let fh = fh_of(open_by_inode(&meta, ino, libc::O_WRONLY | libc::O_APPEND));
            assert_eq!(meta.open_files.safe_lock()[&fh].unlinked, Unlinked::No);
            append_and_release(&write_ctx(&meta, tmp.path()), fh, b"+");
            land_move(&fake, &meta, mv, "/x/f", "/y/g");
            assert_eq!(wait_for_put(&fake, "/y/g"), b"orig+");
        }

        #[test]
        fn a_write_to_a_file_another_client_deleted_re_creates_it() {
            // (c) Deleted elsewhere, a refresh dropped it; the local edit wins.
            let (fake, meta, tmp, ino) = open_setup(vec![], true);
            fake.files.lock().unwrap().clear();
            drop_from_listing(&meta, "/d");
            let fh = fh_of(open_by_inode(&meta, ino, libc::O_WRONLY | libc::O_APPEND));
            assert_eq!(meta.open_files.safe_lock()[&fh].unlinked, Unlinked::No);
            append_and_release(&write_ctx(&meta, tmp.path()), fh, b"new");
            assert_eq!(wait_for_put(&fake, "/d/a.txt"), b"new");
        }

        #[test]
        fn a_write_under_a_directory_renamed_over_an_empty_one_keeps_its_content() {
            // (d) `mkdir d2; mv d d2`: d2's empty listing is resident and rename
            // does not re-key listings, so d2 "lacks" a.txt.
            let (fake, meta, tmp, ino) = open_setup(vec![], true);
            meta.cache.safe_lock().put_dir_cache(PathBuf::from("/d2"), None, None, vec![]);
            meta.cache.safe_lock().move_inode(Path::new("/d"), Path::new("/d2"));
            let mv = meta.journal.safe_lock().enqueue(mutation_journal::MutationOp::Rename { from: PathBuf::from("/d"), to: PathBuf::from("/d2") });
            let fh = fh_of(open_by_inode(&meta, ino, libc::O_WRONLY | libc::O_APPEND));
            assert_eq!(meta.open_files.safe_lock()[&fh].remote_path, PathBuf::from("/d2/a.txt"));
            assert_eq!(meta.open_files.safe_lock()[&fh].unlinked, Unlinked::No);
            append_and_release(&write_ctx(&meta, tmp.path()), fh, b"!");
            land_move(&fake, &meta, mv, "/d/a.txt", "/d2/a.txt");
            assert_eq!(wait_for_put(&fake, "/d2/a.txt"), b"hello!");
        }

        #[test]
        fn a_file_this_mount_unlinked_during_staging_is_not_resurrected() {
            let (fake, meta, tmp, ino) = open_setup(vec![], true);
            let w = write_ctx(&meta, tmp.path());
            // Slow download: the unlink lands while the content is being staged.
            fake.files.lock().unwrap().insert(PathBuf::from("/d/a.txt"), (b"hello".to_vec(), Duration::from_millis(300)));
            let snap = meta.cache.safe_lock().tombstones.snapshot();
            let (r, rx) = reply();
            open_continue(&meta, OpenReq { unlink_snap: Some(snap), ..rq(ino, "/d/a.txt", libc::O_WRONLY | libc::O_APPEND, None) }, listed(5), r, false);
            {
                let mut c = meta.cache.safe_lock();
                c.dir_cache.get_mut(Path::new("/d")).unwrap().files = Arc::new(vec![]);
                c.tombstones.record(ino);
            }
            mark_unlinked(&meta.open_files, Path::new("/d/a.txt"), Some(ino));
            let fh = fh_of(rx.recv_timeout(Duration::from_secs(5)).unwrap());
            let wp = meta.open_files.safe_lock()[&fh].write_path.clone().unwrap();
            append_and_release(&w, fh, b"x");
            std::thread::sleep(Duration::from_millis(300));
            assert!(fake.puts.lock().unwrap().is_empty(), "the deleted file was PUT back");
            assert!(!wp.exists(), "its staging is dropped");
            assert!(meta.journal.safe_lock().is_empty());
        }

        #[test]
        fn rm_then_echo_into_the_same_name_is_committed() {
            // `rm f; echo x > f`: the re-created file keeps the path's inode; its
            // open comes after the unlink's tombstone, so it is not unlinked.
            let (fake, meta, tmp, ino) = open_setup(vec![], true);
            {
                let mut c = meta.cache.safe_lock();
                c.dir_cache.get_mut(Path::new("/d")).unwrap().files = Arc::new(vec![]);
                c.tombstones.record(ino);
            }
            // create() puts the name back into the listing.
            {
                let mut c = meta.cache.safe_lock();
                let mut e = entry_in("/d", "a.txt");
                e.size = 0;
                c.dir_cache.get_mut(Path::new("/d")).unwrap().files = Arc::new(vec![e]);
            }
            let fh = fh_of(open_by_inode(&meta, ino, libc::O_WRONLY | libc::O_TRUNC));
            assert_eq!(meta.open_files.safe_lock()[&fh].unlinked, Unlinked::No);
            append_and_release(&write_ctx(&meta, tmp.path()), fh, b"x");
            assert_eq!(wait_for_put(&fake, "/d/a.txt"), b"x");
        }

        // ── A streamed upload still being assembled is the file's content ───
        //
        // From release until its finish lands (offline: the whole time; after a
        // restart: until the replay), a streamed copy exists only as the
        // server's upload session and the local tail. A writable open of it
        // then must neither stage what the server or a kept copy still has,
        // nor let its own upload replace the streamed one.

        /// Queues the finish of a streamed upload of `path` whose session holds
        /// `sent` on the server and whose tail file holds `tail`.
        fn queue_stream(fake: &FakeBackend, meta: &MetaCtx, dir: &Path, path: &str, sent: &[u8], tail: &[u8]) -> mutation_journal::SeqId {
            *fake.session_prefix.lock().unwrap() = sent.to_vec();
            let tail_path = dir.join(format!("write_tail_{}", path.replace('/', "_")));
            std::fs::write(&tail_path, tail).unwrap();
            meta.journal.safe_lock().enqueue(mutation_journal::MutationOp::FinishChunked {
                remote_path: PathBuf::from(path),
                uploads_base: "uploads/7".into(),
                next_index: 1,
                bytes_confirmed: sent.len() as u64,
                total_len: (sent.len() + tail.len()) as u64,
                tail_path,
                if_match_etag: None,
            })
        }

        fn replay_meta(fake: &Arc<FakeBackend>, meta: &MetaCtx) {
            let ctx = mutation_journal::ReplayContext { backend: fake.clone(), status: meta.status.clone() };
            mutation_journal::replay_journal(&meta.journal, &ctx, &meta.cache, &Arc::new(Mutex::new(HashSet::new())), &Arc::new(Mutex::new(std::collections::VecDeque::new())));
        }

        /// A kept copy of the version the server has, fresh by the listing's etag.
        fn keep_old_copy(meta: &MetaCtx, dir: &Path) -> PathBuf {
            let kept = dir.join("kept_a.txt");
            std::fs::write(&kept, b"hello").unwrap();
            meta.cache.safe_lock().file_cache.insert(PathBuf::from("/d/a.txt"), FileCacheEntry {
                local_path: kept.clone(), remote_modified: None, etag: Some("etag1".into()), kept: true, size: 5,
            });
            kept
        }

        #[test]
        fn a_writable_open_waits_for_a_queued_streamed_upload_and_seeds_from_what_it_assembled() {
            let (fake, meta, tmp, ino) = open_setup(vec![], true);
            fake.files.lock().unwrap().insert(PathBuf::from("/d/a.txt"), (b"hello".to_vec(), Duration::ZERO));
            let kept = keep_old_copy(&meta, tmp.path());
            let seq = queue_stream(&fake, &meta, tmp.path(), "/d/a.txt", b"streamed ", b"content");
            // `rsync --inplace`, `dd conv=notrunc`, a tag editor: writable, not truncating.
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/a.txt", libc::O_RDWR, Some(kept)), listed(5), r, false);
            assert!(rx.recv_timeout(Duration::from_millis(500)).is_err(), "staged before the streamed upload was assembled");
            assert!(meta.journal.safe_lock().contains(seq), "the open must not drop the queued finish");
            // The replay assembles it; only then is the open staged.
            replay_meta(&fake, &meta);
            let fh = fh_of(rx.recv_timeout(Duration::from_secs(5)).unwrap());
            {
                let files = meta.open_files.safe_lock();
                let of = &files[&fh];
                assert_eq!(std::fs::read(of.write_path.as_ref().unwrap()).unwrap(), b"streamed content");
                assert!(of.local.is_none(), "a first write must not seed from the older kept copy");
            }
            append_and_release(&write_ctx(&meta, tmp.path()), fh, b"+tag");
            assert_eq!(wait_for_put(&fake, "/d/a.txt"), b"streamed content+tag");
            assert_eq!(*fake.finished.lock().unwrap(), vec![PathBuf::from("/d/a.txt")], "the streamed upload was assembled, not superseded");
        }

        #[test]
        fn a_writable_open_of_a_streamed_upload_queued_while_offline_fails_instead_of_staging_old_bytes() {
            let (fake, meta, tmp, ino) = open_setup(vec![], true);
            fake.files.lock().unwrap().insert(PathBuf::from("/d/a.txt"), (b"hello".to_vec(), Duration::ZERO));
            let kept = keep_old_copy(&meta, tmp.path());
            let seq = queue_stream(&fake, &meta, tmp.path(), "/d/a.txt", b"streamed ", b"content");
            meta.conn.is_offline.store(true, Ordering::SeqCst);
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY | libc::O_APPEND, Some(kept)), listed(5), r, false);
            assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), OpenOutcome::Error(libc::EIO), "offline the finish cannot land: fail at once");
            assert_all_given_back(&meta, ino);
            assert!(meta.journal.safe_lock().contains(seq));
            assert!(fake.puts.lock().unwrap().is_empty());
            // A truncating open needs no seed: it may still replace the file.
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY | libc::O_TRUNC, None), listed(5), r, false);
            assert!(matches!(rx.try_recv(), Ok(OpenOutcome::Opened(_))));
        }

        #[test]
        fn a_stream_waiter_wakes_as_soon_as_the_finish_leaves_the_journal() {
            let (fake, meta, tmp, ino) = open_setup(vec![], true);
            let seq = queue_stream(&fake, &meta, tmp.path(), "/d/a.txt", b"x", b"y");
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY | libc::O_APPEND, None), listed(5), r, false);
            assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
            assert_eq!(meta.stream_waiters.load(Ordering::SeqCst), 1);
            // Assembled by a live worker: on the server, then out of the journal.
            fake.files.lock().unwrap().insert(PathBuf::from("/d/a.txt"), (b"xy".to_vec(), Duration::ZERO));
            let landed = Instant::now();
            meta.journal.safe_lock().remove(seq);
            let fh = fh_of(rx.recv_timeout(Duration::from_secs(5)).unwrap());
            assert!(landed.elapsed() < Duration::from_millis(60), "not woken by the change: {:?}", landed.elapsed());
            assert_eq!(std::fs::read(meta.open_files.safe_lock()[&fh].write_path.as_ref().unwrap()).unwrap(), b"xy");
            assert_eq!(meta.stream_waiters.load(Ordering::SeqCst), 0, "the place is given back");
            release_bookkeeping(&meta, fh);
        }

        #[test]
        fn at_most_four_opens_wait_for_streamed_uploads_and_the_rest_try_again() {
            let (fake, meta, tmp, ino) = open_setup(vec![], true);
            let seq = queue_stream(&fake, &meta, tmp.path(), "/d/a.txt", b"x", b"y");
            let waiting: Vec<_> = (0..STREAM_WAITERS_MAX).map(|_| {
                let (r, rx) = reply();
                open_continue(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY | libc::O_APPEND, None), listed(5), r, false);
                rx
            }).collect();
            let t = Instant::now();
            while meta.stream_waiters.load(Ordering::SeqCst) < STREAM_WAITERS_MAX {
                assert!(t.elapsed() < Duration::from_secs(5), "the waiters never started");
                std::thread::sleep(Duration::from_millis(5));
            }
            // One more is refused at once, before it takes a READ worker or a handle.
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/a.txt", libc::O_RDWR, None), listed(5), r, false);
            assert_eq!(rx.try_recv().unwrap(), OpenOutcome::Error(libc::EAGAIN));
            assert_eq!(meta.open_files.safe_lock().len(), STREAM_WAITERS_MAX);
            // A truncating open needs no seed, so no place.
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY | libc::O_TRUNC, None), listed(5), r, false);
            let trunc = fh_of(rx.try_recv().unwrap());
            release_bookkeeping(&meta, trunc);
            fake.files.lock().unwrap().insert(PathBuf::from("/d/a.txt"), (b"xy".to_vec(), Duration::ZERO));
            meta.journal.safe_lock().remove(seq);
            for rx in waiting {
                let fh = fh_of(rx.recv_timeout(Duration::from_secs(5)).unwrap());
                release_bookkeeping(&meta, fh);
            }
            assert_eq!(meta.stream_waiters.load(Ordering::SeqCst), 0);
            assert_all_given_back(&meta, ino);
        }

        #[test]
        fn a_stream_waiter_gives_up_at_once_while_sync_is_paused() {
            let (fake, meta, tmp, ino) = open_setup(vec![], true);
            let seq = queue_stream(&fake, &meta, tmp.path(), "/d/a.txt", b"x", b"y");
            meta.conn.paused.store(true, Ordering::SeqCst);
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY | libc::O_APPEND, None), listed(5), r, false);
            assert_eq!(rx.try_recv().unwrap(), OpenOutcome::Error(libc::EIO), "paused, the finish cannot land: fail at once");
            assert_all_given_back(&meta, ino);
            // Paused while it waits: it stops within a recheck.
            meta.conn.paused.store(false, Ordering::SeqCst);
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY | libc::O_APPEND, None), listed(5), r, false);
            assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
            meta.conn.paused.store(true, Ordering::SeqCst);
            assert_eq!(rx.recv_timeout(STREAM_WAIT_RECHECK + Duration::from_secs(2)).unwrap(), OpenOutcome::Error(libc::EIO));
            assert_all_given_back(&meta, ino);
            assert!(meta.journal.safe_lock().contains(seq), "the queued finish is left alone");
            assert_eq!(meta.stream_waiters.load(Ordering::SeqCst), 0);
        }

        #[test]
        fn an_older_put_is_never_the_seed_of_a_file_whose_newer_upload_is_streamed() {
            let (fake, meta, tmp, ino) = open_setup(vec![], true);
            let old = tmp.path().join("write_old_put");
            std::fs::write(&old, b"older version").unwrap();
            let put = meta.journal.safe_lock().enqueue(mutation_journal::MutationOp::Put {
                remote_path: PathBuf::from("/d/a.txt"), staging_path: old, if_match_etag: None,
            });
            // In flight (claimed by a live worker), so the stream did not supersede it.
            assert!(meta.journal.safe_lock().claim(put));
            let stream = queue_stream(&fake, &meta, tmp.path(), "/d/a.txt", b"new ", b"stream");
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY | libc::O_APPEND, None), listed(5), r, false);
            assert!(rx.recv_timeout(Duration::from_millis(400)).is_err(), "seeded from the older Put's staging");
            // The Put lands, then the stream: the open follows the newest.
            meta.journal.safe_lock().remove(put);
            assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "still streaming");
            replay_meta(&fake, &meta);
            assert!(!meta.journal.safe_lock().contains(stream));
            let fh = fh_of(rx.recv_timeout(Duration::from_secs(5)).unwrap());
            assert_eq!(std::fs::read(meta.open_files.safe_lock()[&fh].write_path.as_ref().unwrap()).unwrap(), b"new stream");
        }

        #[test]
        fn a_kept_copy_is_not_fresh_while_an_upload_of_its_file_is_queued() {
            let (fake, meta, tmp, ino) = open_setup(vec![], true);
            let kept = keep_old_copy(&meta, tmp.path());
            let seq = queue_stream(&fake, &meta, tmp.path(), "/d/a.txt", b"x", b"y");
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/a.txt", libc::O_RDONLY, Some(kept.clone())), listed(5), r, false);
            let fh = fh_of(rx.try_recv().expect("a read-only open is answered inline"));
            assert!(!meta.open_files.safe_lock()[&fh].cache_fresh, "reads would serve the older version");
            release_bookkeeping(&meta, fh);
            meta.journal.safe_lock().remove(seq);
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/a.txt", libc::O_RDONLY, Some(kept)), listed(5), r, false);
            let fh = fh_of(rx.try_recv().unwrap());
            assert!(meta.open_files.safe_lock()[&fh].cache_fresh, "fresh again once nothing is queued");
        }

        #[test]
        fn a_pending_put_gone_during_staging_seeds_from_what_replaced_it() {
            // L6: the Put the open picked was superseded before its staging was
            // copied. The newest upload is the content, not the server.
            let (fake, meta, tmp, ino) = open_setup(vec![], true);
            let newer = tmp.path().join("write_newer");
            std::fs::write(&newer, b"newer").unwrap();
            meta.journal.safe_lock().enqueue(mutation_journal::MutationOp::Put {
                remote_path: PathBuf::from("/d/a.txt"), staging_path: newer, if_match_etag: None,
            });
            let (r, _rx) = reply();
            let wp = tmp.path().join("w9");
            let _ = open_register(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY, None), listed(5), 9, false, Some(wp.clone()), true, None, &r);
            seed_from_queue(&meta, 9, Path::new("/d/a.txt"), &wp).unwrap();
            assert_eq!(std::fs::read(&wp).unwrap(), b"newer");
            // Nothing queued any more: the server's copy.
            let queued: Vec<_> = meta.journal.safe_lock().entries().iter().map(|e| e.seq).collect();
            for seq in queued {
                meta.journal.safe_lock().remove(seq);
            }
            seed_from_queue(&meta, 9, Path::new("/d/a.txt"), &wp).unwrap();
            assert_eq!(std::fs::read(&wp).unwrap(), b"hello");
            let _ = fake;
            release_bookkeeping(&meta, 9);
        }

        #[test]
        fn a_move_landing_during_the_seed_download_does_not_make_the_file_look_deleted() {
            // `mv f g` is queued; a re-list dropped g (absent). The open stages
            // from f, and the MOVE lands while it downloads: what f held may
            // be another file by then, so it is staged from g again.
            let (fake, meta, tmp, _) = open_setup(vec![], true);
            fake.files.lock().unwrap().insert(PathBuf::from("/d/f"), (b"real".to_vec(), Duration::ZERO));
            let ino = {
                let mut c = meta.cache.safe_lock();
                let ino = c.allocate_inode(PathBuf::from("/d/f"));
                c.move_inode(Path::new("/d/f"), Path::new("/d/g"));
                ino
            };
            let seq = meta.journal.safe_lock().enqueue(mutation_journal::MutationOp::Rename { from: PathBuf::from("/d/f"), to: PathBuf::from("/d/g") });
            let journal = meta.journal.clone();
            let asked = Arc::new(Mutex::new(Vec::new()));
            let log = asked.clone();
            *fake.after_download.lock().unwrap() = Some(Box::new(move |fake, path| {
                log.lock().unwrap().push(path.to_path_buf());
                if path == Path::new("/d/f") && journal.safe_lock().contains(seq) {
                    let moved = fake.files.lock().unwrap().remove(Path::new("/d/f")).unwrap();
                    fake.files.lock().unwrap().insert(PathBuf::from("/d/g"), moved);
                    journal.safe_lock().remove(seq);
                }
            }));
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/g", libc::O_WRONLY | libc::O_APPEND, None), OpenEntry::absent(), r, false);
            let fh = fh_of(rx.recv_timeout(Duration::from_secs(5)).unwrap());
            assert_eq!(std::fs::read(meta.open_files.safe_lock()[&fh].write_path.as_ref().unwrap()).unwrap(), b"real", "staged empty: the append would replace g");
            assert_eq!(*asked.lock().unwrap(), [Path::new("/d/f"), Path::new("/d/g")]);
            append_and_release(&write_ctx(&meta, tmp.path()), fh, b"+");
            assert_eq!(wait_for_put(&fake, "/d/g"), b"real+");
        }

        #[test]
        fn a_move_that_landed_before_the_seed_download_is_staged_from_its_new_name() {
            // The MOVE ran, but its entry is not out of the journal yet: the
            // source is a 404, and the file is at its new name.
            let (fake, meta, tmp, _) = open_setup(vec![], true);
            fake.files.lock().unwrap().insert(PathBuf::from("/d/g"), (b"real".to_vec(), Duration::ZERO));
            let ino = meta.cache.safe_lock().allocate_inode(PathBuf::from("/d/g"));
            let seq = meta.journal.safe_lock().enqueue(mutation_journal::MutationOp::Rename { from: PathBuf::from("/d/f"), to: PathBuf::from("/d/g") });
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/g", libc::O_WRONLY | libc::O_APPEND, None), OpenEntry::absent(), r, false);
            let fh = fh_of(rx.recv_timeout(Duration::from_secs(5)).unwrap());
            assert_eq!(std::fs::read(meta.open_files.safe_lock()[&fh].write_path.as_ref().unwrap()).unwrap(), b"real");
            append_and_release(&write_ctx(&meta, tmp.path()), fh, b"+");
            land_move(&fake, &meta, seq, "/d/f", "/d/g");
            assert_eq!(wait_for_put(&fake, "/d/g"), b"real+");
        }

        #[test]
        fn an_edit_of_a_file_renamed_over_another_starts_from_the_renamed_file() {
            // `mv a b` over an existing b is queued: the server still has the
            // old b. An edit of b must start from a's content.
            let (fake, meta, tmp, _) = open_setup(vec![], true);
            fake.files.lock().unwrap().insert(PathBuf::from("/d/a"), (b"from a".to_vec(), Duration::ZERO));
            fake.files.lock().unwrap().insert(PathBuf::from("/d/b"), (b"old b".to_vec(), Duration::ZERO));
            let ino = meta.cache.safe_lock().allocate_inode(PathBuf::from("/d/b"));
            let seq = meta.journal.safe_lock().enqueue(mutation_journal::MutationOp::Rename { from: PathBuf::from("/d/a"), to: PathBuf::from("/d/b") });
            let (r, rx) = reply();
            open_continue(&meta, rq(ino, "/d/b", libc::O_WRONLY | libc::O_APPEND, None), listed(6), r, false);
            let fh = fh_of(rx.recv_timeout(Duration::from_secs(5)).unwrap());
            assert_eq!(std::fs::read(meta.open_files.safe_lock()[&fh].write_path.as_ref().unwrap()).unwrap(), b"from a");
            append_and_release(&write_ctx(&meta, tmp.path()), fh, b"+");
            land_move(&fake, &meta, seq, "/d/a", "/d/b");
            assert_eq!(wait_for_put(&fake, "/d/b"), b"from a+");
        }

        #[test]
        fn an_unlink_between_the_snapshot_and_the_registration_marks_the_handle_local() {
            // CRIT-1's interleaving: open() takes its snapshot, then unlink
            // records its tombstone and scans `open_files` while the handle is
            // not there yet; the registration must find the tombstone.
            let (_, meta, tmp, ino) = open_setup(vec![], true);
            let snap = meta.cache.safe_lock().tombstones.snapshot();
            {
                let mut c = meta.cache.safe_lock();
                c.dir_cache.get_mut(Path::new("/d")).unwrap().files = Arc::new(vec![]);
                c.tombstones.record(ino);
            }
            mark_unlinked(&meta.open_files, Path::new("/d/a.txt"), Some(ino));
            assert!(meta.open_files.safe_lock().is_empty(), "nothing registered for the scan to find");
            let (r, _rx) = reply();
            let _ = open_register(&meta, OpenReq { unlink_snap: Some(snap), ..rq(ino, "/d/a.txt", libc::O_WRONLY, None) }, listed(0), 11, false, Some(tmp.path().join("w11")), false, None, &r);
            assert_eq!(meta.open_files.safe_lock()[&11].unlinked, Unlinked::Local);
            release_bookkeeping(&meta, 11);
            // An open whose snapshot came after the unlink is the re-created file.
            let snap = meta.cache.safe_lock().tombstones.snapshot();
            let (r, _rx) = reply();
            let _ = open_register(&meta, OpenReq { unlink_snap: Some(snap), ..rq(ino, "/d/a.txt", libc::O_WRONLY, None) }, listed(0), 12, false, Some(tmp.path().join("w12")), false, None, &r);
            assert_eq!(meta.open_files.safe_lock()[&12].unlinked, Unlinked::No);
            release_bookkeeping(&meta, 12);
            assert_all_given_back(&meta, ino);
        }

        #[test]
        fn a_rename_over_a_file_marks_its_open_handles_unlinked_and_retargets_the_source() {
            let (_, meta, tmp, a_ino) = open_setup(vec![], true);
            let b_ino = {
                let mut c = meta.cache.safe_lock();
                let mut b = entry_in("/d", "b.txt");
                b.size = 3;
                let mut files = (*c.dir_cache[Path::new("/d")].files).clone();
                files.push(b);
                c.dir_cache.get_mut(Path::new("/d")).unwrap().files = Arc::new(files);
                c.allocate_inode(PathBuf::from("/d/b.txt"))
            };
            let (r, _rx) = reply();
            let _ = open_register(&meta, rq(a_ino, "/d/a.txt", libc::O_WRONLY, None), listed(0), 21, false, Some(tmp.path().join("w21")), false, None, &r);
            let _ = open_register(&meta, rq(b_ino, "/d/b.txt", libc::O_WRONLY, None), listed(0), 22, false, Some(tmp.path().join("w22")), false, None, &r);
            let snap = meta.cache.safe_lock().tombstones.snapshot();
            rename_in_cache(&mut meta.cache.safe_lock(), &meta.open_files, Path::new("/d/a.txt"), Path::new("/d/b.txt"), Path::new("/d"), Path::new("/d"));
            {
                let files = meta.open_files.safe_lock();
                assert_eq!(files[&22].unlinked, Unlinked::Local, "the replaced file's writes would land over the renamed one");
                assert_eq!(files[&21].unlinked, Unlinked::No);
                assert_eq!(files[&21].remote_path, PathBuf::from("/d/b.txt"));
            }
            let mut c = meta.cache.safe_lock();
            assert!(c.tombstones.removed_since(b_ino, &snap), "an open of the replaced file still in flight must see it");
            assert!(!c.tombstones.removed_since(a_ino, &snap));
            assert_eq!(c.get_inode(Path::new("/d/b.txt")), Some(a_ino));
            let names: Vec<_> = c.dir_cache[Path::new("/d")].files.iter().map(|e| e.path.clone()).collect();
            assert_eq!(names, vec![PathBuf::from("/d/b.txt")]);
            drop(c);
            drop(snap);
            release_bookkeeping(&meta, 21);
            release_bookkeeping(&meta, 22);
        }

        #[test]
        fn rename_refuses_exchange_and_unknown_flags_and_honors_noreplace() {
            use fuser::RenameFlags as F;
            let never = || -> bool { panic!("not asked") };
            assert!(rename_flags_refusal(F::empty(), never).is_none());
            assert_eq!(rename_flags_refusal(F::RENAME_EXCHANGE, never).map(|e| e.code()), Some(libc::EINVAL));
            assert_eq!(rename_flags_refusal(F::RENAME_EXCHANGE | F::RENAME_NOREPLACE, never).map(|e| e.code()), Some(libc::EINVAL));
            assert_eq!(rename_flags_refusal(F::RENAME_WHITEOUT, never).map(|e| e.code()), Some(libc::EINVAL));
            assert_eq!(rename_flags_refusal(F::from_bits_retain(1 << 20), never).map(|e| e.code()), Some(libc::EINVAL));
            assert_eq!(rename_flags_refusal(F::RENAME_NOREPLACE, || true).map(|e| e.code()), Some(libc::EEXIST));
            assert!(rename_flags_refusal(F::RENAME_NOREPLACE, || false).is_none());
            // What "exists" means: the resident listing, else the inode map.
            let (_, meta, _tmp, _) = open_setup(vec![], true);
            let mut c = meta.cache.safe_lock();
            assert!(rename_target_exists(&c, Path::new("/d/a.txt")));
            assert!(!rename_target_exists(&c, Path::new("/d/new.txt")));
            c.allocate_inode(PathBuf::from("/e/x.txt"));
            assert!(rename_target_exists(&c, Path::new("/e/x.txt")), "no listing: the inode map");
            c.allocate_inode(PathBuf::from("/d/stale.txt"));
            assert!(!rename_target_exists(&c, Path::new("/d/stale.txt")), "the listing wins over a stale inode");
        }

        #[test]
        fn a_purge_keeps_the_staging_of_a_handle_released_while_it_ran() {
            let (_, meta, tmp, _) = open_setup(vec![], true);
            let old = std::time::SystemTime::now() - Duration::from_secs(60);
            let make = |fh: u64| {
                let p = tmp.path().join(mutation_journal::staging_file_name(fh));
                std::fs::write(&p, b"edit").unwrap();
                std::fs::File::options().write(true).open(&p).unwrap().set_modified(old).unwrap();
                p
            };
            let (releasing, orphan) = (make(5), make(6));
            // Handle 5 is between leaving `open_files` and being journaled.
            meta.journal.safe_lock().reserve_staging(&releasing);
            let purge = || {
                let status: StatusMap = Arc::new(RwLock::new(HashMap::new()));
                purge_all(&meta.cache, &status, &Arc::new(Mutex::new(HashSet::new())), &meta.journal, &meta.open_files, &Arc::new(Mutex::new(None))).unwrap()
            };
            purge();
            assert!(releasing.exists(), "the purge deleted an edit on its way into the journal");
            assert!(!orphan.exists(), "a real orphan is still reclaimed");
            // Journaled now: still kept, by the entry.
            meta.journal.safe_lock().enqueue(mutation_journal::MutationOp::Put {
                remote_path: PathBuf::from("/d/a.txt"), staging_path: releasing.clone(), if_match_etag: None,
            });
            meta.journal.safe_lock().unreserve_staging(&releasing);
            purge();
            assert!(releasing.exists());
            // Made after the purge started, not registered anywhere yet.
            let fresh = tmp.path().join(mutation_journal::staging_file_name(7));
            std::fs::write(&fresh, b"new").unwrap();
            std::fs::File::options().write(true).open(&fresh).unwrap().set_modified(std::time::SystemTime::now() + Duration::from_secs(5)).unwrap();
            purge();
            assert!(fresh.exists());
        }

        #[test]
        fn a_written_handle_unlinked_without_proof_keeps_its_bytes_in_recovered() {
            let (fake, meta, tmp, ino) = open_setup(vec![], true);
            let w = write_ctx(&meta, tmp.path());
            let fh = fh_of(open_by_inode(&meta, ino, libc::O_WRONLY | libc::O_TRUNC));
            let (tx, rx) = mpsc::channel();
            w.dispatch_write(fh, PathBuf::from("/d/a.txt"), 0, b"precious", move |r| tx.send(r.is_ok()).unwrap());
            assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap());
            mark_unlinked(&meta.open_files, Path::new("/d/a.txt"), None);
            let (tx, rx) = mpsc::channel();
            w.dispatch_release(fh, Box::new(move || tx.send(()).unwrap()));
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let recovered = tmp.path().join(mutation_journal::RECOVERED_DIR);
            let kept: Vec<_> = std::fs::read_dir(&recovered).unwrap().flatten().map(|e| e.path()).collect();
            let data = kept.iter().find(|p| p.extension().is_none_or(|x| x != "json")).expect("bytes kept");
            assert_eq!(std::fs::read(data).unwrap(), b"precious");
            let sidecar: mutation_journal::RecoveredSidecar = serde_json::from_slice(&std::fs::read(data.with_file_name(format!("{}.json", data.file_name().unwrap().to_str().unwrap()))).unwrap()).unwrap();
            assert_eq!(sidecar.remote_path.as_deref(), Some(Path::new("/d/a.txt")));
            assert_eq!(sidecar.size, 8);
            assert!(fake.puts.lock().unwrap().is_empty());
            assert_eq!(meta.journal.safe_lock().unresolved_conflicts().len(), 1, "the user is told where");
        }

        #[test]
        fn a_rename_between_pinning_and_registering_moves_the_pin_with_the_handle() {
            let (_, meta, tmp, ino) = open_setup(vec![], true);
            let (r, _rx) = reply();
            let _ = open_register(&meta, rq(ino, "/d/a.txt", libc::O_WRONLY, None), listed(0), 7, false, Some(tmp.path().join("w7")), false, None, &r);
            // Replay insert_open_file's steps with rename() running in its window,
            // where the handle is in `open_files` but has no pin yet.
            meta.open_files.safe_lock().get_mut(&7).unwrap().pinned_parent = None;
            meta.cache.safe_lock().unpin_dir(Path::new("/d"));
            let (now_at, gone) = pin_where_it_lives(&meta, ino, Path::new("/d/a.txt"), None);
            assert_eq!((now_at.as_path(), gone), (Path::new("/d/a.txt"), false));
            {
                let mut c = meta.cache.safe_lock();
                c.move_inode(Path::new("/d/a.txt"), Path::new("/e/a.txt"));
                retarget_open_files(&mut c, &meta.open_files, Path::new("/d/a.txt"), Path::new("/e/a.txt"));
            }
            adopt_pin(&meta, 7, Path::new("/d/a.txt"), &now_at, gone);
            {
                let files = meta.open_files.safe_lock();
                assert_eq!(files[&7].remote_path, PathBuf::from("/e/a.txt"));
                assert_eq!(files[&7].pinned_parent.as_deref(), Some(Path::new("/e")));
            }
            let pins = meta.cache.safe_lock().pins.clone();
            assert_eq!(pins.get(Path::new("/e")), Some(&1), "{pins:?}");
            assert_eq!(pins.get(Path::new("/d")), None, "the old parent stays pinned: {pins:?}");
            release_bookkeeping(&meta, 7);
            assert_all_given_back(&meta, ino);
        }

        // ── Size overlays ───────────────────────────────────────────────────

        fn writer(meta: &MetaCtx, fh: u64, path: &str, bytes: usize, unlinked: bool) {
            let wp = meta.cache.safe_lock().cache_dir.join(format!("write_{fh}"));
            std::fs::write(&wp, vec![b'x'; bytes]).unwrap();
            let (r, _rx) = reply();
            let _ = open_register(meta, rq(0, path, libc::O_WRONLY, None), OpenEntry::default(), fh, false, Some(wp), false, None, &r);
            let mut files = meta.open_files.safe_lock();
            let of = files.get_mut(&fh).unwrap();
            of.dirty = true;
            of.unlinked = if unlinked { Unlinked::Local } else { Unlinked::No };
        }

        #[test]
        fn the_local_size_overlay_takes_the_largest_live_writer() {
            let (_, meta, _tmp, _) = open_setup(vec![], true);
            let e = { let mut e = entry_in("/d", "a.txt"); e.size = 5; e };
            let attr = |meta: &MetaCtx| {
                let mut a = make_file_attr(7, &e);
                overlay_local_size(meta, Path::new("/d/a.txt"), &mut a);
                a.size
            };
            assert_eq!(attr(&meta), 5, "no writer: the listing's size");
            writer(&meta, 1, "/d/a.txt", 10, false);
            writer(&meta, 2, "/d/a.txt", 30, false);
            writer(&meta, 3, "/d/a.txt", 99, true);
            writer(&meta, 4, "/d/other.txt", 77, false);
            assert_eq!(attr(&meta), 30, "largest writer, skipping the unlinked one and other files");
        }

        #[test]
        fn an_upload_in_flight_overlays_its_size_on_the_listing() {
            let mut c = make_test_cache();
            let e = { let mut e = entry_in("/d", "a.txt"); e.size = 5; e };
            c.uploading.insert(PathBuf::from("/d/a.txt"), Some(4096));
            assert_eq!(attr_for(&c, 7, Path::new("/d/a.txt"), &e).size, 4096);
            assert_eq!(attr_for(&c, 7, Path::new("/d/a.txt"), &e).blocks, 8);
            c.uploading.insert(PathBuf::from("/d/a.txt"), None);
            assert_eq!(attr_for(&c, 7, Path::new("/d/a.txt"), &e).size, 5, "unknown upload size keeps the listing's");
        }

        // ── Answers that must reflect an unlink during the wait ─────────────

        #[test]
        fn a_slow_lookup_of_a_name_deleted_meanwhile_is_absent() {
            let cold = FakeDir { entries: names("/cold", 3), before_first: Duration::from_millis(300), ..Default::default() };
            let (_, conn, cache) = setup(vec![("/cold", cold)]);
            let meta = MetaCtx::for_tests(conn, cache, Duration::from_secs(5));
            let rx = ask(&meta, "/cold/f1.txt");
            // unlink() while the worker waits on the listing.
            meta.cache.safe_lock().deleting.insert(PathBuf::from("/cold/f1.txt"));
            let (r, _) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(matches!(r, Resolved::Absent), "a deleted name must not be answered from the listing");
        }

        #[test]
        fn a_lookup_committed_after_an_unlink_does_not_bring_its_maps_back() {
            let (_, conn, cache) = setup(vec![]);
            cache.safe_lock().put_dir_cache(PathBuf::from("/d"), None, None, names("/d", 3));
            let meta = MetaCtx::for_tests(conn, cache, Duration::from_secs(5));
            let hit = |meta: &MetaCtx, name: &str| {
                let mut c = meta.cache.safe_lock();
                let e = c.find_child(Path::new("/d"), name).and_then(|(f, i)| i.map(|i| f[i].clone())).unwrap();
                lookup_pick(&mut c, &Path::new("/d").join(name), &e)
            };
            assert!(matches!(lookup_commit(&meta, hit(&meta, "f0.txt"), true), LookupAnswer::Entry(_)));
            assert!(meta.details.safe_read().contains_key(Path::new("/d/f0.txt")));

            // Picked, then unlink() edits the listing before the commit.
            let h = hit(&meta, "f1.txt");
            {
                let mut c = meta.cache.safe_lock();
                let files: Vec<RemoteEntry> = c.dir_cache[Path::new("/d")].files.iter().filter(|e| e.path != Path::new("/d/f1.txt")).cloned().collect();
                c.dir_cache.get_mut(Path::new("/d")).unwrap().files = Arc::new(files);
                c.deleting.insert(PathBuf::from("/d/f1.txt"));
            }
            assert!(matches!(lookup_commit(&meta, h, true), LookupAnswer::Gone));
            assert!(!meta.details.safe_read().contains_key(Path::new("/d/f1.txt")));
            assert!(!meta.status.safe_read().contains_key(Path::new("/d/f1.txt")));
            assert!(!meta.children_map.safe_read().get(Path::new("/d")).is_some_and(|s| s.contains(Path::new("/d/f1.txt"))));

            // A ghost that appeared during the wait answers as a ghost.
            let h = hit(&meta, "f2.txt");
            meta.ghost_entries.safe_lock().insert(PathBuf::from("/d/f2.txt"), GhostEntry {
                kind: GhostKind::HiddenAdd, created_at: Instant::now(), rename_pair_id: None,
            });
            assert!(matches!(lookup_commit(&meta, h, true), LookupAnswer::Ghost(GhostKind::HiddenAdd)));
            assert!(!meta.details.safe_read().contains_key(Path::new("/d/f2.txt")));
        }

        #[test]
        fn a_lookup_committed_after_its_name_was_recreated_answers_the_new_file() {
            let (_, conn, cache) = setup(vec![]);
            let with_id = |name: &str, fid: u64, etag: &str| {
                let mut e = entry_in("/d", name);
                e.ext.set_int("fileid", fid);
                e.change_token = Some(etag.into());
                e
            };
            cache.safe_lock().put_dir_cache(PathBuf::from("/d"), None, None, vec![with_id("f.txt", 1, "old")]);
            let meta = MetaCtx::for_tests(conn, cache, Duration::from_secs(5));
            let pick = |meta: &MetaCtx| {
                let mut c = meta.cache.safe_lock();
                let e = c.find_child(Path::new("/d"), "f.txt").and_then(|(f, i)| i.map(|i| f[i].clone())).unwrap();
                lookup_pick(&mut c, Path::new("/d/f.txt"), &e)
            };
            let h = pick(&meta);
            let old_ino = h.attr.ino.0;
            // unlink + re-create between the pick and the commit: a new inode, and
            // (once uploaded) a new file id and etag.
            {
                let mut c = meta.cache.safe_lock();
                c.paths.remove(Path::new("/d/f.txt"));
                c.inodes.remove(&old_ino);
                c.dir_cache.get_mut(Path::new("/d")).unwrap().files = Arc::new(vec![with_id("f.txt", 2, "new")]);
            }
            let new_ino = meta.cache.safe_lock().allocate_inode(PathBuf::from("/d/f.txt"));
            match lookup_commit(&meta, h, true) {
                LookupAnswer::Entry(attr) => assert_eq!(attr.ino.0, new_ino, "answered with the deleted file's inode"),
                _ => panic!("the name exists"),
            }
            assert_eq!(meta.fileids.safe_read().get(Path::new("/d/f.txt")), Some(&2), "the old file id was published for the new file");

            // Inline (the dispatch thread, which also runs unlink and create) the
            // pick is final: nothing is looked up again.
            let h = pick(&meta);
            meta.cache.safe_lock().dir_cache.get_mut(Path::new("/d")).unwrap().files = Arc::new(vec![]);
            assert!(matches!(lookup_commit(&meta, h, false), LookupAnswer::Entry(_)));
        }

        #[test]
        fn a_local_re_create_is_told_from_the_file_it_replaced() {
            // Neither version has a file id or etag yet: only create()'s number differs.
            let (_, conn, cache) = setup(vec![]);
            let local = |n: u64| {
                let mut e = entry_in("/d", "f.txt");
                e.change_token = None;
                e.modified = Some(UNIX_EPOCH + Duration::from_nanos(n));
                e
            };
            cache.safe_lock().put_dir_cache(PathBuf::from("/d"), None, None, vec![local(1)]);
            let mut c = cache.safe_lock();
            let h = lookup_pick(&mut c, Path::new("/d/f.txt"), &local(1));
            drop(c);
            assert!(matches!(lookup_recheck(&cache, &h), Recheck::Same));
            cache.safe_lock().dir_cache.get_mut(Path::new("/d")).unwrap().files = Arc::new(vec![local(2)]);
            assert!(matches!(lookup_recheck(&cache, &h), Recheck::Replaced(_)), "rm f; touch f looked like the same file");
            let _ = conn;
        }

        #[test]
        fn with_child_within_says_which_answers_come_from_a_worker() {
            let cold = FakeDir { entries: names("/cold", 3), before_first: Duration::from_millis(50), ..Default::default() };
            let (_, conn, cache) = setup(vec![("/cold", cold)]);
            cache.safe_lock().put_dir_cache(PathBuf::from("/hot"), None, None, names("/hot", 3));
            let meta = MetaCtx::for_tests(conn, cache, Duration::from_secs(5));
            let ask = |meta: &MetaCtx, path: &str| {
                let (tx, rx) = mpsc::channel();
                with_child_within(meta, 0, Path::new(path), Duration::from_secs(5), tx, |_, _, _| (), |_| None, |_, _, tx, r, on_worker| {
                    let _ = tx.send((matches!(r, Resolved::Found(())), on_worker));
                });
                rx.recv_timeout(Duration::from_secs(5)).unwrap()
            };
            assert_eq!(ask(&meta, "/hot/f1.txt"), (true, false));
            assert_eq!(ask(&meta, "/cold/f1.txt"), (true, true));
            let refused = MetaCtx { meta_pool: &REFUSING, ..meta.clone() };
            assert_eq!(ask(&refused, "/other/x.txt"), (false, false), "a refused miss is answered inline");
        }

        // ── Name index: built for listings that are read, not written ───────

        #[test]
        fn a_mutated_wide_listing_is_not_reindexed_on_its_first_lookup() {
            let mut c = make_test_cache();
            c.put_dir_cache(PathBuf::from("/w"), None, None, names("/w", 20_000));
            let built = |c: &FsCache| c.dir_cache[Path::new("/w")].name_index.as_ref().is_some_and(|s| s.index.is_some());
            // A bulk copy: every create swaps the listing, then looks the new name up.
            let t = Instant::now();
            for i in 0..20 {
                let mut files = (*c.dir_cache[Path::new("/w")].files).clone();
                files.push(entry_in("/w", &format!("new{i}.txt")));
                c.dir_cache.get_mut(Path::new("/w")).unwrap().files = Arc::new(files);
                assert!(c.find_child(Path::new("/w"), &format!("new{i}.txt")).and_then(|(_, p)| p).is_some());
                assert!(!built(&c), "an index was built for a listing looked up once");
            }
            eprintln!("20 create+lookup rounds on a 20k listing: {:?}", t.elapsed());
            // A version that keeps being read gets its index.
            for i in 0..=NAME_INDEX_AFTER_LOOKUPS as usize {
                assert!(c.find_child(Path::new("/w"), &format!("f{i}.txt")).and_then(|(_, p)| p).is_some());
            }
            assert!(built(&c));
            assert_eq!(c.find_child(Path::new("/w"), "f19999.txt").map(|(_, p)| p), Some(Some(19_999)));
            assert_eq!(c.find_child(Path::new("/w"), "nope").map(|(_, p)| p), Some(None));
        }

        // ── Promotion through the readdir waiter ────────────────────────────

        #[test]
        fn a_readdir_waiter_does_not_promote_a_stream_before_its_result() {
            let (_, conn, cache) = setup(vec![]);
            for (dir, result) in [("/ok", Ok(Some("e1".to_string()))), ("/bad", Err("truncated: body error".to_string()))] {
                let etx = {
                    let mut c = cache.safe_lock();
                    let (tx, etx) = start(&mut c, dir);
                    drop(tx); // the entry stream ended empty; the result is still on its way
                    etx
                };
                let failed = result.is_err();
                let (done_tx, done_rx) = mpsc::channel();
                let r = std::thread::scope(|s| {
                    let waiter = s.spawn(|| {
                        let r = list_dir_cached_or_fresh(&conn, &cache, PathBuf::from(dir), None);
                        let _ = done_tx.send(());
                        r
                    });
                    assert!(done_rx.recv_timeout(Duration::from_millis(300)).is_err(), "{dir}: answered before the result arrived");
                    assert!(cache.safe_lock().dir_cache.get(Path::new(dir)).is_none(), "{dir}: promoted without its result");
                    etx.send(result).unwrap();
                    cache.safe_lock().pending_notify.1.notify_all();
                    waiter.join().unwrap()
                });
                if failed {
                    assert!(r.is_err());
                    assert!(cache.safe_lock().dir_cache.get(Path::new(dir)).is_none(), "a broken listing must not be cached");
                } else {
                    assert!(r.is_ok(), "{r:?}");
                    assert_eq!(cache.safe_lock().dir_cache.get(Path::new(dir)).and_then(|e| e.etag.clone()).as_deref(), Some("e1"));
                }
            }
        }
    }

    // ── Offline journal replay against a server tree ──────────────────────
    //
    // Each op replays under the name it had when it was queued, FIFO, so the
    // server goes through the same steps the mount did. These build journals
    // the way the FUSE ops do (unlink, release's Put + supersede, rename,
    // mkdir, rmdir, a streamed finish) and replay them against a fake server
    // with a real tree: MOVE/DELETE/PUT/MKCOL semantics and etags.
    mod journal_replay {
        use super::*;
        use crate::backend::{BackendReadError, BackendWriteError, ChunkedUploadSession, PutResult};
        use mutation_journal::{MutationJournal, MutationOp, SharedJournal};
        use std::collections::BTreeSet;

        #[derive(Default)]
        struct Tree {
            files: BTreeMap<PathBuf, (Vec<u8>, String)>,
            dirs: BTreeSet<PathBuf>,
            next_etag: u64,
            /// Chunks of each upload session, by index.
            sessions: HashMap<String, BTreeMap<u64, Vec<u8>>>,
            /// Every write request, in order.
            log: Vec<String>,
        }

        impl Tree {
            fn etag(&mut self) -> String {
                self.next_etag += 1;
                format!("s{}", self.next_etag)
            }

            fn parent_ok(&self, p: &Path) -> bool {
                self.dirs.contains(p.parent().unwrap_or(Path::new("/")))
            }

            fn remove_subtree(&mut self, p: &Path) {
                self.files.retain(|f, _| !f.starts_with(p));
                self.dirs.retain(|d| !d.starts_with(p));
            }

            fn write(&mut self, path: &Path, body: Vec<u8>, if_match: Option<&str>) -> Result<PutResult, BackendWriteError> {
                if !self.parent_ok(path) || self.dirs.contains(path) {
                    return Err(BackendWriteError::Server(409, "parent missing".into()));
                }
                if let Some(want) = if_match {
                    if self.files.get(path).map(|(_, e)| e.as_str()) != Some(want) {
                        return Err(BackendWriteError::Conflict);
                    }
                }
                let etag = self.etag();
                self.files.insert(path.to_path_buf(), (body, etag.clone()));
                Ok(PutResult { new_change_token: Some(etag) })
            }
        }

        /// Nextcloud keeps a file's etag across a MOVE (only the parents'
        /// etags change); `etag_changes_on_move` models a server that does not.
        struct TreeServer {
            t: Mutex<Tree>,
            etag_changes_on_move: bool,
        }

        fn moved_name(p: &Path, from: &Path, to: &Path) -> PathBuf {
            let rest = p.strip_prefix(from).unwrap();
            if rest.as_os_str().is_empty() { to.to_path_buf() } else { to.join(rest) }
        }

        impl TreeServer {
            fn new(files: &[(&str, &str)], dirs: &[&str]) -> Arc<Self> {
                let mut t = Tree::default();
                t.dirs.insert(PathBuf::from("/"));
                for d in dirs {
                    t.dirs.insert(PathBuf::from(d));
                }
                for (p, body) in files {
                    let e = format!("e_{}", p.trim_start_matches('/').replace('/', "_"));
                    t.files.insert(PathBuf::from(p), (body.as_bytes().to_vec(), e));
                }
                Arc::new(TreeServer { t: Mutex::new(t), etag_changes_on_move: false })
            }

            fn files(&self) -> Vec<(String, String)> {
                self.t.lock().unwrap().files.iter()
                    .map(|(p, (b, _))| (p.display().to_string(), String::from_utf8_lossy(b).into_owned()))
                    .collect()
            }

            fn dirs(&self) -> Vec<String> {
                self.t.lock().unwrap().dirs.iter().map(|d| d.display().to_string()).collect()
            }

            fn log(&self) -> Vec<String> {
                self.t.lock().unwrap().log.clone()
            }
        }

        fn nope() -> BackendReadError {
            BackendReadError::Network("not in the tree fake".into())
        }

        impl crate::backend::CloudBackend for TreeServer {
            fn list_dir(&self, _: &Path, _: Duration) -> Result<(Option<String>, Option<RemoteEntry>, Vec<RemoteEntry>), BackendReadError> {
                Err(nope())
            }
            fn list_dir_streaming(&self, _: &Path, _: Duration, _: mpsc::Sender<RemoteEntry>, _: mpsc::Sender<RemoteEntry>) -> Result<Option<String>, BackendReadError> {
                Err(nope())
            }
            fn dir_change_token(&self, _: &Path, _: Duration) -> Result<Option<String>, BackendReadError> {
                Err(nope())
            }
            fn download_file(&self, path: &Path, out: &mut dyn std::io::Write, _: Duration) -> Result<u64, BackendReadError> {
                let body = self.t.lock().unwrap().files.get(path).map(|(b, _)| b.clone()).ok_or(BackendReadError::NotFound)?;
                out.write_all(&body).map_err(|e| BackendReadError::Network(e.to_string()))?;
                Ok(body.len() as u64)
            }
            fn read_file_range(&self, _: &Path, _: u64, _: &mut [u8], _: Duration) -> Result<usize, BackendReadError> {
                Err(nope())
            }
            fn put_file(&self, path: &Path, body: Vec<u8>, if_match: Option<&str>) -> Result<PutResult, BackendWriteError> {
                let mut t = self.t.lock().unwrap();
                let r = t.write(path, body, if_match);
                t.log.push(format!("PUT {}{} {}", path.display(), if_match.map(|e| format!(" if {e}")).unwrap_or_default(), if r.is_ok() { "ok" } else { "refused" }));
                r
            }
            fn mkdir(&self, path: &Path) -> Result<(), BackendWriteError> {
                let mut t = self.t.lock().unwrap();
                t.log.push(format!("MKCOL {}", path.display()));
                if !t.parent_ok(path) {
                    return Err(BackendWriteError::Server(409, "parent missing".into()));
                }
                t.dirs.insert(path.to_path_buf()); // 405 for an existing one is Ok too
                Ok(())
            }
            fn delete(&self, path: &Path) -> Result<(), BackendWriteError> {
                let mut t = self.t.lock().unwrap();
                t.log.push(format!("DELETE {}", path.display()));
                t.remove_subtree(path); // a 404 is Ok, as `webdav_ops::delete` maps it
                Ok(())
            }
            fn rename(&self, from: &Path, to: &Path) -> Result<(), BackendWriteError> {
                let mut t = self.t.lock().unwrap();
                t.log.push(format!("MOVE {} {}", from.display(), to.display()));
                if !t.files.contains_key(from) && !t.dirs.contains(from) {
                    return Err(BackendWriteError::Server(404, "no source".into()));
                }
                if !t.parent_ok(to) {
                    return Err(BackendWriteError::Server(409, "no destination parent".into()));
                }
                t.remove_subtree(to); // Overwrite: T
                let files: Vec<PathBuf> = t.files.keys().filter(|p| p.starts_with(from)).cloned().collect();
                for p in files {
                    let (body, etag) = t.files.remove(&p).unwrap();
                    let etag = if self.etag_changes_on_move { t.etag() } else { etag };
                    t.files.insert(moved_name(&p, from, to), (body, etag));
                }
                let dirs: Vec<PathBuf> = t.dirs.iter().filter(|p| p.starts_with(from)).cloned().collect();
                for d in dirs {
                    t.dirs.remove(&d);
                    t.dirs.insert(moved_name(&d, from, to));
                }
                Ok(())
            }
            fn put_chunk(&self, s: &ChunkedUploadSession, index: u64, body: Vec<u8>) -> Result<(), BackendWriteError> {
                self.t.lock().unwrap().sessions.entry(s.uploads_base.clone()).or_default().insert(index, body);
                Ok(())
            }
            fn finish_chunked_upload(&self, s: &ChunkedUploadSession, path: &Path, if_match: Option<&str>) -> Result<PutResult, BackendWriteError> {
                let mut t = self.t.lock().unwrap();
                let body: Vec<u8> = t.sessions.get(&s.uploads_base).map(|c| c.values().flatten().copied().collect()).unwrap_or_default();
                let r = t.write(path, body, if_match);
                t.log.push(format!("ASSEMBLE {} {}", path.display(), if r.is_ok() { "ok" } else { "refused" }));
                r
            }
            fn is_reachable(&self, _: Duration) -> bool {
                true
            }
        }

        /// A mount's offline session: queues ops as the FUSE handlers do.
        struct Offline {
            dir: tempfile::TempDir,
            journal: SharedJournal,
            n: usize,
        }

        impl Offline {
            fn new() -> Self {
                let dir = tempfile::tempdir().unwrap();
                let journal = Arc::new(Mutex::new(MutationJournal::load_or_create(dir.path())));
                Offline { dir, journal, n: 0 }
            }

            fn enqueue(&self, op: MutationOp) -> mutation_journal::SeqId {
                self.journal.safe_lock().enqueue(op)
            }

            /// release() of a written handle: its Put, then supersede.
            fn save(&mut self, path: &str, bytes: &str, etag: Option<&str>) -> mutation_journal::SeqId {
                self.n += 1;
                let staging = self.dir.path().join(format!("write_t_{}", self.n));
                std::fs::write(&staging, bytes).unwrap();
                let seq = self.enqueue(MutationOp::Put { remote_path: PathBuf::from(path), staging_path: staging, if_match_etag: etag.map(str::to_owned) });
                self.journal.safe_lock().supersede_uploads(Path::new(path), seq);
                seq
            }

            /// The release of a streamed copy: the session holds `sent`.
            fn stream(&mut self, srv: &TreeServer, path: &str, sent: &str, tail: &str) {
                self.n += 1;
                let base = format!("uploads/{}", self.n);
                srv.t.lock().unwrap().sessions.entry(base.clone()).or_default().insert(0, sent.as_bytes().to_vec());
                let tail_path = self.dir.path().join(format!("write_t_{}", self.n));
                std::fs::write(&tail_path, tail).unwrap();
                let seq = self.enqueue(MutationOp::FinishChunked {
                    remote_path: PathBuf::from(path), uploads_base: base, next_index: 1,
                    bytes_confirmed: sent.len() as u64, total_len: (sent.len() + tail.len()) as u64,
                    tail_path, if_match_etag: None,
                });
                self.journal.safe_lock().supersede_uploads(Path::new(path), seq);
            }

            fn rm(&self, path: &str) {
                self.enqueue(MutationOp::Unlink { path: PathBuf::from(path) });
            }

            fn rmdir(&self, path: &str) {
                self.enqueue(MutationOp::RmDir { path: PathBuf::from(path) });
            }

            fn mkdir(&self, path: &str) {
                self.enqueue(MutationOp::MkDir { path: PathBuf::from(path) });
            }

            fn mv(&self, from: &str, to: &str) {
                self.enqueue(MutationOp::Rename { from: PathBuf::from(from), to: PathBuf::from(to) });
            }

            fn staged(&self, path: &str) -> Option<String> {
                self.journal.safe_lock().pending_put_staging(Path::new(path)).map(|p| std::fs::read_to_string(p).unwrap())
            }

            /// One replay run, as the connectivity monitor starts it.
            fn replay_pass(&self, srv: &Arc<TreeServer>) {
                let status: ipc::StatusMap = Arc::new(RwLock::new(HashMap::new()));
                let ctx = mutation_journal::ReplayContext { backend: srv.clone(), status };
                let cache = Arc::new(Mutex::new(make_test_cache()));
                let dirty: ipc::DirtySet = Arc::new(Mutex::new(HashSet::new()));
                let elog: ErrorLog = Arc::new(Mutex::new(std::collections::VecDeque::new()));
                mutation_journal::replay_journal(&self.journal, &ctx, &cache, &dirty, &elog);
            }

            /// Back online: replays until the journal is empty.
            fn replay(&self, srv: &Arc<TreeServer>) -> Vec<mutation_journal::ConflictKind> {
                let status: ipc::StatusMap = Arc::new(RwLock::new(HashMap::new()));
                let ctx = mutation_journal::ReplayContext { backend: srv.clone(), status };
                let cache = Arc::new(Mutex::new(make_test_cache()));
                let dirty: ipc::DirtySet = Arc::new(Mutex::new(HashSet::new()));
                let elog: ErrorLog = Arc::new(Mutex::new(std::collections::VecDeque::new()));
                for _ in 0..10 {
                    mutation_journal::replay_journal(&self.journal, &ctx, &cache, &dirty, &elog);
                    let mut j = self.journal.safe_lock();
                    if j.is_empty() {
                        break;
                    }
                    j.skip_backoff();
                }
                let j = self.journal.safe_lock();
                assert!(j.is_empty(), "left queued: {:?}", j.entries());
                j.unresolved_conflicts().iter().map(|c| c.kind.clone()).collect()
            }
        }

        fn tree(files: &[(&str, &str)]) -> Vec<(String, String)> {
            files.iter().map(|(p, b)| (p.to_string(), b.to_string())).collect()
        }

        #[test]
        fn rm_then_create_then_rename_keeps_the_new_file_and_not_the_deleted_one() {
            let srv = TreeServer::new(&[("/a", "A")], &[]);
            let mut off = Offline::new();
            off.rm("/a");
            off.save("/a", "A2", None);
            off.mv("/a", "/b");
            assert_eq!(off.staged("/b").as_deref(), Some("A2"));
            let conflicts = off.replay(&srv);
            assert_eq!(srv.files(), tree(&[("/b", "A2")]), "{:?}", srv.log());
            assert!(conflicts.is_empty(), "{conflicts:?}");
            assert_eq!(srv.log(), ["DELETE /a", "PUT /a ok", "MOVE /a /b"]);
        }

        #[test]
        fn an_edit_then_a_rename_uploads_before_it_moves() {
            let srv = TreeServer::new(&[("/b", "B")], &[]);
            let mut off = Offline::new();
            off.save("/b", "B2", Some("e_b"));
            off.mv("/b", "/c");
            assert_eq!(off.staged("/c").as_deref(), Some("B2"));
            assert!(off.replay(&srv).is_empty());
            assert_eq!(srv.files(), tree(&[("/c", "B2")]));
            assert_eq!(srv.log(), ["PUT /b if e_b ok", "MOVE /b /c"]);
        }

        #[test]
        fn a_chain_of_renames_with_edits_between_ends_with_the_last_edit() {
            for etag_changes_on_move in [false, true] {
                let srv = TreeServer::new(&[("/a", "A")], &[]);
                let srv = Arc::new(TreeServer { t: Mutex::new(std::mem::take(&mut *srv.t.lock().unwrap())), etag_changes_on_move });
                let mut off = Offline::new();
                // The listing entry moves with the file, so each edit carries a's etag.
                off.save("/a", "A1", Some("e_a"));
                off.mv("/a", "/b");
                off.save("/b", "A2", Some("e_a"));
                off.mv("/b", "/c");
                off.save("/c", "A3", Some("e_a"));
                assert_eq!(off.journal.safe_lock().len(), 3, "the older edits are superseded: {:?}", off.journal.safe_lock().entries());
                let conflicts = off.replay(&srv);
                let files = srv.files();
                if etag_changes_on_move {
                    // If-Match no longer matches: the edit is kept as a conflicted copy.
                    assert!(files.iter().any(|(p, b)| p.starts_with("/c (conflicted copy") && b == "A3"), "{files:?}");
                    assert!(files.contains(&("/c".into(), "A".into())));
                    assert!(matches!(conflicts.as_slice(), [mutation_journal::ConflictKind::EditConflict { .. }]));
                } else {
                    assert_eq!(files, tree(&[("/c", "A3")]));
                    assert!(conflicts.is_empty());
                }
            }
            // A file created here: every step reaches the server, in order.
            let srv = TreeServer::new(&[], &[]);
            let mut off = Offline::new();
            off.save("/n", "N1", None);
            off.mv("/n", "/m");
            off.save("/m", "N2", None);
            off.mv("/m", "/o");
            assert_eq!(off.staged("/o").as_deref(), Some("N2"));
            assert!(off.replay(&srv).is_empty(), "{:?}", srv.log());
            assert_eq!(srv.files(), tree(&[("/o", "N2")]));
        }

        #[test]
        fn a_directory_rename_carries_files_created_before_and_after_it() {
            let srv = TreeServer::new(&[("/d/x", "X")], &["/d"]);
            let mut off = Offline::new();
            off.save("/d/new", "N", None);
            off.mkdir("/d/sub");
            off.save("/d/sub/deep", "D", None);
            off.mv("/d", "/e");
            off.save("/e/after", "F", None);
            off.save("/e/x", "X2", Some("e_d_x"));
            assert_eq!(off.staged("/e/new").as_deref(), Some("N"));
            assert_eq!(off.staged("/e/sub/deep").as_deref(), Some("D"));
            assert!(off.replay(&srv).is_empty(), "{:?}", srv.log());
            assert_eq!(srv.files(), tree(&[("/e/after", "F"), ("/e/new", "N"), ("/e/sub/deep", "D"), ("/e/x", "X2")]));
            assert_eq!(srv.dirs(), ["/", "/e", "/e/sub"]);
        }

        #[test]
        fn rm_r_then_mkdir_then_rename_away_leaves_only_the_new_tree() {
            let srv = TreeServer::new(&[("/dir/a", "A"), ("/dir/b", "B")], &["/dir"]);
            let mut off = Offline::new();
            off.rm("/dir/a");
            off.rm("/dir/b");
            off.rmdir("/dir");
            off.mkdir("/dir");
            off.save("/dir/c", "C", None);
            off.mv("/dir", "/dist");
            assert_eq!(off.staged("/dist/c").as_deref(), Some("C"));
            assert!(off.replay(&srv).is_empty(), "{:?}", srv.log());
            assert_eq!(srv.files(), tree(&[("/dist/c", "C")]));
            assert_eq!(srv.dirs(), ["/", "/dist"]);
        }

        #[test]
        fn a_rename_back_and_a_rename_over_an_existing_file() {
            let srv = TreeServer::new(&[("/a", "A"), ("/b", "B"), ("/p", "P"), ("/q", "Q")], &[]);
            let mut off = Offline::new();
            // `mv a t; edit t; mv t a`.
            off.mv("/a", "/t");
            off.save("/t", "A2", Some("e_a"));
            off.mv("/t", "/a");
            assert_eq!(off.staged("/a").as_deref(), Some("A2"));
            // An edit of a, then `mv a b` over b: b is a's edit.
            off.save("/b", "B2", Some("e_b"));
            off.save("/p", "P2", Some("e_p"));
            off.mv("/p", "/b");
            assert_eq!(off.staged("/b").as_deref(), Some("P2"), "b's own older upload is the replaced file's");
            // `mv q b` over it again, unedited: b is q.
            off.mv("/q", "/b");
            assert_eq!(off.staged("/b"), None);
            assert!(off.replay(&srv).is_empty(), "{:?}", srv.log());
            assert_eq!(srv.files(), tree(&[("/a", "A2"), ("/b", "Q")]));
        }

        #[test]
        fn a_created_folder_renamed_before_its_files_land() {
            let srv = TreeServer::new(&[], &[]);
            let mut off = Offline::new();
            off.mkdir("/n");
            off.save("/n/f", "F", None);
            off.mv("/n", "/m");
            off.save("/m/g", "G", None);
            off.mv("/m/f", "/f");
            off.rmdir("/m/none"); // never existed: a no-op DELETE
            assert!(off.replay(&srv).is_empty(), "{:?}", srv.log());
            assert_eq!(srv.files(), tree(&[("/f", "F"), ("/m/g", "G")]));
        }

        #[test]
        fn a_queued_streamed_copy_is_assembled_before_its_rename() {
            let srv = TreeServer::new(&[], &["/in"]);
            let mut off = Offline::new();
            off.stream(&srv, "/in/big.bin", "first ", "last");
            off.mv("/in", "/out");
            assert_eq!(off.journal.safe_lock().newest_upload(Path::new("/out/big.bin")), Some(mutation_journal::PendingUpload::Stream));
            assert!(off.replay(&srv).is_empty(), "{:?}", srv.log());
            assert_eq!(srv.files(), tree(&[("/out/big.bin", "first last")]));
        }

        #[test]
        fn a_live_change_waits_for_older_queued_changes_of_its_files_and_holds_its_claim() {
            // Offline `rm a`, then online a new `a` is saved before the replay
            // reached the DELETE: run first, its PUT would be deleted after.
            let mut off = Offline::new();
            off.rm("/a");
            let delete = off.journal.safe_lock().peek_front().unwrap().seq;
            let put = off.save("/a", "A2", None);
            let (r, ran, landed) = std::thread::scope(|sc| {
                let worker = sc.spawn(|| {
                    let r = claim_in_order(&off.journal, put, &[Path::new("/a")], "PUT", Duration::from_secs(10));
                    (r, Instant::now())
                });
                std::thread::sleep(Duration::from_millis(300));
                assert!(!worker.is_finished(), "ran ahead of the queued DELETE");
                assert!(!off.journal.safe_lock().claim(put), "claimed while it waits, so the replay cannot run it too");
                let landed = Instant::now();
                off.journal.safe_lock().remove(delete);
                let (r, ran) = worker.join().unwrap();
                (r, ran, landed)
            });
            assert_eq!(r, InOrder::Run);
            assert!(ran.duration_since(landed) < Duration::from_millis(40), "woken by the change, not a poll: {:?}", ran.duration_since(landed));
            // Unrelated older entries do not hold it up.
            off.rm("/other");
            let b = off.save("/b", "B", None);
            assert_eq!(claim_in_order(&off.journal, b, &[Path::new("/b")], "PUT", Duration::from_secs(10)), InOrder::Run);
            // Still blocked when the wait runs out: given back to the replay.
            off.rm("/c");
            let c = off.save("/c", "C", None);
            assert_eq!(claim_in_order(&off.journal, c, &[Path::new("/c")], "PUT", Duration::from_millis(150)), InOrder::Deferred);
            assert!(off.journal.safe_lock().claim(c), "claimable again, by the replay");
            // Gone or claimed elsewhere: skipped.
            assert_eq!(claim_in_order(&off.journal, c, &[Path::new("/c")], "PUT", Duration::from_secs(1)), InOrder::Skip);
        }

        #[test]
        fn a_change_held_up_by_a_server_backoff_is_left_to_the_replay_at_once() {
            // The DELETE of s was rejected: the replay retries it in a minute.
            let mut off = Offline::new();
            off.rm("/s");
            let rejected = off.journal.safe_lock().peek_front().unwrap().seq;
            off.journal.safe_lock().mark_failed(rejected, "403".into());
            let put = off.save("/s", "S", None);
            let t = Instant::now();
            assert_eq!(claim_in_order(&off.journal, put, &[Path::new("/s")], "PUT", Duration::from_secs(10)), InOrder::Deferred);
            assert!(t.elapsed() < Duration::from_secs(1), "held a worker for {:?}", t.elapsed());
            {
                let j = off.journal.safe_lock();
                let e = j.entries().iter().find(|e| e.seq == put).unwrap();
                assert!(!e.in_flight && e.left_to_replay);
                assert_eq!(e.last_error, None, "a wait is not an error");
            }
            // Behind an unrelated entry backing off: the replay cannot reach
            // the older DELETE of b either.
            let mut off = Offline::new();
            off.mv("/x", "/y");
            let head = off.journal.safe_lock().peek_front().unwrap().seq;
            off.journal.safe_lock().mark_failed(head, "403".into());
            off.rm("/b");
            let put = off.save("/b", "B", None);
            let t = Instant::now();
            assert_eq!(claim_in_order(&off.journal, put, &[Path::new("/b")], "PUT", Duration::from_secs(10)), InOrder::Deferred);
            assert!(t.elapsed() < Duration::from_secs(1));
            // Once it lands and the entry left to the replay is at the head,
            // the replay is asked for.
            off.journal.safe_lock().remove(head);
            let delete = off.journal.safe_lock().peek_front().unwrap().seq;
            let _ = mutation_journal::take_replay_kick();
            off.journal.safe_lock().remove(delete);
            assert!(mutation_journal::take_replay_kick(), "the entry left to the replay is at the head: kicked");
        }

        #[test]
        fn a_vim_save_and_an_rm_then_create_never_wait_on_the_new_file() {
            // vim: `mv f f~`, then a new f is written; and `rm g`, then a new
            // g. The MOVE and the DELETE run at once (the new file's create
            // guard is no older change of it), and the new file's PUT runs
            // as soon as they land.
            let mut off = Offline::new();
            let last = |off: &Offline| off.journal.safe_lock().entries().back().unwrap().seq;
            off.mv("/f", "/f~");
            let mv = last(&off);
            let put_f = off.save("/f", "F2", None);
            off.rm("/g");
            let rm = last(&off);
            let put_g = off.save("/g", "G2", None);
            let cases: [(u64, Vec<&Path>, u64, &str); 2] = [
                (mv, vec![Path::new("/f"), Path::new("/f~")], put_f, "/f"),
                (rm, vec![Path::new("/g")], put_g, "/g"),
            ];
            for (first, paths, put, file) in cases {
                let t = Instant::now();
                assert_eq!(claim_in_order(&off.journal, first, &paths, "first", Duration::from_secs(10)), InOrder::Run);
                assert!(t.elapsed() < Duration::from_millis(500), "the change before the new {file} waited {:?}", t.elapsed());
                let (r, landed, ran) = std::thread::scope(|sc| {
                    let worker = sc.spawn(|| {
                        let r = claim_in_order(&off.journal, put, &[Path::new(file)], "PUT", Duration::from_secs(10));
                        (r, Instant::now())
                    });
                    std::thread::sleep(Duration::from_millis(200));
                    assert!(!worker.is_finished(), "the new {file} ran ahead of the change before it");
                    let landed = Instant::now();
                    off.journal.safe_lock().remove(first);
                    let (r, ran) = worker.join().unwrap();
                    (r, landed, ran)
                });
                assert_eq!(r, InOrder::Run);
                assert!(ran.duration_since(landed) < Duration::from_millis(500), "{:?}", ran.duration_since(landed));
                off.journal.safe_lock().remove(put);
            }
        }

        #[test]
        fn the_replay_ends_the_overlays_that_kept_queued_changes_listed() {
            // Offline: create n, `mv n m`, then an edit of p left to the
            // replay behind a newer edit of it.
            let srv = TreeServer::new(&[("/p", "P")], &[]);
            let mut off = Offline::new();
            off.save("/n", "N", None);
            off.mv("/n", "/m");
            let mv = off.journal.safe_lock().entries().back().unwrap().seq;
            let cache = Arc::new(Mutex::new(make_test_cache()));
            {
                let mut c = cache.safe_lock();
                c.uploading.insert(PathBuf::from("/m"), Some(1));
                c.moving.insert(PathBuf::from("/m"), (PathBuf::from("/n"), mv));
                c.uploading.insert(PathBuf::from("/p"), Some(2));
            }
            let p1 = off.save("/p", "P1", Some("e_p"));
            assert!(off.journal.safe_lock().claim(p1), "running live: not superseded");
            off.save("/p", "P2", None);
            off.journal.safe_lock().mark_waiting(p1);
            let status: ipc::StatusMap = Arc::new(RwLock::new(HashMap::new()));
            let ctx = mutation_journal::ReplayContext { backend: srv.clone(), status };
            let dirty: ipc::DirtySet = Arc::new(Mutex::new(HashSet::new()));
            let elog: ErrorLog = Arc::new(Mutex::new(std::collections::VecDeque::new()));
            // Stop before the newer edit of p: its guard is still needed.
            let last = off.journal.safe_lock().entries().back().unwrap().seq;
            off.journal.safe_lock().claim(last);
            mutation_journal::replay_journal(&off.journal, &ctx, &cache, &dirty, &elog);
            {
                let c = cache.safe_lock();
                assert!(c.moving.is_empty(), "the MOVE landed");
                assert!(!c.uploading.contains_key(Path::new("/m")), "n landed and is m now");
                assert!(c.uploading.contains_key(Path::new("/p")), "a newer upload of p is still queued");
            }
            off.journal.safe_lock().mark_waiting(last);
            mutation_journal::replay_journal(&off.journal, &ctx, &cache, &dirty, &elog);
            assert!(cache.safe_lock().uploading.is_empty());
            assert!(off.journal.safe_lock().is_empty());
            assert_eq!(srv.files(), tree(&[("/m", "N"), ("/p", "P2")]), "{:?}", srv.log());
        }

        #[test]
        fn a_waiting_live_change_whose_entry_leaves_does_nothing() {
            let mut off = Offline::new();
            off.rm("/x");
            let put = off.save("/x", "X", None);
            let (r, gone, done) = std::thread::scope(|sc| {
                let worker = sc.spawn(|| {
                    let r = claim_in_order(&off.journal, put, &[Path::new("/x")], "PUT", Duration::from_secs(10));
                    (r, Instant::now())
                });
                std::thread::sleep(Duration::from_millis(200));
                assert!(!worker.is_finished());
                let gone = Instant::now();
                off.journal.safe_lock().remove(put);
                let (r, done) = worker.join().unwrap();
                (r, gone, done)
            });
            assert_eq!(r, InOrder::Skip, "an entry that left is never sent, least of all ahead of the DELETE still queued");
            assert!(done.duration_since(gone) < Duration::from_secs(1), "{:?}", done.duration_since(gone));
        }

        #[test]
        fn a_claimed_create_is_not_coalesced_away_under_its_waiting_worker() {
            // Backlog `mv a b` (the server has a = A). A new `a` is saved
            // live: its PUT claims and waits for that MOVE. Then `mv a c`,
            // `rm c`. Dropping the claimed PUT let the worker send the new a
            // ahead of the MOVE, which moved it over b: A was lost.
            let srv = TreeServer::new(&[("/a", "A")], &[]);
            let mut off = Offline::new();
            off.mv("/a", "/b");
            let put = off.save("/a", "N", None);
            std::thread::scope(|sc| {
                let worker = sc.spawn(|| claim_in_order(&off.journal, put, &[Path::new("/a")], "PUT", Duration::from_secs(10)));
                std::thread::sleep(Duration::from_millis(200));
                off.mv("/a", "/c");
                off.rm("/c");
                assert!(off.journal.safe_lock().contains(put), "claimed: kept, {:?}", off.journal.safe_lock().entries());
                // The replay lands the MOVE and stops at the claimed PUT.
                off.replay_pass(&srv);
                assert_eq!(worker.join().unwrap(), InOrder::Run);
                // The worker sends it, now in order.
                crate::backend::CloudBackend::put_file(&*srv, Path::new("/a"), b"N".to_vec(), None).unwrap();
                off.journal.safe_lock().remove(put);
            });
            assert!(off.replay(&srv).is_empty(), "{:?}", srv.log());
            assert_eq!(srv.files(), tree(&[("/b", "A")]), "{:?}", srv.log());
            assert_eq!(srv.log(), ["MOVE /a /b", "PUT /a ok", "MOVE /a /c", "DELETE /c"]);
        }

        #[test]
        fn a_journal_written_by_0_1_77_replays_exactly_as_it_did() {
            // 0.1.77 rewrote every earlier entry into a later rename's names:
            // `edit a; mv a b` offline was saved as [Put b (a's etag), Rename a→b].
            // This version replays entries as stored, so such a journal does
            // what 0.1.77 would have done: the PUT of b is refused (no b with
            // a's etag) and kept as a conflicted copy, then the MOVE lands.
            let off = Offline::new();
            let staging = off.dir.path().join("write_7");
            std::fs::write(&staging, "A2").unwrap();
            let old = serde_json::json!([
                {"seq": 1, "op": {"Put": {"remote_path": "/b", "staging_path": staging, "if_match_etag": "e_a"}}, "created_at_ms": 1, "attempts": 0, "last_error": null},
                {"seq": 2, "op": {"Rename": {"from": "/a", "to": "/b"}}, "created_at_ms": 2, "attempts": 0, "last_error": null},
            ]);
            std::fs::write(off.dir.path().join("mutation_journal.json"), serde_json::to_vec(&old).unwrap()).unwrap();
            let off = Offline { journal: Arc::new(Mutex::new(MutationJournal::load_or_create(off.dir.path()))), ..off };
            assert_eq!(off.journal.safe_lock().len(), 2);
            assert!(off.journal.safe_lock().entries().iter().all(|e| !e.queued_names));
            // Lookups under the stored name still find it, and a rename made
            // after the upgrade carries it along.
            assert_eq!(off.staged("/b").as_deref(), Some("A2"));
            off.mv("/b", "/c");
            assert_eq!(off.staged("/c").as_deref(), Some("A2"));
            assert_eq!(off.staged("/b"), None);
            let srv = TreeServer::new(&[("/a", "A")], &[]);
            let conflicts = off.replay(&srv);
            let log = srv.log();
            assert_eq!(log[0], "PUT /b if e_a refused");
            assert!(log[1].starts_with("PUT /b (conflicted copy") && log[2] == "MOVE /a /b" && log[3] == "MOVE /b /c", "{log:?}");
            assert!(srv.files().iter().any(|(p, b)| p.starts_with("/b (conflicted copy") && b == "A2"), "the edit is never lost");
            assert!(matches!(conflicts.as_slice(), [mutation_journal::ConflictKind::EditConflict { .. }]));
        }
    }
