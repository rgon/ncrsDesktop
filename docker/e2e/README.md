# End-to-end test suite

Exercises the **ncrs CLI daemon** (`ncrs`) against a **real WebDAV server that is
not Nextcloud** (Apache `mod_dav`), over an actual FUSE mount, and asserts that
**no data is lost** across create / read / update / rename / move / delete /
mkdir / rmdir and remote→local propagation.

## What it verifies

Every byte written through the mount must be retrievable, unchanged, from both
the mount **and** the backend; renames/moves must preserve content; deletes must
propagate. Integrity is checked with `sha256`. See `ncrs/scenarios.sh`.

## Run locally

```sh
scripts/e2e.sh
```

Requires Docker (with `compose`) and `/dev/fuse`. The `ncrs` container needs
`SYS_ADMIN` + the fuse device (already set in `docker-compose.yml`).

## Layout

- `webdav/` — Apache `mod_dav` image serving a DAV collection at
  `/remote.php/dav/files/testuser/` (Basic auth `testuser:testpass`).
- `ncrs/Dockerfile` — builds the headless `ncrs` binary (no Tauri GUI) and the
  test harness.
- `ncrs/entrypoint.sh` — waits for WebDAV, writes the config, mounts, runs the
  scenarios, unmounts.
- `ncrs/scenarios.sh` — the data-loss assertions.

## Why the WebDAV path looks Nextcloud-shaped

The daemon reads from the configured `url` but currently **writes** to a
hardcoded `{host}/remote.php/dav/files/{username}/` path (`webdav_ops.rs`). To
exercise the real write path against a non-Nextcloud server, the WebDAV
collection is served at that exact path and `url` points at it. Files stay under
the 10 MiB chunk threshold so uploads use a single `PUT` (Nextcloud-style
chunked assembly is not something a plain server can do).
