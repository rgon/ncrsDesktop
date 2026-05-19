# Right-Click "Backup in Nextcloud" — Design Document

**Status:** Draft
**Date:** 2026-05-19

---

## 1. Problem Statement

Users want to back up arbitrary local files (outside the FUSE mount) to
Nextcloud with a single right-click. Currently, the Nautilus extension only
operates on files *inside* the mount point (keep/evict/web URL). There is no
way to push external files into Nextcloud from the file manager.

### Goal

Add a "Backup in Nextcloud" right-click menu entry that appears for files and
folders *outside* the ncrs mount point, uploads them to a configurable backup
directory on the server, and shows progress via the existing transfer tracking.

---

## 2. User Experience

### Right-Click Flow

1. User right-clicks a file or folder anywhere on the local filesystem
   (outside the ncrs mount point)
2. Context menu shows **"Backup in Nextcloud"** with the Nextcloud icon
3. User clicks — file starts uploading immediately
4. Nautilus emblem changes to syncing indicator (if supported outside mount)
5. Desktop notification on completion: "Backed up report.pdf to Nextcloud"
6. On error: notification with reason ("Upload failed: insufficient storage")

### Folder Behavior

- Selecting a folder backs up the entire subtree recursively
- Directory structure is preserved under the backup destination
- Example: backing up `~/Documents/taxes/` with backup root `/Backups` creates
  `/Backups/taxes/2024-return.pdf`, `/Backups/taxes/receipts/...`, etc.

### Conflict Handling

- If a file with the same name exists at the destination, overwrite it
  (backup semantics — latest version wins)
- Alternatively, append timestamp: `report_2026-05-19T14-30-00.pdf`
  (configurable via `backup_conflict` option)

---

## 3. Architecture

### Components

```
Nautilus extension (syncstate.py)
  |
  | IPC: BACKUP <local_path>
  v
ncrs daemon (ipc.rs)
  |
  | spawns upload thread
  v
webdav_ops.rs (PUT / MKCOL + chunked upload)
  |
  v
Nextcloud server (/remote.php/dav/files/<user>/Backups/...)
```

### Why Use the Daemon

Uploading via the existing daemon (rather than a standalone script) gives us:
- Reuse of authenticated HTTP client and credentials
- Transfer progress tracking via existing `TRANSFERS` IPC command
- Error reporting via existing `ERRORS` IPC command
- Chunked upload support for large files (already implemented)
- Single point of credential management

---

## 4. Configuration

```yaml
# In config.yaml:
backup_remote_dir: "/Backups"           # Remote directory for backups (default: /Backups)
backup_conflict: "overwrite"            # "overwrite" or "timestamp" (default: overwrite)
backup_max_file_size_mb: 1024           # Skip files larger than this (default: 1024 MB)
```

```rust
// In MountOptions:
pub backup_remote_dir: String,          // default "/Backups"
pub backup_conflict: BackupConflict,    // enum { Overwrite, Timestamp }
pub backup_max_file_size_mb: u64,       // default 1024
```

---

## 5. Implementation Plan

### Step 1: IPC Protocol — `BACKUP` Command

**File:** `ncrs_core/src/ipc.rs`

Add a new IPC command:

```
Request:  BACKUP /home/user/Documents/report.pdf
Response: ok <transfer_id>
      or: error: <reason>
```

The command:
1. Validates the path exists and is readable
2. Checks file size against `backup_max_file_size_mb`
3. Computes the remote destination path:
   - File: `{backup_remote_dir}/{filename}` (or with timestamp suffix)
   - Directory: `{backup_remote_dir}/{dirname}/...` (recursive)
4. Spawns a background thread for the upload
5. Returns immediately with a transfer ID

For directories, the command walks the tree and enqueues each file. The
transfer ID covers the entire batch.

**Handler pseudocode:**
```rust
"BACKUP" => {
    let local_path = PathBuf::from(args);
    if !local_path.exists() {
        return "error: path does not exist";
    }
    let metadata = std::fs::metadata(&local_path)?;
    if metadata.is_file() && metadata.len() > max_size {
        return "error: file exceeds backup_max_file_size_mb";
    }

    let transfer_id = generate_transfer_id();
    let remote_base = config.backup_remote_dir.clone();

    // Spawn upload thread
    thread::spawn(move || {
        if metadata.is_dir() {
            backup_directory(&local_path, &remote_base, ...);
        } else {
            backup_file(&local_path, &remote_base, ...);
        }
    });

    format!("ok {}", transfer_id)
}
```

### Step 2: Backup Upload Logic

**File (new):** `ncrs_core/src/backup.rs`

```rust
pub fn backup_file(
    client: &reqwest::blocking::Client,
    base_url: &str,
    username: &str,
    password: &str,
    local_path: &Path,
    remote_dir: &str,
    conflict: BackupConflict,
    transfer_progress: Arc<Mutex<TransferMap>>,
) -> Result<(), String>
```

Flow:
1. Read local file into memory (or stream for large files)
2. Compute remote path: `{remote_dir}/{filename}` or with timestamp
3. Ensure remote directory exists via `webdav_ops::mkcol()`
4. Upload via `webdav_ops::put_file_chunked()` (handles chunking automatically)
5. Update transfer progress throughout
6. On completion, send desktop notification via `notify-send` or libnotify

For directories:
1. Walk the local directory tree with `walkdir` or `std::fs::read_dir` recursive
2. Create remote directory structure via `mkcol()` for each subdirectory
3. Upload each file, tracking progress for the batch
4. Report summary on completion ("Backed up 42 files, 1.2 GB")

### Step 3: Nautilus Extension — Menu Item for External Files

**File:** `shell_integration/nautilus/syncstate.py`

Modify `NcrsMenuProvider.get_file_items()`:

Currently, the extension only adds menu items for files under the mount point.
Add a branch for files *outside* the mount point:

```python
def get_file_items(self, files):
    paths = [_uri_to_path(f.get_uri()) for f in files]

    # Existing logic for files inside mount point
    inside = [p for p in paths if p and p.startswith(self._mount)]
    outside = [p for p in paths if p and not p.startswith(self._mount)]

    items = []

    if inside:
        # ... existing keep/evict/web URL logic ...
        pass

    if outside:
        item = Nautilus.MenuItem(
            name="NcrsMenuProvider::backup",
            label="Backup in Nextcloud",
            tip="Upload selected files to Nextcloud backup folder",
            icon="folder-remote",  # or "network-server" / custom icon
        )
        item.connect("activate", self._on_backup, outside)
        items.append(item)

    return items
```

Callback:
```python
def _on_backup(self, menu, paths):
    def _do_backup():
        for path in paths:
            resp = _send_ipc(f"BACKUP {path}")
            if resp and resp.startswith("error"):
                _notify(f"Backup failed: {resp}", urgency="critical")
    _POOL.submit(_do_backup)
```

### Step 4: Desktop Notifications

**File:** `ncrs_core/src/backup.rs` (or reuse existing notification mechanism)

On backup completion, the daemon should emit a notification. Options:

1. **Via IPC event queue** — add backup completion events to a new
   `BACKUP_EVENTS` queue that the Nautilus extension polls and surfaces
   via `Gio.Notification` or `notify-send`

2. **Direct from daemon** — use `notify-rust` crate or shell out to
   `notify-send` from the Rust daemon

Option 1 is preferred for consistency with existing architecture (Nautilus
already polls `CHANGES` and `ERRORS`).

### Step 5: Progress Tracking

Reuse the existing `TRANSFERS` IPC mechanism:

```rust
// In the backup thread, register transfer:
transfer_map.safe_lock().insert(transfer_id, TransferProgress {
    path: remote_path.clone(),
    direction: TransferDirection::Upload,
    bytes_transferred: 0,
    bytes_total: file_size,
    started_at: Instant::now(),
});
```

The Nautilus extension (or GUI) can poll `TRANSFERS` to show progress. The
existing GUI `TransferView` component would automatically pick up backup
transfers.

---

## 6. New IPC Commands Summary

| Command | Format | Response |
|---------|--------|----------|
| `BACKUP <path>` | `BACKUP /home/user/file.pdf` | `ok <transfer_id>` or `error: <reason>` |
| `BACKUP_STATUS` | `BACKUP_STATUS` | JSON array of recent backup results (last 50) |

---

## 7. File Changes Summary

| File | Change |
|------|--------|
| `ncrs_core/src/backup.rs` | **NEW** — backup_file, backup_directory logic |
| `ncrs_core/src/ipc.rs` | Add BACKUP + BACKUP_STATUS command handlers |
| `ncrs_core/src/config.rs` | Add backup_remote_dir, backup_conflict, backup_max_file_size_mb |
| `ncrs_core/src/lib.rs` | Add `pub mod backup;`, pass config to IPC server |
| `shell_integration/nautilus/syncstate.py` | Add "Backup in Nextcloud" menu item for external files |
| `shell_integration/nautilus/test_syncstate.py` | Tests for backup menu visibility logic |

---

## 8. Edge Cases

| Scenario | Behavior |
|----------|----------|
| File deleted during upload | Report error, clean up transfer entry |
| No network connectivity | Queue in mutation journal for retry (or fail immediately with notification) |
| Backup dir doesn't exist on server | Auto-create via MKCOL before first upload |
| Symlinks | Follow symlinks for files, skip for directories (avoid loops) |
| Permission denied on local file | Report error per-file, continue batch |
| Duplicate filename with `overwrite` | PUT replaces existing file |
| Duplicate filename with `timestamp` | Append `_2026-05-19T14-30-00` before extension |
| File > backup_max_file_size_mb | Skip with warning notification |
| Mount point not active | Return error — daemon must be running |

---

## 9. Security Considerations

- The daemon already holds credentials; no new credential flow needed
- Local file paths sent via IPC are validated (must exist, must be readable)
- Remote paths are sanitized via existing `dav_url()` percent-encoding
- No arbitrary command execution — only file read + WebDAV upload

---

## 10. Testing Strategy

### Unit Tests
- Config parsing: backup_remote_dir, backup_conflict, backup_max_file_size_mb defaults
- Remote path computation: filename, with timestamp, nested directory structure
- Size limit enforcement

### Integration Tests
- IPC BACKUP command with mock filesystem and mock HTTP backend
- Directory traversal produces correct MKCOL + PUT sequence
- Transfer progress updates correctly during upload
- Error handling: missing file, permission denied, server error

### Nautilus Extension Tests
- Menu item appears only for files outside mount point
- Menu item does NOT appear for files inside mount point (existing items shown instead)
- Backup callback sends correct IPC command

### Manual Verification
- Right-click file outside mount -> "Backup in Nextcloud" appears
- Right-click file inside mount -> existing keep/evict items shown (no backup)
- Upload a 5 MB file, verify it appears at `/Backups/filename` on server
- Upload a folder, verify directory structure preserved
- Upload file > max size, verify rejection notification
- Upload while offline, verify error handling

---

## 11. Estimated Effort

| Component | Hours |
|-----------|-------|
| Config fields + parsing | 1 |
| `backup.rs` — single file upload | 3 |
| `backup.rs` — directory recursive upload | 3 |
| IPC BACKUP command handler | 2 |
| Transfer progress integration | 1 |
| Notification on completion | 1 |
| Nautilus menu item + callback | 2 |
| Tests (unit + integration + nautilus) | 4 |
| **Total** | **~17 hours** |

---

## 12. Future Extensions

- **Scheduled backups** — cron-like config to back up specific local directories
  on a schedule
- **Incremental backup** — integrate with Phase 1 upload dedup to skip unchanged
  files on repeated backups
- **Backup history** — track what was backed up and when, allow restoring from
  backup
- **Selective restore** — right-click a backed-up file in the mount to restore
  to original local location
- **Encryption** — client-side encrypt before upload for sensitive backups
