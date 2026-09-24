//! The write path of an open file (write, truncate, flush, fsync, release),
//! arranged so the FUSE dispatch thread never waits on the network or on a big
//! local copy or fsync (step 8 of the 2026-09-24 review).
//!
//! Before, `write()` PUT each filled 10 MB chunk of a streamed upload inline,
//! with retries and backoff sleeps, so every other request on the mount (a
//! `stat` of an unrelated cached file included) queued behind the upload; and
//! `flush`/`fsync`/`release` fsynced whole staging files and the journal there.
//! Now each handle's work goes through its lane (`fh_lane.rs`), which keeps it
//! in the order the kernel sent it. What is cheap (a pwrite or append to the
//! staging file) still runs on the dispatch thread when the lane is idle; a
//! chunk graduation runs on `bg::UPLOAD`, and seeding a staging file from a
//! kept copy, `flush` and `fsync` run on `bg::DISK`, each owning its reply. The
//! handle's state is updated before the reply goes out, and RELEASE (which the
//! kernel sends only after the handle's last write was answered) queues behind
//! anything still in flight, so it always commits the finished file, once.
//! Release's own fsyncs moved into the journal's group commit.
//!
//! Lock order: never `cache` while holding `open_files` (rename nests them the
//! other way round on the dispatch thread while this runs on a worker).

use super::*;

/// What the write path needs off the dispatch thread: the shared handles of
/// `MetaCtx` plus the journal side of a commit. Built once, at the first write.
#[derive(Clone)]
pub(crate) struct WriteCtx {
    pub(crate) meta: MetaCtx,
    pub(crate) lanes: Arc<fh_lane::FhLanes>,
    pub(crate) journal: mutation_journal::SharedJournal,
    pub(crate) dirty: ipc::DirtySet,
    pub(crate) error_log: ErrorLog,
    pub(crate) log_user: Arc<str>,
    pub(crate) auto_keep_locally_modified_files: bool,
    // Where staging files live; fixed for the mount, so no `cache` lock.
    pub(crate) cache_dir: PathBuf,
    // `bg::UPLOAD` and `bg::DISK`; tests swap in pools that refuse.
    pub(crate) upload_pool: &'static bg::Pool,
    pub(crate) disk_pool: &'static bg::Pool,
}

impl std::ops::Deref for WriteCtx {
    type Target = MetaCtx;
    fn deref(&self) -> &MetaCtx {
        &self.meta
    }
}

/// Answers a size-changing `setattr` once the handle's side of it is done.
pub(crate) fn truncate_reply(meta: &MetaCtx, pid: u32, ino: u64, new_size: u64, reply: ReplyAttr) {
    let Some(path) = meta.cache.safe_lock().get_path(ino) else {
        reply.attr(&TTL, &make_unknown_file_attr(ino, new_size));
        return;
    };
    // The parent listing can have been evicted since this inode was
    // handed out; `with_child` re-lists it rather than answering from
    // nothing, off the dispatch thread.
    with_child(meta, pid, &path, reply, move |c, p, e| attr_for(c, ino, p, e), |_| None, move |_, _, reply, r| {
        let mut attr = match r {
            Resolved::Found(attr) => attr,
            // This branch only runs for a size change, i.e. a truncate of a
            // regular file: a directory attr here would tell the writer its
            // own file is a directory.
            Resolved::Absent | Resolved::Unknown(_) => make_unknown_file_attr(ino, new_size),
        };
        set_attr_size(&mut attr, new_size);
        reply.attr(&TTL, &attr);
    });
}

/// Where a `write()` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteCost {
    /// A pwrite or append to the staging file: the dispatch thread, if the lane is idle.
    Inline,
    /// The staging file has to be seeded from the kept copy first (`bg::DISK`).
    Seed,
    /// This write fills a chunk of a streamed upload, which is then PUT (`bg::UPLOAD`).
    Graduate,
}

/// Seeds a staging file with the kept copy (or empty), via a temp file, so a
/// copy that fails halfway never leaves a staging file a later write would
/// take as already seeded.
fn seed_staging(local: Option<&Path>, wp: &Path) -> std::io::Result<()> {
    let Some(local) = local else {
        return std::fs::File::create(wp).map(|_| ());
    };
    let tmp = wp.with_extension("seed");
    let copied = std::fs::copy(local, &tmp).and_then(|_| std::fs::rename(&tmp, wp));
    if copied.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    copied
}

impl WriteCtx {
    /// `write()`: inline when the lane is idle and the write is a plain pwrite
    /// or append, else queued on the handle's lane with its reply.
    ///
    /// Only a write classified `Graduate` and running on `upload_pool` may PUT
    /// a chunk. Any other write that takes the tail past a chunk (it was
    /// classified before the offline flag or the handle's state changed, or
    /// the pool refused it and it runs on the caller, possibly `fuser-0`) just
    /// appends, and the handle's next write, which `write_cost` then classifies
    /// `Graduate`, sends every full chunk the tail holds. If no write follows,
    /// release commits the longer tail as the last chunk. So a refusal costs a
    /// little extra staging disk, never a network wait on the dispatch thread
    /// and never an error to the writer.
    pub(crate) fn dispatch_write(
        &self,
        fh: u64,
        path: PathBuf,
        offset: u64,
        data: &[u8],
        reply: impl FnOnce(Result<u32, Errno>) + Send + 'static,
    ) {
        let cost = self.write_cost(fh, offset, data.len());
        let pool: &'static bg::Pool = match cost {
            WriteCost::Inline => match self.lanes.claim(fh) {
                Some(_lane) => return reply(self.write_answer(fh, &path, offset, data, false)),
                // Behind this handle's work in flight.
                None => self.disk_pool,
            },
            WriteCost::Seed => self.disk_pool,
            WriteCost::Graduate => self.upload_pool,
        };
        let (c, data) = (self.clone(), data.to_vec());
        self.lanes.run(fh, pool, move |ran| {
            let may_graduate = cost == WriteCost::Graduate && ran == fh_lane::Ran::OnPool;
            let r = c.write_answer(fh, &path, offset, &data, may_graduate);
            move || reply(r)
        });
    }

    /// The handle's side of a size-changing `setattr`; `then` answers it.
    pub(crate) fn dispatch_truncate(
        &self,
        fh: u64,
        new_size: u64,
        then: impl FnOnce(&MetaCtx, Result<(), Errno>) + Send + 'static,
    ) {
        if !self.truncate_needs_seed(fh) {
            if let Some(_lane) = self.lanes.claim(fh) {
                return then(&self.meta, self.truncate_answer(fh, new_size));
            }
        }
        // Seeding copies the whole kept file, or the handle has work in flight.
        let c = self.clone();
        self.lanes.run(fh, self.disk_pool, move |_| {
            let r = c.truncate_answer(fh, new_size);
            move || then(&c.meta, r)
        });
    }

    /// `flush()`: a clean handle (most closes) is answered inline; a dirty one
    /// fsyncs its staging file after anything in flight on it. close() waits
    /// for this reply; the dispatch thread doesn't.
    pub(crate) fn dispatch_flush(&self, fh: u64, reply: impl FnOnce() + Send + 'static) {
        if let Some(_lane) = self.lanes.claim(fh) {
            if !self.flush_has_work(fh) {
                return reply();
            }
        }
        let c = self.clone();
        self.lanes.run(fh, self.disk_pool, move |_| {
            c.flush_answer(fh);
            reply
        });
    }

    /// `fsync()`: answered only once the staged bytes are on disk.
    pub(crate) fn dispatch_fsync(&self, fh: u64, reply: impl FnOnce() + Send + 'static) {
        if let Some(_lane) = self.lanes.claim(fh) {
            if !self.fsync_has_work(fh) {
                return reply();
            }
        }
        let c = self.clone();
        self.lanes.run(fh, self.disk_pool, move |_| {
            c.fsync_answer(fh);
            reply
        });
    }

    /// `release()`: inline when nothing is in flight on the handle. The kernel
    /// sends RELEASE only after the handle's last write was answered, so
    /// anything still in flight is writeback of a shared mapping: queue behind
    /// it, so the commit is of the finished file and happens once.
    pub(crate) fn dispatch_release(&self, fh: u64, reply: impl FnOnce() + Send + 'static) {
        if let Some(_lane) = self.lanes.claim(fh) {
            return self.release_answer(fh, reply);
        }
        let c = self.clone();
        self.lanes.run(fh, self.disk_pool, move |_| {
            c.release_answer(fh, reply);
            || {}
        });
    }

    fn staging_path_for(&self, fh: u64) -> PathBuf {
        self.cache_dir.join(mutation_journal::staging_file_name(fh))
    }

    /// Picks where a write of `len` bytes at `offset` on `fh` runs. Decided
    /// once: `write_answer` may find the handle changed (the offline flag, or a
    /// write queued ahead of it), but only ever graduates a chunk when this
    /// said `Graduate` and it runs on the upload pool.
    pub(crate) fn write_cost(&self, fh: u64, offset: u64, len: usize) -> WriteCost {
        let files = self.open_files.safe_lock();
        let Some(of) = files.get(&fh) else { return WriteCost::Inline };
        if of.local.is_some() && of.write_path.as_ref().map_or(true, |wp| !wp.exists()) {
            return WriteCost::Seed;
        }
        let blocked_by_offline = of.chunk_upload.is_none() && self.conn.is_offline.load(Ordering::Relaxed);
        let confirmed = of.chunk_upload.as_ref().map_or(0, |c| c.bytes_confirmed);
        if of.stream_eligible && offset == of.total_written && !blocked_by_offline
            && of.total_written + len as u64 - confirmed >= webdav_ops::CHUNK_SIZE as u64
        {
            return WriteCost::Graduate;
        }
        WriteCost::Inline
    }

    /// Creates `fh`'s staging file if this is its first write or truncate.
    /// The copy runs outside `open_files`: the handle's lane keeps every
    /// other step on it waiting, and nothing else touches its staging file.
    fn ensure_staging(&self, fh: u64, what: &str) -> Result<PathBuf, Errno> {
        let (wp, seed) = {
            let mut files = self.open_files.safe_lock();
            let of = files.get_mut(&fh).ok_or(Errno::EIO)?;
            let wp = of.write_path.get_or_insert_with(|| self.staging_path_for(fh)).clone();
            let seed = (!wp.exists()).then(|| of.local.clone());
            (wp, seed)
        };
        if let Some(local) = seed {
            if let Err(e) = seed_staging(local.as_deref(), &wp) {
                log::error!("{}: cannot create staging file {}: {}", what, wp.display(), e);
                return Err(Errno::EIO);
            }
        }
        Ok(wp)
    }

    /// Everything `write()` does, on whichever thread its lane runs it. The
    /// handle's state is updated before this returns, so the reply that
    /// follows never reports a write the next request can't see.
    ///
    /// `may_graduate` is false unless this runs on the upload pool: a write
    /// that fills a chunk without it only appends (see `dispatch_write`).
    pub(crate) fn write_answer(&self, fh: u64, path: &Path, offset: u64, data: &[u8], may_graduate: bool) -> Result<u32, Errno> {
        let wp = self.ensure_staging(fh, "write")?;

        // Bounded chunked streaming: a freshly-written file, written purely
        // sequentially from offset 0 while online, never keeps more than ~1
        // chunk of unsent bytes on local disk — completed CHUNK_SIZE chunks are
        // pushed to Nextcloud's chunked-upload extension as they fill, instead
        // of the whole file landing on disk before any upload starts. See
        // ChunkUploadState's doc comment for why any deviation once a chunk has
        // actually been sent fails the handle instead of trying to reconcile.
        //
        // `is_offline` only gates *starting* a fresh session (chunk_upload is
        // still None): it is a mount-wide flag that unrelated traffic on any
        // other file handle (a read, a PROPFIND) can flip momentarily, and once
        // a session has a chunk sitting on the server there is no local-only
        // fallback for it — bailing out here on every such blip would abort a
        // perfectly healthy in-progress upload of file B just because file A's
        // read hit a transient error at the same moment. Once streaming has
        // actually begun, let the tail keep buffering locally and let the next
        // real network call (graduate_chunk, which retries transient failures)
        // be the one to decide whether the server is actually gone.
        let stream = {
            let mut files = self.open_files.safe_lock();
            let of = files.get_mut(&fh).ok_or(Errno::EIO)?;
            let blocked_by_offline = of.chunk_upload.is_none() && self.conn.is_offline.load(Ordering::Relaxed);
            if of.stream_eligible && offset == of.total_written && !blocked_by_offline {
                true
            } else if of.chunk_upload.is_some() {
                // At least one chunk is already sitting on the server with nothing
                // local to reconstruct it from — a non-sequential write, a resize,
                // or going offline mid-copy cannot be reconciled here. Fail the
                // write rather than misplace bytes in the small tail file or lose
                // the already-uploaded prefix silently. A plain sequential copy
                // never reaches this.
                log::error!(
                    "write: non-sequential write on {} after chunked upload had begun (offset={}, expected={}) — aborting handle",
                    path.display(), offset, of.total_written,
                );
                of.upload_failed = true;
                return Err(Errno::EIO);
            } else {
                // First deviation before any chunk was ever sent — the tail file
                // already holds 100% of the content written so far, so there is
                // nothing to reconcile; just stop trying to stream this handle.
                of.stream_eligible = false;
                false
            }
        };

        if !stream {
            let written = std::fs::OpenOptions::new().write(true).create(true).open(&wp)
                .map_err(|e| log::error!("open staging file: {}", e))
                .and_then(|f| f.write_at(data, offset).map_err(|e| log::error!("write to staging file: {}", e)))
                .map_err(|()| Errno::EIO)?;
            if let Some(of) = self.open_files.safe_lock().get_mut(&fh) {
                of.dirty = true;
            }
            return Ok(written as u32);
        }

        if let Err(e) = append_to_tail_file(&wp, data) {
            log::error!("write to staging tail file: {}", e);
            return Err(Errno::EIO);
        }
        let (total_written, existing_session) = {
            let mut files = self.open_files.safe_lock();
            let of = files.get_mut(&fh).ok_or(Errno::EIO)?;
            of.dirty = true;
            of.total_written += data.len() as u64;
            let bytes_confirmed = of.chunk_upload.as_ref().map_or(0, |c| c.bytes_confirmed);
            if !may_graduate || of.total_written - bytes_confirmed < webdav_ops::CHUNK_SIZE as u64 {
                return Ok(data.len() as u32);
            }
            (of.total_written, of.chunk_upload.clone())
        };
        let had_session = existing_session.is_some();
        // Each chunk the server confirmed is recorded before the tail file is
        // cut down to the leftover bytes: a `stat` reading the tail's length
        // while `chunk_upload` was still unset would see a file that shrank.
        let mut publish = |s: &ChunkUploadState| {
            if let Some(of) = self.open_files.safe_lock().get_mut(&fh) {
                of.chunk_upload = Some(s.clone());
            }
        };
        match graduate_chunk(&*self.conn.backend, path, &wp, existing_session, total_written, &mut publish) {
            Ok(new_state) => {
                if let Some(of) = self.open_files.safe_lock().get_mut(&fh) {
                    of.chunk_upload = Some(new_state);
                }
            }
            Err((e, None)) if !had_session => {
                // The session never opened (e.g. a server without chunked uploads
                // answers MKCOL with 404): nothing was sent, so the tail still holds
                // every byte. Keep it as a whole-file staging copy for release().
                log::warn!("write: chunked upload unavailable for {} ({}) — staging the whole file instead", path.display(), e);
                if let Some(of) = self.open_files.safe_lock().get_mut(&fh) {
                    of.stream_eligible = false;
                }
            }
            Err((e, partial_state)) => {
                log::warn!("write: chunk upload failed for {}, aborting handle: {}", path.display(), e);
                // Persist whatever session state the server actually confirmed
                // (including a session opened by this very call) so release()'s
                // abandoned-session cleanup can still find and abort it instead
                // of leaking it server-side.
                if let Some(of) = self.open_files.safe_lock().get_mut(&fh) {
                    of.chunk_upload = partial_state;
                    of.upload_failed = true;
                }
                return Err(Errno::EIO);
            }
        }
        Ok(data.len() as u32)
    }

    /// Whether a truncate of `fh` has to seed its staging file from the kept copy first.
    pub(crate) fn truncate_needs_seed(&self, fh: u64) -> bool {
        self.open_files.safe_lock().get(&fh).is_some_and(|of| {
            of.chunk_upload.is_none() && of.local.is_some() && of.write_path.as_ref().map_or(true, |wp| !wp.exists())
        })
    }

    /// The open-handle half of a size-changing `setattr` on `fh`.
    pub(crate) fn truncate_answer(&self, fh: u64, new_size: u64) -> Result<(), Errno> {
        {
            let mut files = self.open_files.safe_lock();
            let Some(of) = files.get_mut(&fh) else { return Ok(()) };
            if let Some(cs) = &of.chunk_upload {
                // A streamed upload is in progress: already-sent chunks are
                // gone from local disk, so only a no-op truncate (to the
                // length already accounted for) can be honored — anything
                // else would need to shrink/extend bytes we no longer have.
                if new_size != of.total_written {
                    log::error!(
                        "setattr: truncate on fh {} to {} while a streamed upload is in progress ({} bytes already sent) — aborting handle",
                        fh, new_size, cs.bytes_confirmed,
                    );
                    of.upload_failed = true;
                    return Err(Errno::EIO);
                }
                of.dirty = true;
                return Ok(());
            }
            // No chunk sent yet, so the tail file still holds 100% of the
            // content — safe to truncate directly and stop trying to stream
            // this handle (a resize is not a sequential append the fast path
            // can reason about).
            of.stream_eligible = false;
        }
        let wp = self.ensure_staging(fh, "setattr")?;
        match std::fs::OpenOptions::new().write(true).open(&wp) {
            Ok(f) => {
                if let Err(e) = f.set_len(new_size) {
                    log::error!("setattr: truncate staging file failed: {}", e);
                    return Err(Errno::EIO);
                }
            }
            Err(e) => {
                log::error!("setattr: open staging file for truncate failed: {}", e);
                return Err(Errno::EIO);
            }
        }
        if let Some(of) = self.open_files.safe_lock().get_mut(&fh) {
            of.total_written = new_size;
            of.dirty = true;
        }
        Ok(())
    }

    /// Whether `flush` of `fh` has anything to do (most closes are of clean handles).
    pub(crate) fn flush_has_work(&self, fh: u64) -> bool {
        self.open_files.safe_lock().get(&fh).is_some_and(|of| of.dirty && of.write_path.is_some())
    }

    /// Durability only. FLUSH is sent on every close() of any fd sharing this handle —
    /// a forked child or a shell's per-command redirect exiting mid-write included — so
    /// it is never the end of the file: committing here uploaded half-written snapshots
    /// and deleted the staging file under a writer. release() commits.
    pub(crate) fn flush_answer(&self, fh: u64) {
        let staged = self.open_files.safe_lock().get(&fh).filter(|of| of.dirty).and_then(|of| {
            let streamed_size = of.chunk_upload.as_ref().map(|_| of.total_written);
            of.write_path.clone().map(|wp| (wp, of.remote_path.clone(), streamed_size))
        });
        let Some((wp, remote_path, streamed_size)) = staged else { return };
        if let Ok(f) = std::fs::File::open(&wp) {
            if let Err(e) = f.sync_all() {
                log::warn!("flush: fsync staging {} failed: {}", wp.display(), e);
            }
        }
        if let Some(size) = streamed_size.or_else(|| std::fs::metadata(&wp).ok().map(|m| m.len())) {
            set_listed_size(&mut self.cache.safe_lock(), &remote_path, size);
        }
    }

    /// Whether `fsync` of `fh` has a staging file to sync.
    pub(crate) fn fsync_has_work(&self, fh: u64) -> bool {
        self.open_files.safe_lock().get(&fh).is_some_and(|of| of.write_path.is_some())
    }

    /// The staged bytes are on disk once this returns; the reply follows it.
    ///
    /// No journal record is written: the file is queued for upload only at
    /// release. After a crash with the handle still open, the startup sweep
    /// finds its staging unnamed by the journal and moves it to
    /// `<cache_dir>/recovered/` (`mutation_journal::quarantine_unreferenced_staging`)
    /// instead of deleting it, so fsynced bytes survive but are not uploaded.
    pub(crate) fn fsync_answer(&self, fh: u64) {
        let wp = self.open_files.safe_lock().get(&fh).and_then(|of| of.write_path.clone());
        if let Some(Ok(f)) = wp.map(std::fs::File::open) {
            if let Err(e) = f.sync_all() {
                log::warn!("fsync: staging {} failed: {}", fh, e);
            }
        }
    }

    /// Everything `release()` does. `reply` is sent once the handle is gone and
    /// before its commit starts, as before: RELEASE is the last request on a
    /// handle, so nothing waits on the commit.
    pub(crate) fn release_answer(&self, fh: u64, reply: impl FnOnce()) {
        self.publish_released_size(fh);
        // The last close of the handle: the only point where no further write can arrive.
        let Some(of) = self.open_files.safe_lock().remove(&fh) else {
            reply();
            return;
        };
        if of.writer {
            self.open_writers.fetch_sub(1, Ordering::Relaxed);
        }
        if let Some(ref parent) = of.pinned_parent {
            self.cache.safe_lock().unpin_dir(parent);
        }
        self.io_modes.safe_lock().release(of.ino, of.io_kind);
        reply();

        if of.upload_failed || of.unlinked {
            // A write already returned EIO after chunks reached the server (assembling them
            // would publish an incomplete file), or the file was deleted while open
            // (committing would re-create it). Tear any session down instead.
            if of.unlinked {
                if let Some(ref wp) = of.write_path {
                    let _ = std::fs::remove_file(wp);
                }
            }
            self.cache.safe_lock().uploading.remove(&of.remote_path);
            if let Some(cs) = of.chunk_upload {
                log::warn!(
                    "release: fh {} streamed upload of {} failed mid-copy ({}) — abandoning it, the copy must be retried",
                    fh, of.remote_path.display(), cs.uploads_base,
                );
                // On the mutation pool, which never refuses: a dropped abort
                // leaks the session's chunks on the server.
                let backend = self.conn.backend.clone();
                submit_mutation(move || {
                    backend.abort_chunked_upload(&backend::ChunkedUploadSession { uploads_base: cs.uploads_base });
                });
            }
            return;
        }
        if !of.dirty {
            if let Some(ref wp) = of.write_path {
                // Opened writable but never written — discard the staging copy and the
                // create() guard; no PUT will follow.
                let _ = std::fs::remove_file(wp);
                self.cache.safe_lock().uploading.remove(&of.remote_path);
            }
            return;
        }
        if let Err(e) = self.commit_released(FileHandle(fh), of) {
            log::warn!("release: commit of fh {} failed: {:?}", fh, e);
        }
    }

    /// Publishes the size a committing handle is about to upload while the
    /// handle still overlays it (`overlay_local_size`), so a `stat` between
    /// the handle's removal and the commit never sees the server's older,
    /// smaller size: the kernel would shrink the inode and a concurrent reader
    /// would take the old end for EOF. The listing entry carries it, and an
    /// in-flight upload guard (create()'s, or an earlier commit's) is raised to
    /// it, which `attr_for` prefers to a refresh from the server. Offline, a
    /// guard is left alone: nothing clears one once the journal replays.
    fn publish_released_size(&self, fh: u64) {
        let staged = self.open_files.safe_lock().get(&fh)
            .filter(|of| of.dirty && !of.upload_failed && !of.unlinked)
            .and_then(|of| {
                let streamed = of.chunk_upload.as_ref().map(|_| of.total_written);
                of.write_path.clone().map(|wp| (of.remote_path.clone(), wp, streamed))
            });
        let Some((remote_path, wp, streamed)) = staged else { return };
        // Stat outside both locks.
        let Some(size) = streamed.or_else(|| std::fs::metadata(&wp).ok().map(|m| m.len())) else { return };
        let mut c = self.cache.safe_lock();
        if !self.conn.is_offline.load(Ordering::Relaxed) {
            if let Some(guard) = c.uploading.get_mut(&remote_path) {
                *guard = Some(size);
            }
        }
        set_listed_size(&mut c, &remote_path, size);
    }

    /// Journals the end of a streamed upload and hands it to a worker, so a failed finish
    /// is retried from the journal and release() never waits on the network. The
    /// journal fsyncs the tail before any journal naming it is written.
    fn commit_streamed(
        &self,
        remote_path: PathBuf,
        tail_path: PathBuf,
        original_etag: Option<String>,
        opened_gen: u64,
        total_len: u64,
        cs: ChunkUploadState,
    ) -> Result<(), Errno> {
        log::info!(
            "[{}] COMMIT (streamed) {} size={} etag={:?}",
            self.log_user, remote_path.display(), total_len, original_etag,
        );
        let parent = remote_path.parent().unwrap_or(Path::new("/")).to_path_buf();
        {
            let mut c = self.cache.safe_lock();
            if let Some(dir) = c.dir_cache.get_mut(&parent) {
                let mut files = (*dir.files).clone();
                if let Some(e) = files.iter_mut().find(|e| e.path == remote_path) {
                    e.size = total_len;
                }
                dir.files = Arc::new(files);
            }
        }
        let seq = self.journal.safe_lock().enqueue(mutation_journal::MutationOp::FinishChunked {
            remote_path: remote_path.clone(),
            uploads_base: cs.uploads_base.clone(),
            next_index: cs.next_index,
            bytes_confirmed: cs.bytes_confirmed,
            total_len,
            tail_path: tail_path.clone(),
            if_match_etag: original_etag.clone(),
        });
        self.journal.safe_lock().supersede_uploads(&remote_path, seq);
        self.dirty.safe_lock().insert(remote_path.clone());
        if self.conn.is_offline.load(Ordering::Relaxed) {
            self.status.safe_write().insert(remote_path.clone(), FileStatus::PendingSync);
            return Ok(());
        }
        self.status.safe_write().insert(remote_path.clone(), FileStatus::Uploading);
        self.cache.safe_lock().uploading.insert(remote_path.clone(), Some(total_len));

        let conn = self.conn.clone();
        let cache = self.cache.clone();
        let dirty = self.dirty.clone();
        let elog = self.error_log.clone();
        let journal = self.journal.clone();
        let smap = self.status.clone();
        let uploads = self.uploads.clone();
        let ticket = uploads.ticket_entry(&remote_path);
        submit_mutation(move || {
            ticket.wait();
            if !journal.safe_lock().claim(seq) {
                log::debug!("streamed finish of {} skipped — superseded or replayed", remote_path.display());
                if !journal.safe_lock().has_pending_put(&remote_path) {
                    cache.safe_lock().uploading.remove(&remote_path);
                }
                return;
            }
            let etag = uploads.etag_for(&remote_path, opened_gen, original_etag);
            let _permit = conn.throttle.acquire();
            let result = mutation_journal::finish_chunked(
                &*conn.backend, &cs.uploads_base, cs.next_index, cs.bytes_confirmed, total_len,
                &tail_path, &remote_path, etag.as_deref(),
            );
            cache.safe_lock().uploading.remove(&remote_path);
            match result {
                Ok(result) => {
                    log::info!("PUT (streamed) {} → new etag {:?}", remote_path.display(), result.new_change_token);
                    uploads.record(&remote_path, result.new_change_token.clone());
                    {
                        let mut c = cache.safe_lock();
                        if let Some(dir) = c.dir_cache.get_mut(&parent) {
                            let mut files = (*dir.files).clone();
                            if let Some(entry) = files.iter_mut().find(|e| e.path == remote_path) {
                                entry.change_token = result.new_change_token;
                                entry.size = total_len;
                                entry.modified = Some(SystemTime::now());
                            }
                            dir.files = Arc::new(files);
                            dir.at = Instant::now() - (DIR_CACHE_TTL + Duration::from_secs(1));
                        }
                    }
                    // The tail is only the end of the file, so it can never become a kept copy.
                    smap.safe_write().insert(remote_path.clone(), FileStatus::Synced);
                    journal.safe_lock().remove_discarding(seq, &tail_path);
                }
                Err(backend::BackendWriteError::Conflict) => {
                    // Every chunk is already on the server: assemble ours as a conflicted copy.
                    let conflict_name = make_conflict_name(&remote_path);
                    let session = backend::ChunkedUploadSession { uploads_base: cs.uploads_base.clone() };
                    match conn.backend.finish_chunked_upload(&session, &conflict_name, None) {
                        Ok(_) => log::info!("conflicted copy assembled as {}", conflict_name.display()),
                        Err(e) => log::error!("failed to assemble conflicted copy {}: {}", conflict_name.display(), e),
                    }
                    push_error(&elog, remote_path.clone(), SyncErrorKind::Conflict, "Server version changed — conflicted copy created".into());
                    smap.safe_write().remove(&remote_path);
                    journal.safe_lock().remove_discarding(seq, &tail_path);
                }
                Err(ref e) if e.is_transient() => {
                    if e.is_network_down() {
                        mark_offline(&conn.is_offline, &conn.offline_since);
                    }
                    log::warn!("streamed PUT {} deferred — {} (queued for retry)", remote_path.display(), e);
                    smap.safe_write().insert(remote_path.clone(), FileStatus::PendingSync);
                    journal.safe_lock().mark_deferred(seq, e.to_string());
                }
                Err(e) => {
                    log::error!("streamed PUT {} failed at finish: {}", remote_path.display(), e);
                    let kind = match &e {
                        backend::BackendWriteError::Forbidden => SyncErrorKind::PermissionDenied,
                        backend::BackendWriteError::QuotaExceeded => SyncErrorKind::QuotaExceeded,
                        backend::BackendWriteError::Server(code, _) => SyncErrorKind::ServerError(*code),
                        _ => SyncErrorKind::UploadFailed,
                    };
                    push_error(&elog, remote_path.clone(), kind, format!("Streamed upload could not complete — please retry the copy: {}", e));
                    smap.safe_write().insert(remote_path.clone(), FileStatus::PendingSync);
                    if matches!(e, backend::BackendWriteError::Server(404, _)) {
                        // The session is gone; nothing left to retry from.
                        conn.backend.abort_chunked_upload(&backend::ChunkedUploadSession { uploads_base: cs.uploads_base.clone() });
                        journal.safe_lock().remove_discarding(seq, &tail_path);
                    } else {
                        journal.safe_lock().mark_failed(seq, e.to_string());
                    }
                }
            }
            dirty.safe_lock().insert(remote_path.clone());
            dirty.safe_lock().insert(parent);
        });
        Ok(())
    }

    /// Uploads a handle's staged bytes once release() has removed it. FLUSH is sent on
    /// every close() of any fd sharing the handle, so only RELEASE marks the end of a file.
    fn commit_released(&self, fh: FileHandle, of: OpenFile) -> Result<(), Errno> {
        let OpenFile { remote_path, write_path, original_etag, chunk_upload, total_written, opened_gen, .. } = of;
        let Some(write_path) = write_path else { return Ok(()) };
        if let Some(chunk_state) = chunk_upload {
            return self.commit_streamed(remote_path, write_path, original_etag, opened_gen, total_written, chunk_state);
        }
        let upload_size = match std::fs::metadata(&write_path) {
            Ok(m) => m.len(),
            Err(_) => {
                log::error!("release: staging file missing at {}", write_path.display());
                return Err(Errno::EIO);
            }
        };
        log::info!("[{}] COMMIT {} size={} etag={:?}", self.log_user, remote_path.display(), upload_size, original_etag);

        // Durability: the journal fsyncs the staged bytes before it writes any
        // journal naming them (see `MutationJournal::unsynced`), off this thread,
        // so a crash can never leave an entry pointing at bytes that did not
        // reach disk.

        // Update dir_cache size synchronously so getattr returns the correct size
        // before the background PUT thread has a chance to run.
        {
            let mut c = self.cache.safe_lock();
            let parent = remote_path.parent().unwrap_or(Path::new("/")).to_path_buf();
            if let Some(dir) = c.dir_cache.get_mut(&parent) {
                let mut files = (*dir.files).clone();
                if let Some(e) = files.iter_mut().find(|e| e.path == remote_path) {
                    e.size = upload_size;
                }
                dir.files = Arc::new(files);
            }
        }

        let seq = self.journal.safe_lock().enqueue(
            mutation_journal::MutationOp::Put {
                remote_path: remote_path.clone(),
                staging_path: write_path.clone(),
                if_match_etag: original_etag.clone(),
            },
        );
        self.journal.safe_lock().supersede_uploads(&remote_path, seq);

        if !self.conn.is_offline.load(Ordering::Relaxed) {
            self.status.safe_write().insert(remote_path.clone(), FileStatus::Uploading);
            self.dirty.safe_lock().insert(remote_path.clone());
        } else {
            // Offline: the edit is saved locally and journaled; show it as pending
            // sync until connectivity returns and the queued PUT is replayed.
            self.status.safe_write().insert(remote_path.clone(), FileStatus::PendingSync);
            self.dirty.safe_lock().insert(remote_path.clone());
        }

        if !self.conn.is_offline.load(Ordering::Relaxed) {
            let conn = self.conn.clone();
            let cache = self.cache.clone();
            let dirty = self.dirty.clone();
            let open_files = self.open_files.clone();
            let elog = self.error_log.clone();
            let tmap = self.transfer_map.clone();
            let journal = self.journal.clone();
            let smap = self.status.clone();
            let auto_keep = self.auto_keep_locally_modified_files;

            // Guard this path in the uploading set so put_dir_cache doesn't
            // evict it from a concurrent PROPFIND refresh before the PUT lands.
            self.cache.safe_lock().uploading.insert(remote_path.clone(), Some(upload_size));
            let uploads = self.uploads.clone();
            let ticket = uploads.ticket_entry(&remote_path);

            submit_mutation(move || {
                ticket.wait();
                if !journal.safe_lock().claim(seq) {
                    log::debug!("PUT {} skipped — superseded or replayed", remote_path.display());
                    if !journal.safe_lock().has_pending_put(&remote_path) {
                        cache.safe_lock().uploading.remove(&remote_path);
                    }
                    return;
                }
                let original_etag = uploads.etag_for(&remote_path, opened_gen, original_etag);
                let _permit = conn.throttle.acquire();
                tmap.safe_lock().insert(remote_path.clone(), TransferProgress {
                    path: remote_path.clone(),
                    direction: TransferDirection::Upload,
                    bytes_done: 0,
                    total_bytes: upload_size,
                });
                let etag_ref = original_etag.as_deref();
                match conn.backend.put_file_from_path(&remote_path, &write_path, etag_ref) {
                    Ok(result) => {
                        tmap.safe_lock().remove(&remote_path);
                        cache.safe_lock().uploading.remove(&remote_path);
                        log::info!("PUT {} → new etag {:?}", remote_path.display(), result.new_change_token);
                        uploads.record(&remote_path, result.new_change_token.clone());
                        let new_size = upload_size;
                        {
                            let mut c = cache.safe_lock();
                            let parent = remote_path.parent().unwrap_or(Path::new("/")).to_path_buf();
                            if let Some(dir) = c.dir_cache.get_mut(&parent) {
                                let mut files = (*dir.files).clone();
                                if let Some(entry) = files.iter_mut().find(|e| e.path == remote_path) {
                                    entry.change_token = result.new_change_token.clone();
                                    entry.size = new_size;
                                    entry.modified = Some(SystemTime::now());
                                }
                                dir.files = Arc::new(files);
                                // Expire the cache so the next readdir triggers a PROPFIND
                                // and populates NC-assigned properties (permissions, fileid, owner).
                                dir.at = Instant::now() - (DIR_CACHE_TTL + Duration::from_secs(1));
                            }
                        }
                        if let Some(of) = open_files.safe_lock().get_mut(&fh.0) {
                            of.dirty = false;
                            of.original_etag = result.new_change_token.clone();
                        }
                        if auto_keep {
                            let rel = remote_path.strip_prefix("/").unwrap_or(&remote_path);
                            let keep_path = cache.safe_lock().kept_dir.join(rel);
                            let mut kept = false;
                            if let Some(parent) = keep_path.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            if std::fs::copy(&write_path, &keep_path).is_ok() {
                                cache.safe_lock().file_cache.insert(remote_path.clone(), FileCacheEntry {
                                    local_path: keep_path,
                                    remote_modified: Some(SystemTime::now()),
                                    etag: result.new_change_token,
                                    kept: true,
                                    size: upload_size,
                                });
                                smap.safe_write().insert(remote_path.clone(), FileStatus::Kept);
                                kept = true;
                            }
                            if !kept {
                                smap.safe_write().insert(remote_path.clone(), FileStatus::Synced);
                            }
                        } else {
                            smap.safe_write().insert(remote_path.clone(), FileStatus::Synced);
                        }
                        dirty.safe_lock().insert(remote_path.clone());
                        dirty.safe_lock().insert(remote_path.parent().unwrap_or(Path::new("/")).to_path_buf());
                        // The staging goes once the journal without this entry is on disk.
                        journal.safe_lock().remove_discarding(seq, &write_path);
                    }
                    Err(backend::BackendWriteError::Conflict) => {
                        tmap.safe_lock().remove(&remote_path);
                        cache.safe_lock().uploading.remove(&remote_path);
                        smap.safe_write().remove(&remote_path);
                        log::warn!("CONFLICT on PUT {} — creating conflicted copy", remote_path.display());
                        push_error(&elog, remote_path.clone(), SyncErrorKind::Conflict, "Server version changed — conflicted copy created".into());
                        let conflict_name = make_conflict_name(&remote_path);
                        match conn.backend.put_file_from_path(&conflict_name, &write_path, None) {
                            Ok(_) => log::info!("conflicted copy uploaded as {}", conflict_name.display()),
                            Err(e) => log::error!("failed to upload conflict copy: {}", e),
                        }
                        if let Some(of) = open_files.safe_lock().get_mut(&fh.0) {
                            of.dirty = false;
                        }
                        dirty.safe_lock().insert(remote_path.clone());
                        dirty.safe_lock().insert(remote_path.parent().unwrap_or(Path::new("/")).to_path_buf());
                        journal.safe_lock().remove_discarding(seq, &write_path);
                    }
                    Err(ref e) => {
                        tmap.safe_lock().remove(&remote_path);
                        cache.safe_lock().uploading.remove(&remote_path);
                        // Keep the local edit: the staging file and journal entry stay put,
                        // so the content survives and the mutation is retried. Surface it as
                        // PendingSync rather than dropping the status, so the UI shows the
                        // file is saved locally but not yet on the server.
                        smap.safe_write().insert(remote_path.clone(), FileStatus::PendingSync);
                        if e.is_transient() {
                            // Server down/overloaded/timed out or resource locked. Retry
                            // indefinitely (no attempt-budget cost) with no user-facing
                            // error — the PendingSync marker already conveys the state, and
                            // the local edit stays safely staged until the server is back.
                            //
                            // Flip offline NOW rather than waiting up to 30s for the
                            // connectivity monitor's next poll: this upload just proved the
                            // network is down, and a save is typically a burst of ops
                            // (write→flush plus read-modify-write reads). Flagging offline
                            // here makes every following op in the same save take the
                            // instant cache/journal path instead of each blocking on its own
                            // connect timeout. The monitor re-probes every 5s while offline
                            // and clears the flag (and replays the journal) once the server
                            // is back, so a brief hiccup self-heals quickly.
                            if e.is_network_down() {
                                mark_offline(&conn.is_offline, &conn.offline_since);
                            }
                            log::warn!("PUT {} deferred — {} (queued for retry)", remote_path.display(), e);
                            journal.safe_lock().mark_deferred(seq, e.to_string());
                        } else {
                            // Permanent — the server refuses this write (permission, quota,
                            // malformed) and retrying cannot help. Flag it so the user can
                            // act; still keep the local copy staged and let the journal's
                            // attempt budget decide when to give up.
                            let kind = match e {
                                backend::BackendWriteError::Forbidden => SyncErrorKind::PermissionDenied,
                                backend::BackendWriteError::QuotaExceeded => SyncErrorKind::QuotaExceeded,
                                backend::BackendWriteError::Server(code, _) => SyncErrorKind::ServerError(*code),
                                _ => SyncErrorKind::UploadFailed,
                            };
                            log::error!("PUT {} failed permanently: {}", remote_path.display(), e);
                            push_error(&elog, remote_path.clone(), kind, e.to_string());
                            journal.safe_lock().mark_failed(seq, e.to_string());
                        }
                        dirty.safe_lock().insert(remote_path.clone());
                    }
                }
            });
        }
        Ok(())
    }

}

/// Sets `path`'s size in its parent's resident listing, if any.
fn set_listed_size(c: &mut FsCache, path: &Path, size: u64) {
    let parent = path.parent().unwrap_or(Path::new("/"));
    if let Some(dir) = c.dir_cache.get_mut(parent) {
        if let Some(i) = dir.files.iter().position(|e| e.path == path) {
            if dir.files[i].size != size {
                let mut files = (*dir.files).clone();
                files[i].size = size;
                dir.files = Arc::new(files);
            }
        }
    }
}

/// Called from write() once the tail staging file has accumulated at
/// least one full CHUNK_SIZE of unsent bytes. Lazily opens the
/// chunked-upload session on the very first graduation, PUTs as many full
/// chunks as the tail currently holds (normally exactly one — FUSE writes
/// are far smaller than CHUNK_SIZE, so the tail crosses the threshold by a
/// small margin each time), and rewrites the tail file down to just the
/// leftover bytes. Must be called without `open_files` locked, and off the
/// dispatch thread: this performs network I/O (a 10 MB PUT, with retries),
/// and that mutex guards every open handle in the mount, not just this one.
/// `publish` records each chunk the server confirmed, before the tail file
/// is cut down.
///
/// On failure, returns the most recent server-confirmed session state
/// alongside the error (rather than just discarding it) — including one
/// opened by *this* call, if the MKCOL succeeded but a subsequent chunk
/// PUT then failed. The caller must store it back into `of.chunk_upload`
/// even on the error path, otherwise a freshly-opened session the caller
/// never learns the `uploads_base` of leaks server-side: release()'s
/// abandoned-session cleanup can only abort a session it knows about.
fn graduate_chunk(
    backend: &dyn backend::CloudBackend,
    remote_path: &Path,
    wp: &Path,
    mut state: Option<ChunkUploadState>,
    total_written: u64,
    publish: &mut dyn FnMut(&ChunkUploadState),
) -> Result<ChunkUploadState, (String, Option<ChunkUploadState>)> {
    loop {
        let bytes_confirmed = state.as_ref().map_or(0, |s| s.bytes_confirmed);
        let tail_len = total_written - bytes_confirmed;
        if tail_len < webdav_ops::CHUNK_SIZE as u64 {
            return state.clone().ok_or((
                "graduate_chunk called with nothing to graduate".to_string(),
                state,
            ));
        }

        if state.is_none() {
            let session = retry_chunk_write("chunked-upload open", || {
                backend.open_chunked_upload(remote_path)
            }).map_err(|e| (e.to_string(), None))?;
            state = Some(ChunkUploadState {
                uploads_base: session.uploads_base,
                next_index: 0,
                bytes_confirmed: 0,
            });
        }
        // Snapshot the session as the server last confirmed it, before
        // attempting this chunk — on failure below this is what the
        // caller needs to be able to find and abort the session later.
        let confirmed_state = state.clone();
        let s = state.as_mut().expect("just ensured Some above");

        let mut chunk = vec![0u8; webdav_ops::CHUNK_SIZE];
        {
            use std::io::Read;
            let mut f = std::fs::File::open(wp)
                .map_err(|e| (format!("staging read: {}", e), confirmed_state.clone()))?;
            f.read_exact(&mut chunk)
                .map_err(|e| (format!("staging read: {}", e), confirmed_state.clone()))?;
        }

        let session = backend::ChunkedUploadSession { uploads_base: s.uploads_base.clone() };
        let index = s.next_index;
        retry_chunk_write("chunk upload", || {
            backend.put_chunk(&session, index, chunk.clone())
        }).map_err(|e| (e.to_string(), confirmed_state.clone()))?;
        let s = state.as_mut().expect("just ensured Some above");
        s.next_index += 1;
        s.bytes_confirmed += webdav_ops::CHUNK_SIZE as u64;
        publish(s);

        shrink_tail_file(wp, webdav_ops::CHUNK_SIZE as u64)
            .map_err(|e| (format!("tail rewrite: {}", e), state.clone()))?;
    }
}
