# End-to-end test suite

Exercises the **ncrs CLI daemon** (`ncrs`) against a **real WebDAV server that is
not Nextcloud** (Apache `mod_dav`), over an actual FUSE mount, and asserts that
**no data is lost** across create / read / update / rename / move / delete /
mkdir / rmdir and remote→local propagation.

## What it verifies

Every byte written through the mount must be retrievable, unchanged, from both
the mount **and** the backend; renames/moves must preserve content; deletes must
propagate. Integrity is checked with `sha256`. See `ncrs/scenarios.sh`.

One scenario covers the **directory-listing freshness window**
(`dir_cache_max_stale_mins`, set to 1 minute in this suite): three directories are
cached, aged past the window, then listed exactly once each to assert that a
directory changed on the server is already correct on the **first** listing (not
the second, which scenario 18 covers), that an unchanged one is confirmed by a
single ETag probe instead of a full re-list, and that an aged listing is still
served from cache — not turned into an error — while the server is unreachable.

The final scenario also guards a **performance regression**: GLib content-type
sniffing (an `O_NOATIME` read of the first ~16 KiB) must be answered with
synthetic magic bytes rather than downloading the whole file, while ordinary
reads and copies still receive true content. It probes in the read-ahead
window (a 24 KiB `O_NOATIME` read) so it fails if the intercept guard is ever
tightened below the kernel read-ahead size. The matching live/manual check is
`scripts/perf_test_listing.py <mounted-dir>`.

## Run locally

```sh
scripts/e2e.sh
```

Requires Docker (with `compose`) and `/dev/fuse`. The `ncrs` container needs
`SYS_ADMIN` + the fuse device (already set in `docker-compose.yml`).

## Layout

- `webdav` service (in `docker-compose.yml`) — `rclone serve webdav` (a real
  WebDAV server, **not** Nextcloud) serving a collection at
  `/remote.php/dav/files/testuser/` with Basic auth `testuser:testpass`.
- `ncrs/Dockerfile` — builds the headless `ncrs` binary (no Tauri GUI) and the
  test harness.
- `ncrs/entrypoint.sh` — waits for WebDAV, writes the config, mounts, runs the
  scenarios, unmounts.
- `ncrs/scenarios.sh` — the data-loss assertions.

## Environment notes

- **Why the WebDAV path looks Nextcloud-shaped.** The daemon reads from the
  configured `url` but currently **writes** to a hardcoded
  `{host}/remote.php/dav/files/{username}/` path (`webdav_ops.rs`). So the
  WebDAV collection is served at that exact path (via rclone's `--baseurl`) and
  `url` points at it.
- **Why rclone and not Apache `mod_dav`.** The daemon sends `If-Match` on
  overwrites for optimistic concurrency. Apache marks a just-modified file's
  ETag *weak* for one second, and weak ETags fail `If-Match` (412), so overwrites
  land as conflict copies instead of clean updates. rclone returns strong ETags
  immediately — the same contract Nextcloud provides.
- **File sizes stay under the 10 MiB chunk threshold** so uploads use a single
  `PUT`; Nextcloud-style chunked assembly is not something a plain server does.
- **Uploads are asynchronous** (PUT on close, in a background thread) and a
  not-kept file is re-downloaded on read, so the scenarios poll both the mount
  and the backend to a timeout — the guarantee is durability, not first-read
  consistency.
