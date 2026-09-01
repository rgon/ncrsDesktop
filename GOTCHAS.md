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
