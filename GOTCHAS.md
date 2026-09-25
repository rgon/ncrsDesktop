# GOTCHAS & 'BYPASSES' that we had to implement to fix platform issues

Subtle platform behaviours and non-obvious design decisions that affect ncrs.

## 1. Cannot absolutely place windows in wayland

To avoid this, we create a display-sized transparent and undecorated window, with an on-click 'exit' handler on this transparent background, and draw a virtual floating window on the rightmost of the screen, to match the original Nextcloud client's behaviour.

## 2. GLib MIME detection causes per-file WebDAV downloads on directory listing

**Confirmed affects:** GLib 2.80+ (Ubuntu 24.04, Fedora 40, and later). Manifests in any
application that uses GLib for file browsing, tested with Nautilus.

### What changed in GLib 2.80

> NOTE: this may not always be the case, read this note:
> Before GLib 2.80, GLib resolved MIME types by reading a `user.xdg.mime.type` extended attribute directly from the file's inode — a metadata-only operation that never required reading file content.
> GLib 2.80 removed this xattr check. For any file whose extension is unrecognised or ambiguous, 

GLib now falls back to **magic-byte detection**: it opens the file and reads the first 16 KB to inspect the binary signature (e.g. `%PDF-` for PDF, `PK\x03\x04` for ZIP/Office, `\xFF\xD8\xFF` for JPEG, etc.).

### Why this is catastrophic on a FUSE/WebDAV mount

+ On a local filesystem the 16 KB read is a microsecond. On a WebDAV-backed FUSE mount it triggers a full HTTP round-trip to download the beginning of the file from the remote server (~280 ms over a typical home internet connection).

+ GLib's detection loop is sequential: `open(A) → read(A) → close(A) → open(B)
→ …`. Even with a fast connection, a directory containing 37 files with unusual
extensions (e.g. Spanish tax forms stored with numeric extensions like `.036`,
`.190`, `.349`) incurs 37 × 280 ms ≈ **10 seconds** of latency before Nautilus
can display the directory — even though the directory listing PROPFIND itself
completes in under 300 ms.

+ The `user.xdg.mime.type` xattr that ncrs set on virtual inodes (added in
0.1.22 to address this) has **no effect** on GLib 2.80+: the xattr codepath
was removed entirely, not just deprioritised.

### Detection signal — O_NOATIME

GLib's magic-byte path always opens files with `O_NOATIME | O_NOFOLLOW |
O_CLOEXEC | O_RDONLY`. Normal applications (text editors, media players,
scripts, backup tools) never combine `O_NOATIME` with a read-only open on a
FUSE filesystem.

Two of these flags are invisible to FUSE userspace:

- `O_CLOEXEC` — always handled by the kernel fd table; never forwarded.
- `O_NOFOLLOW` — handled at the VFS layer (the kernel rejects the open if the
  final path component is a symlink); the flag is stripped before the FUSE
  driver receives the request.

`O_NOATIME` is the only one of GLib's detection flags that survives into the
FUSE `open()` handler. It is reliable and distinctive enough to use as the
sole signal.

### The fix (implemented in ncrs)

**`open()` handler** — when a read-only, non-locally-cached file is opened with
`O_NOATIME`, ncrs looks up the `{DAV:}getcontenttype` value that was already
fetched from the server during the preceding PROPFIND (stored in the in-memory
dir cache). This lookup is instant — no network I/O. The result is attached to
the open file handle as `mime_detect_ct`. If the server did not supply a
content-type, `"application/octet-stream"` is used as a fallback so that every
GLib detection open is intercepted without exception.

**`read()` handler** — when a read arrives at offset 0 on a handle that has
`mime_detect_ct` set, ncrs returns a short synthetic byte sequence from
`mime_magic_bytes()` that matches the magic signature for that content-type.
GLib receives a valid answer, classifies the file correctly, and closes the
handle. No data is fetched from the server. The round-trip cost drops from
~280 ms to a few microseconds.

**Guard against corrupting copies** — `O_NOATIME` is also used by copy tools
(`cp`, Nautilus's `g_file_copy`) on the source file to avoid updating its
access time. Without a guard, the copy would receive magic bytes instead of
real content and write a tiny corrupted file to the destination. The
distinguishing signal is the **read size**: GLib always requests exactly 16384
bytes (`MAGIC_BYTES_BUFFER_SIZE` in `gcontenttype.c`), while copy tools use
much larger buffers (`cp` uses 131072 bytes, GIO uses 65536 bytes).

+  **Kernel read-ahead inflates the magic read — do NOT guard on `sz <= 16384`.**
GLib asks userspace-side for 16384 bytes, but the kernel enlarges the *initial*
FUSE `read` to fill its read-ahead window: measured at exactly **32768 bytes**
(one 8-page window) for the first read of any file, even a multi-GB one, because
a magic-detection open reads once and closes so the sequential read-ahead ramp
never grows. A file larger than 16 KiB therefore arrives with `sz` in
`(16384, 32768]`. Guarding on `sz <= 16384` silently rejected every such file
and downloaded it in full (~300 ms each; ~19 s for a 114-entry folder) — a
regression seen after the guard was first added. `read()` now intercepts when
`off == 0 && sz <= MIME_DETECT_MAX_READ` (**32768**), which covers the
read-ahead-inflated read while staying below the smallest copy buffer (65536),
so copies still fall through. Verify with `scripts/perf_test_listing.py`.
(In practice copy tools do not even set `O_NOATIME` on the source — confirmed
via FUSE flag logging that `cp`/`gio copy` open with `0x8000`, no `O_NOATIME` —
so the size guard is defence in depth.)

**Result:** a directory with 266 files (37 with unusual extensions) that
previously took 10–12 seconds to list in Nautilus now lists in under 100 ms.

### Forward-compatibility

`mime_magic_bytes()` covers the most common binary formats. Any type not in
the table returns `b"# text\n"`, which GLib recognises as `text/plain`. This
is an acceptable fallback: it prevents the download and gives the file a usable
icon. GLib's extension-based detection — which runs *before* magic-byte
detection — already handles the vast majority of files; this fallback is only
reached for genuinely obscure extensions.

This fix applies to all users on GLib 2.80+, regardless of which file types or
directory structures they have. It is not specific to any particular extension
set or server configuration.

### Other attempted methods that did not work

- Setting `user.xdg.mime.type` on FUSE virtual inodes — GLib 2.80 removed the
  xattr check entirely.
- Prefetching file content during `readdir()` — GLib's detection loop is
  strictly sequential (`open→read→close→open→…`); content prefetched into a
  local buffer during `readdir` is never in place before GLib's first `read()`
  arrives, and the prefetch adds overhead to every directory listing regardless
  of whether any detection occurs.
- Per-extension MIME database patches (`~/.local/share/mime/packages/*.xml`) —
  these work for the specific extensions patched, but require manual
  maintenance for every new unusual extension a user encounters, and provide
  no protection for files with no extension at all.

## 3. reqwest `http3_prior_knowledge()` does not actually send HTTP/3

In reqwest 0.13, `ClientBuilder::http3_prior_knowledge()` only *builds* the QUIC
connector. A request is routed to it solely when the request itself carries
`Version::HTTP_3`; anything else silently rides TCP — and since the h3
preference sets no ALPN on that TCP path, the "HTTP/3 client" actually speaks
HTTP/1.1. The daemon shipped that way for months: every request was h1, and the
"HTTP/3 unusable" demotions were two TCP probes racing an ordinary network blip.

All requests therefore go through `http_clients::DavClient`, which stamps
`Version::HTTP_3` while HTTP/3 is active. Never hand out a raw
`reqwest::blocking::Client` for server traffic. More h3 surprises the
construction accounts for (see `build_pair` in `lib.rs`):

- The h3 pool ignores `pool_max_idle_per_host`, and `connect_timeout` never
  reaches the QUIC connector, so neither bounds a QUIC read.
- A request's own `.timeout()` on the *blocking* client is both the async total
  deadline for the whole exchange and the bound on each blocking `read` call. It
  cannot catch a stall without also killing a healthy large body. The per-read
  stall bound is the blocking `ClientBuilder::timeout` (never handed to the async
  client; it bounds each blocking wait and resets per call, over QUIC too), so the
  read clients set it (`READ_STALL_TIMEOUT`) and range GETs carry no timeout of
  their own.
- The h3 pool keeps exactly one QUIC connection per host and client, and a
  connection's idle clock is stamped when it is *checked out*, not when the
  request ends. Parallel downloads therefore get one read client each (one per
  `read_throttle` slot, `DOWNLOAD_CONNECTIONS`), and warm connections are kept for
  just under `http3_max_idle_timeout`: a short pool timeout made every window
  redo the QUIC+TLS handshake and slow start.
- The connector binds its own UDP socket and exposes no knob for it, so its
  receive buffer stays at the kernel default and drops datagrams at speed.
  `http_clients::raise_quic_socket_buffers` finds the sockets via `/proc/self/fd`
  and enlarges them after each HTTP/3 client is built.
- No QUIC keep-alive and no 0-RTT are exposed through the blocking builder; TLS
  session resumption is rustls' default in-memory cache, per client.

HTTP/3 suitability is judged once, at mount time (`NextcloudBackend::new`):
if the startup probe fails over QUIC while the same probe answers over plain
HTTPS, the daemon demotes to HTTP/2 for the session and persists the verdict
for a week (`h3_demoted` in the cache dir). Mid-session QUIC failures are
outages, never transport verdicts — a mount whose HTTP/3 worked at startup
would see HTTP/2 fail the same way, so `check_reachability` retries once and
reports reachability without ever demoting.

`ncrs_core/examples/h3probe.rs` probes a server end-to-end (`--v3` for real
HTTP/3, sleeps to cross idle boundaries) and prints the negotiated version —
use it before blaming the server or the network.

## 4. Every file browser brings its own download storms (desktop profiles)

A remote mount looks local to the desktop, so indexers, MIME sniffers and
thumbnailers read file contents freely, and on ncrs each read is a download.
These behaviours differ per browser/toolkit, so they are handled per
*desktop profile* (`ncrs_core/src/desktop/`), never as ad-hoc special cases in
the FUSE layer. Toolkit profiles (GIO, KIO) own these behaviours and are
enabled by default iff their libraries are installed; browser profiles
(Nautilus, Dolphin, Nemo) add the emblem adapter and keep their toolkit on.

Three different reads, three different answers. None of them replaces another:

| Behaviour | GIO toolkit | KIO toolkit |
|---|---|---|
| Indexer | Tracker: synthetic `/.trackerignore` | Baloo: mount added to `exclude folders` (via `balooctl6`, fallback `baloofilerc`); only the entry ncrs added is ever removed |
| File-type probe (`desktop::sniff`) — *answered* | declared `SniffProbe`: `O_NOATIME` open **from a process with libgio loaded**, first read ≤ 32 KiB answered with magic bytes from the PROPFIND content-type (§2); `user.xdg.mime.type` xattr. Non-GLib tools that share the flag (cp, rsync, backups) always get real bytes | **Unmeasured**: no probe declared. Qt's `QMimeDatabase` does not use `O_NOATIME`; once its read signature is known it becomes a probe scoped to processes with `libKF6KIOCore`/`libQt6Core` loaded |
| Thumbnailer (`desktop::thumbguard`) — *refused* | every program in a `.thumbnailer` `Exec=` line is refused on uncached files; the server preview is fetched instead | the thumbnail worker (`kioworker …/kio/thumbnail.so`) is refused the same way (match unverified on a real session) |
| Thumbnail cache — *pre-filled* | freedesktop `normal` from server previews | `normal` + `large` (one 256 px fetch, scaled down for `normal`) |
| Atomic-write temps | `.goutputstream-*`, `.xdp-*` hidden and optionally purged | **Unmeasured:** KIO `*.part` copies (not hidden: a user's own `.part` file would be deleted) |
| Folder view settings | — | **Unmeasured:** `user.kde.fm.viewproperties` xattr has no `setxattr` handler, so Dolphin may fall back to writing `.directory` files that get uploaded |

The unmeasured cells need a strace / FUSE-debug session of Dolphin, Baloo and
the KIO thumbnailer against a *test* mount (never the live one) before any
intercept is written. A wrong sniff signal serves fake bytes to real readers
(see the `cp` false positives in §2).

The file-type probe comes from the file manager or any app itself, before a
thumbnailer is chosen, so it must be answered, never refused. Refusing it
would break type detection. It is recognised by the read signature *and* a
process condition, both required. The same process also does real reads, so
the process alone cannot decide, and unrelated tools share the signature, so
the signature alone is not enough.
