"""
ncRS Nautilus extension — decorates files under the ncRS mount point with
sync-state emblems and custom columns showing Nextcloud metadata.

Requires:
  sudo apt install python3-nautilus

Install:
  ./install.sh
  nautilus -q   # restart Nautilus

The ncRS daemon must be running; it exposes a Unix socket at
$XDG_RUNTIME_DIR/ncrs.sock (usually /run/user/<UID>/ncrs.sock).

Protocol (line-oriented over Unix socket):
  STATUS   <abs-path>  → local | synced | remote | downloading | unknown[,shared]
  DETAIL   <abs-path>  → status\\tsharing\\tpermissions\\towner\\tsize  (tab-separated)
  WEBURL   <abs-path>  → https://…  (Nextcloud web link)
  KEEP     <abs-path>  → ok
  EVICT    <abs-path>  → ok  (remove local copy, set status to remote)
  PREFETCH <abs-path>  → ok  (background PROPFIND to warm the dir cache)
  CHANGES              → tab-separated abs-paths whose status changed (drains queue)
"""

import json
import os
import socket
import subprocess
import sys
import threading
import time
import traceback
from concurrent.futures import ThreadPoolExecutor

import gi

gi.require_version("Nautilus", "4.0")
gi.require_version("Gtk", "4.0")
from gi.repository import Gio, GLib, GObject, Gtk, Nautilus  # noqa: E402

# ── Emblem names (standard XDG / FreeDesktop icon names) ─────────────────────
# See: https://flying-sheep.github.io/freedesktop-icons/
_EMBLEM_KEPT = "emblem-default"  # green tick — user-pinned file
_EMBLEM_CACHED = "emblem-generic"  # auto-downloaded file (read cache)
_EMBLEM_REMOTE = "emblem-downloads"  # cloud / down-arrow
_EMBLEM_SYNCED = "emblem-default"  # green tick — reserved for future use (synced status emits no emblem)
_EMBLEM_SHARED = "emblem-shared"  # people / shared
_EMBLEM_PARTIAL = "emblem-downloads"  # partial download (some files local)
_EMBLEM_UPLOADING = "emblem-synchronizing"  # circular arrows — upload in progress

SOCKET_TIMEOUT = 2.0  # seconds

_POOL = ThreadPoolExecutor(max_workers=16, thread_name_prefix="ncrs-nautilus")

# Nextcloud oc:permissions flag letters → human labels.
# G = readable, W = writable, C = can create, D = can delete,
# N = can rename (within parent), V = can move (to different parent),
# R = can reshare with others, S = this item has been shared,
# M = mounted (external storage or federated share), K = can lock.
_PERM_FLAGS = {
    "G": "Read",
    "W": "Write",
    "C": "Create",
    "D": "Delete",
    "N": "Rename",
    "V": "Move",
    "R": "Reshare",
    "S": "Shared",
    "M": "Mounted",
    "K": "Lock",
}


def _log_error(context: str) -> None:
    print(f"[ncrs-nautilus] {context}:", file=sys.stderr)
    traceback.print_exc(file=sys.stderr)


def _sock_path() -> str:
    runtime = os.environ.get("XDG_RUNTIME_DIR") or f"/run/user/{os.getuid()}"
    return os.path.join(runtime, "ncrs.sock")


def _load_mount_point(config_path: str | None = None) -> str | None:
    if config_path is None:
        config_home = os.environ.get("XDG_CONFIG_HOME") or os.path.expanduser(
            "~/.config"
        )
        config_path = os.path.join(config_home, "ncrs", "config.yaml")
    try:
        with open(config_path) as f:
            for line in f:
                key, sep, rest = line.strip().partition(":")
                if sep and key.strip() == "mount_point":
                    val = rest.strip().strip('"').strip("'")
                    if val:
                        return val.rstrip("/")
    except (OSError, ValueError):
        pass
    return None


class _PersistentConn:
    __slots__ = ("_sock", "_rfile")

    def __init__(self):
        self._sock = None
        self._rfile = None

    def _ensure(self):
        if self._sock is not None:
            return
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            s.settimeout(SOCKET_TIMEOUT)
            s.connect(_sock_path())
        except OSError:
            s.close()
            raise
        self._sock = s
        self._rfile = s.makefile("rb")

    def send(self, cmd: str) -> str:
        for attempt in range(2):
            try:
                self._ensure()
                self._sock.sendall(f"{cmd}\n".encode())
                line = self._rfile.readline()
                if not line:
                    raise ConnectionError("closed")
                return line.decode(errors="replace").strip()
            except (OSError, ConnectionError):
                self._close()
                if attempt > 0:
                    return "error: connection failed"
        return "error: connection failed"

    def _close(self):
        for obj in (self._rfile, self._sock):
            if obj is not None:
                try:
                    obj.close()
                except OSError:
                    pass
        self._sock = None
        self._rfile = None


_local = threading.local()


def query_status(path: str, sock_path: str | None = None) -> str:
    """Return the sync status for *path* by querying the daemon socket.

    Accepts an optional *sock_path* override for use in tests.  Returns
    ``'unknown'`` on any connection or timeout error.
    """
    effective_path = sock_path if sock_path is not None else _sock_path()
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.settimeout(SOCKET_TIMEOUT)
        s.connect(effective_path)
        with s:
            s.sendall(f"STATUS {path}\n".encode())
            line = s.makefile("rb").readline()
            if not line:
                return "unknown"
            return line.decode(errors="replace").strip()
    except (OSError, ConnectionError):
        return "unknown"


def _send_command(cmd: str) -> str:
    conn = getattr(_local, "conn", None)
    if conn is None:
        conn = _PersistentConn()
        _local.conn = conn
    return conn.send(cmd)


def _human_perms(raw: str) -> str:
    if not raw:
        return ""
    seen = set()
    parts = []
    for ch in raw:
        label = _PERM_FLAGS.get(ch)
        if label and label not in seen:
            seen.add(label)
            parts.append(label)
    return ", ".join(parts) if parts else raw


def _human_size(n: int) -> str:
    if n < 1024:
        return f"{n} B"
    for unit in ("KiB", "MiB", "GiB", "TiB"):
        n /= 1024.0
        if n < 1024:
            return f"{n:.1f} {unit}"
    return f"{n:.1f} PiB"


# ── Per-directory detail cache ───────────────────────────────────────────────
# The InfoProvider answers Nautilus synchronously from this cache so it never
# blocks the file view on a per-file socket round-trip. A cache miss paints the
# file immediately with no metadata and warms the *whole* directory with a
# single DETAILDIR query in the background, then invalidates the children so
# Nautilus repaints them from the now-populated cache.
_DIR_CACHE_TTL = 30.0  # seconds
_dir_cache: dict = {}  # dir path → {basename: (sync, sharing, perms, owner, size)}
_dir_cache_ts: dict = {}  # dir path → monotonic timestamp of last fetch
_dir_inflight: set = set()  # dirs with a fetch in progress
_cache_lock = threading.Lock()


def _parse_detaildir(resp: str) -> dict:
    result = {}
    if not resp:
        return result
    for rec in resp.split("\x1e"):
        parts = rec.split("\t")
        if len(parts) < 6:
            continue
        result[parts[0]] = (parts[1], parts[2], parts[3], parts[4], parts[5])
    return result


def _invalidate_path(path: str) -> bool:
    try:
        # Force the parent directory to re-fetch on the next request so the
        # freshly-changed file picks up its new status/metadata.
        parent = os.path.dirname(path)
        with _cache_lock:
            _dir_cache_ts.pop(parent, None)
        fi = Nautilus.FileInfo.lookup(Gio.File.new_for_path(path))
        if fi is not None:
            fi.invalidate_extension_info()
    except Exception:
        pass
    return GLib.SOURCE_REMOVE


_POLL_KEEP_ATTEMPTS: dict = {}
_keep_poll_active: set = set()


def _do_check_keep_done(path: str) -> None:
    """Pool thread: send STATUS; stop polling when download finishes."""
    try:
        status = _send_command(f"STATUS {path}")
        if status.split(",")[0] != "downloading":
            _keep_poll_active.discard(path)
            _POLL_KEEP_ATTEMPTS.pop(path, None)
            GLib.idle_add(_invalidate_path, path)
    except Exception:
        _log_error(f"_do_check_keep_done({path})")
        _keep_poll_active.discard(path)
        _POLL_KEEP_ATTEMPTS.pop(path, None)


def _check_keep_done(path: str) -> bool:
    """GTK main-thread timer — submits blocking STATUS to the pool, no I/O here."""
    try:
        _POLL_KEEP_ATTEMPTS[path] = _POLL_KEEP_ATTEMPTS.get(path, 0) + 1
        if _POLL_KEEP_ATTEMPTS[path] > 240 or path not in _keep_poll_active:
            _keep_poll_active.discard(path)
            _POLL_KEEP_ATTEMPTS.pop(path, None)
            return GLib.SOURCE_REMOVE
        _POOL.submit(_do_check_keep_done, path)
    except Exception:
        _log_error(f"_check_keep_done({path})")
        _keep_poll_active.discard(path)
        _POLL_KEEP_ATTEMPTS.pop(path, None)
        return GLib.SOURCE_REMOVE
    return GLib.SOURCE_CONTINUE


def _poll_keep_done(path: str) -> None:
    try:
        if path in _keep_poll_active:
            return
        _keep_poll_active.add(path)
        _POLL_KEEP_ATTEMPTS[path] = 0
        GLib.timeout_add(500, _check_keep_done, path)
    except Exception:
        _log_error(f"_poll_keep_done({path})")


# ── Column provider ──────────────────────────────────────────────────────────


class NcrsColumnProvider(GObject.GObject, Nautilus.ColumnProvider):
    def get_columns(self):
        try:
            return [
                Nautilus.Column(
                    name="NcrsExtension::sync_status",
                    attribute="ncrs_sync",
                    label="Sync",
                    description="ncRS sync status",
                ),
                Nautilus.Column(
                    name="NcrsExtension::sharing",
                    attribute="ncrs_sharing",
                    label="Sharing",
                    description="Nextcloud sharing status",
                ),
                Nautilus.Column(
                    name="NcrsExtension::permissions",
                    attribute="ncrs_permissions",
                    label="NC Permissions",
                    description="Nextcloud permission flags",
                ),
                Nautilus.Column(
                    name="NcrsExtension::owner",
                    attribute="ncrs_owner",
                    label="NC Owner",
                    description="File owner on Nextcloud",
                ),
                Nautilus.Column(
                    name="NcrsExtension::nc_size",
                    attribute="ncrs_size",
                    label="NC Size",
                    description="Size on Nextcloud (includes folder contents)",
                ),
            ]
        except Exception:
            _log_error("get_columns")
            return []


# ── Info provider ─────────────────────────────────────────────────────────────

_SYNC_LABELS = {
    "kept": "Kept locally",
    "cached": "Cached",
    "local": "Local",
    "synced": "Synced",
    "remote": "Remote",
    "downloading": "Downloading",
    "uploading": "Uploading",
    "partial": "Partial",
    "unknown": "",
}


class NcrsInfoProvider(GObject.GObject, Nautilus.InfoProvider):
    def __init__(self):
        super().__init__()
        self._mount = _load_mount_point()
        self._poll_running = False
        self._poll_skip = 0  # 2-second ticks to skip when idle (adaptive backoff)
        GLib.timeout_add_seconds(2, self._poll_changes)

    def _poll_changes(self) -> bool:
        if self._poll_skip > 0:
            self._poll_skip = max(0, self._poll_skip - 1)
            return True
        if self._poll_running:
            return True
        self._poll_running = True
        try:
            _POOL.submit(self._do_poll_changes)
        except Exception:
            self._poll_running = False
            _log_error("_poll_changes submit")
        return True

    def _do_poll_changes(self):
        had_changes = False
        try:
            # Targeted VFS ops to generate kernel inotify events
            fc_resp = _send_command("FILE_CHANGES")
            if fc_resp and not fc_resp.startswith("error"):
                entries = [e for e in fc_resp.split("\t") if ":" in e]
                if entries:
                    had_changes = True
                for entry in entries:
                    kind, path = entry.split(":", 1)
                    try:
                        if kind == "A":
                            fd = os.open(path, os.O_CREAT | os.O_WRONLY, 0o600)
                            os.close(fd)
                        elif kind == "D":
                            os.unlink(path)
                        elif kind == "M":
                            # os.utime() on a FUSE path triggers a WebDAV PROPPATCH
                            # via the kernel→FUSE→daemon chain; signal Nautilus instead.
                            GLib.idle_add(_invalidate_path, path)
                        elif kind == "DA":
                            os.mkdir(path, 0o755)
                        elif kind == "DD":
                            os.rmdir(path)
                        elif kind == "R":
                            old_path, new_path = path.split("\x1e", 1)
                            os.rename(old_path, new_path)
                    except OSError:
                        pass
                    except ValueError:
                        _log_error(f"malformed FILE_CHANGES entry: {entry!r}")

            # Overlay icon refresh
            resp = _send_command("CHANGES")
            if not resp or resp.startswith("error"):
                return
            paths = [p for p in resp.split("\t") if p]
            if paths:
                had_changes = True
                # Force each changed file's directory to re-fetch so the stale
                # cached record is replaced on the next request.
                with _cache_lock:
                    for p in paths:
                        _dir_cache_ts.pop(os.path.dirname(p), None)

            def _invalidate():
                for p in paths:
                    try:
                        fi = Nautilus.FileInfo.lookup(Gio.File.new_for_path(p))
                        if fi is not None:
                            fi.invalidate_extension_info()
                    except Exception:
                        pass
                return GLib.SOURCE_REMOVE

            GLib.idle_add(_invalidate)
        except Exception:
            _log_error("_poll_changes")
        finally:
            # Set _poll_skip before clearing _poll_running so the GLib timer
            # cannot observe _poll_running=False with a stale _poll_skip value.
            self._poll_skip = 0 if had_changes else min(self._poll_skip + 1, 4)
            self._poll_running = False

    def update_file_info_full(self, provider, handle, closure, file_info):
        # Answer synchronously (return COMPLETE, never call the async
        # update_complete_invoke — that is only for IN_PROGRESS results and
        # calling it on a synchronous return triggers Nautilus's "Unexpected
        # plugin response: handle=(nil)" and drops the completion).
        try:
            if not self._mount:
                return Nautilus.OperationResult.COMPLETE

            if file_info.get_uri_scheme() != "file":
                return Nautilus.OperationResult.COMPLETE

            path = file_info.get_location().get_path()
            if path is None or not (
                path == self._mount or path.startswith(self._mount + "/")
            ):
                return Nautilus.OperationResult.COMPLETE

            parent = os.path.dirname(path)
            name = os.path.basename(path)
            ent = None
            fresh = False
            with _cache_lock:
                entry = _dir_cache.get(parent)
                fresh = (time.monotonic() - _dir_cache_ts.get(parent, 0.0)) < _DIR_CACHE_TTL
                if entry is not None:
                    ent = entry.get(name)

            if ent is not None:
                self._apply_detail(file_info, ent)

            # Warm (or refresh) the whole directory in the background on a miss
            # or once the cache has gone stale; the fetch invalidates the
            # children so Nautilus repaints them from the populated cache.
            if ent is None or not fresh:
                self._ensure_dir_fetch(parent)

            return Nautilus.OperationResult.COMPLETE
        except Exception:
            _log_error("update_file_info_full")
            return Nautilus.OperationResult.FAILED

    def cancel_update(self, provider, handle):
        # We answer synchronously, so there is never an outstanding async
        # operation to cancel. Present so nautilus-python does not warn.
        pass

    def _apply_detail(self, file_info, ent):
        sync, sharing, perms, owner, size_str = ent
        try:
            if sync == "kept":
                file_info.add_emblem(_EMBLEM_KEPT)
            elif sync == "cached":
                file_info.add_emblem(_EMBLEM_CACHED)
            elif sync == "local":
                file_info.add_emblem(_EMBLEM_KEPT)
            elif sync == "downloading":
                file_info.add_emblem(_EMBLEM_REMOTE)
            elif sync == "uploading":
                file_info.add_emblem(_EMBLEM_UPLOADING)
            elif sync == "partial":
                file_info.add_emblem(_EMBLEM_PARTIAL)
            if sharing:
                file_info.add_emblem(_EMBLEM_SHARED)

            file_info.add_string_attribute("ncrs_sync", _SYNC_LABELS.get(sync, ""))
            file_info.add_string_attribute("ncrs_sharing", sharing)
            file_info.add_string_attribute("ncrs_permissions", _human_perms(perms))
            file_info.add_string_attribute("ncrs_owner", owner)
            try:
                size_val = int(size_str)
                file_info.add_string_attribute(
                    "ncrs_size", _human_size(size_val) if size_val > 0 else ""
                )
            except ValueError:
                file_info.add_string_attribute("ncrs_size", "")
        except Exception:
            _log_error("_apply_detail")

    def _ensure_dir_fetch(self, parent):
        with _cache_lock:
            if parent in _dir_inflight:
                return
            fresh = (time.monotonic() - _dir_cache_ts.get(parent, 0.0)) < _DIR_CACHE_TTL
            if fresh and parent in _dir_cache:
                return
            _dir_inflight.add(parent)
        try:
            _POOL.submit(self._do_dir_fetch, parent)
        except Exception:
            with _cache_lock:
                _dir_inflight.discard(parent)
            _log_error("_ensure_dir_fetch submit")

    def _do_dir_fetch(self, parent):
        try:
            resp = _send_command(f"DETAILDIR {parent}")
            if resp.startswith("error"):
                return
            parsed = _parse_detaildir(resp)
            with _cache_lock:
                _dir_cache[parent] = parsed
                _dir_cache_ts[parent] = time.monotonic()

            def _invalidate_children():
                for child_name in parsed:
                    try:
                        child = os.path.join(parent, child_name)
                        fi = Nautilus.FileInfo.lookup(Gio.File.new_for_path(child))
                        if fi is not None:
                            fi.invalidate_extension_info()
                    except Exception:
                        pass
                return GLib.SOURCE_REMOVE

            GLib.idle_add(_invalidate_children)
        except Exception:
            _log_error(f"_do_dir_fetch({parent})")
        finally:
            with _cache_lock:
                _dir_inflight.discard(parent)


# ── Menu provider ─────────────────────────────────────────────────────────────


class NcrsMenuProvider(GObject.GObject, Nautilus.MenuProvider):
    def __init__(self):
        super().__init__()
        self._mount = _load_mount_point()

    def get_file_items(self, *args):
        try:
            files = args[-1] if args else []
            if not self._mount:
                return []

            paths = []
            for f in files:
                if f.get_uri_scheme() != "file":
                    continue
                path = f.get_location().get_path()
                if path and (path == self._mount or path.startswith(self._mount + "/")):
                    paths.append(path)

            if not paths:
                return []

            # Both items are always shown — no per-file STATUS queries on the
            # main thread. Keep/Evict are idempotent on the daemon side.
            items = []
            keep = Nautilus.MenuItem(
                name="NcrsMenuProvider::KeepLocally",
                label="Keep Locally",
                tip="Download and keep a local copy of the selected files",
            )
            keep.connect("activate", self._on_keep_locally, paths)
            items.append(keep)

            evict = Nautilus.MenuItem(
                name="NcrsMenuProvider::EvictLocally",
                label="Don't Keep Locally",
                tip="Remove the local copy and free disk space",
            )
            evict.connect("activate", self._on_evict_locally, paths)
            items.append(evict)

            view_web = Nautilus.MenuItem(
                name="NcrsMenuProvider::ViewInWeb",
                label="View in Nextcloud Web",
                tip="Open this file in the Nextcloud web interface",
            )
            view_web.connect("activate", self._on_view_in_web, paths)
            items.append(view_web)

            return items
        except Exception:
            _log_error("get_file_items")
            return []

    def _on_view_in_web(self, _menu_item, paths):
        def _do():
            try:
                for path in paths:
                    url = _send_command(f"WEBURL {path}")
                    if url.startswith("http"):
                        subprocess.Popen(
                            ["xdg-open", url],
                            stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL,
                        )
            except Exception:
                _log_error("_on_view_in_web")

        _POOL.submit(_do)

    def _on_keep_locally(self, _menu_item, paths):
        def _do():
            try:
                for path in paths:
                    _send_command(f"KEEP {path}")
                for path in paths:
                    GLib.idle_add(_invalidate_path, path)
                for path in paths:
                    _POOL.submit(_poll_keep_done, path)
            except Exception:
                _log_error("_on_keep_locally")

        _POOL.submit(_do)

    def _on_evict_locally(self, _menu_item, paths):
        def _do():
            try:
                for path in paths:
                    _send_command(f"EVICT {path}")
                for path in paths:
                    GLib.idle_add(_invalidate_path, path)
            except Exception:
                _log_error("_on_evict_locally")

        _POOL.submit(_do)

    def get_background_items(self, *args):
        try:
            if not self._mount:
                return []
            search_item = Nautilus.MenuItem(
                name="NcrsMenuProvider::SearchNextcloud",
                label="Search Nextcloud...",
                tip="Search files across your Nextcloud instance",
            )
            search_item.connect("activate", self._on_search)
            return [search_item]
        except Exception:
            _log_error("get_background_items")
            return []

    def _on_search(self, _menu_item, *_args):
        GLib.idle_add(self._show_search_dialog)

    def _show_search_dialog(self):
        try:
            dialog = _SearchDialog()
            dialog.present()
        except Exception:
            _log_error("_show_search_dialog")
        return GLib.SOURCE_REMOVE


class _SearchDialog(Gtk.Window):
    def __init__(self):
        super().__init__(
            title="Search Nextcloud", default_width=600, default_height=450
        )

        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        box.set_margin_top(12)
        box.set_margin_bottom(12)
        box.set_margin_start(12)
        box.set_margin_end(12)
        self.set_child(box)

        hbox = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        self._entry = Gtk.Entry()
        self._entry.set_hexpand(True)
        self._entry.set_placeholder_text("Search term...")
        self._entry.connect("activate", self._on_search)
        hbox.append(self._entry)

        btn = Gtk.Button(label="Search")
        btn.connect("clicked", self._on_search)
        hbox.append(btn)
        box.append(hbox)

        self._spinner = Gtk.Spinner()
        box.append(self._spinner)

        scroll = Gtk.ScrolledWindow()
        scroll.set_vexpand(True)
        self._results_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
        scroll.set_child(self._results_box)
        box.append(scroll)

    def _on_search(self, _widget):
        term = self._entry.get_text().strip()
        if not term:
            return
        self._spinner.start()
        self._clear_results()
        _POOL.submit(self._do_search, term)

    def _clear_results(self):
        while True:
            child = self._results_box.get_first_child()
            if child is None:
                break
            self._results_box.remove(child)

    def _do_search(self, term):
        try:
            resp = _send_command(f"SEARCH {term}")
            if resp.startswith("error"):
                GLib.idle_add(self._show_error, resp)
                return
            groups = json.loads(resp)
            GLib.idle_add(self._show_results, groups)
        except Exception:
            _log_error(f"_do_search({term})")
            GLib.idle_add(self._show_error, "Search failed")

    def _show_error(self, msg):
        self._spinner.stop()
        label = Gtk.Label(label=msg)
        label.set_halign(Gtk.Align.START)
        self._results_box.append(label)
        return GLib.SOURCE_REMOVE

    def _show_results(self, groups):
        self._spinner.stop()
        self._clear_results()
        if not groups:
            label = Gtk.Label(label="No results found.")
            label.set_halign(Gtk.Align.START)
            self._results_box.append(label)
            return GLib.SOURCE_REMOVE
        for group in groups:
            header = Gtk.Label()
            header.set_markup(
                f"<b>{GLib.markup_escape_text(group.get('provider_name', ''))}</b>"
            )
            header.set_halign(Gtk.Align.START)
            self._results_box.append(header)
            for entry in group.get("entries", []):
                row = self._make_result_row(entry)
                self._results_box.append(row)
        return GLib.SOURCE_REMOVE

    def _make_result_row(self, entry):
        btn = Gtk.Button()
        btn.set_has_frame(False)
        hbox = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        vbox = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        title = Gtk.Label(label=entry.get("title", ""))
        title.set_halign(Gtk.Align.START)
        title.set_ellipsize(3)  # PANGO_ELLIPSIZE_END
        vbox.append(title)
        subline = entry.get("subline", "")
        if subline:
            sub = Gtk.Label(label=subline)
            sub.set_halign(Gtk.Align.START)
            sub.set_ellipsize(3)
            sub.add_css_class("dim-label")
            vbox.append(sub)
        hbox.append(vbox)
        btn.set_child(hbox)
        url = entry.get("resource_url", "")
        if url:
            btn.connect("clicked", self._on_open_url, url)
        return btn

    @staticmethod
    def _on_open_url(_btn, url):
        try:
            subprocess.Popen(
                ["xdg-open", url],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        except Exception:
            _log_error(f"_on_open_url({url})")
