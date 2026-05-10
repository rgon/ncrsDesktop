"""
ncRS Nautilus extension — decorates files under the ncRS mount point with
sync-state emblems so the user can tell at a glance which files are local
(downloaded to disk) and which are still remote-only (will download on open).

Requires:
  sudo apt install python3-nautilus

Install:
  ./install.sh
  nautilus -q   # restart Nautilus

The ncRS daemon must be running; it exposes a Unix socket at
$XDG_RUNTIME_DIR/ncrs.sock (usually /run/user/<UID>/ncrs.sock).

Protocol: send "STATUS <abs-path>\\n", receive one of:
  local   — cached on disk, up to date
  synced  — cached but dir-listing freshness not confirmed
  remote  — known to exist on server, not yet downloaded
  unknown — path not under mount point or daemon hasn't seen it
"""

import os
import socket
import threading
from concurrent.futures import ThreadPoolExecutor

import gi
gi.require_version("Nautilus", "4.0")
from gi.repository import GLib, GObject, Nautilus  # noqa: E402

# ── Emblem names (standard XDG / FreeDesktop icon names) ─────────────────────
_EMBLEM_LOCAL  = "emblem-default"       # green tick
_EMBLEM_REMOTE = "emblem-downloads"     # cloud / down-arrow
_EMBLEM_SYNCED = "emblem-synchronizing" # circular arrows

SOCKET_TIMEOUT = 0.5  # seconds; keep short to avoid stalling Nautilus

# One shared pool so we don't spawn unbounded threads for large directories.
_POOL = ThreadPoolExecutor(max_workers=4, thread_name_prefix="ncrs-nautilus")


def _sock_path() -> str:
    runtime = os.environ.get("XDG_RUNTIME_DIR") or f"/run/user/{os.getuid()}"
    return os.path.join(runtime, "ncrs.sock")


def query_status(path: str, sock_path: str | None = None) -> str:
    """Query the ncRS daemon for the sync status of *path*.

    Returns 'unknown' on any error (daemon not running, timeout, etc.).
    *sock_path* is injectable for tests.
    """
    sp = sock_path or _sock_path()
    if not os.path.exists(sp):
        return "unknown"
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.settimeout(SOCKET_TIMEOUT)
            s.connect(sp)
            s.sendall(f"STATUS {path}\n".encode())
            buf = b""
            while b"\n" not in buf:
                chunk = s.recv(64)
                if not chunk:
                    break
                buf += chunk
            return buf.decode(errors="replace").strip()
    except (OSError, socket.timeout):
        return "unknown"


# ── Info provider ─────────────────────────────────────────────────────────────

class NcrsInfoProvider(GObject.GObject, Nautilus.InfoProvider):
    """Decorates files with emblems reflecting their ncRS sync state."""

    def __init__(self):
        super().__init__()
        self._cancelled: set[int] = set()
        self._lock = threading.Lock()

    # Synchronous fast path: called for items already in cache.
    # We return COMPLETE immediately without doing any I/O; the async full
    # path below is what does real work.
    def update_file_info(self, file_info):
        return Nautilus.OperationResult.COMPLETE

    # Async path: Nautilus calls this and expects IN_PROGRESS while we work,
    # then update_complete_invoke when we're done.
    def update_file_info_full(self, provider, handle, closure, file_info):
        if file_info.get_uri_scheme() != "file":
            Nautilus.info_provider_update_complete_invoke(
                closure, provider, handle, Nautilus.OperationResult.COMPLETE)
            return Nautilus.OperationResult.IN_PROGRESS

        path = file_info.get_location().get_path()
        if path is None:
            Nautilus.info_provider_update_complete_invoke(
                closure, provider, handle, Nautilus.OperationResult.COMPLETE)
            return Nautilus.OperationResult.IN_PROGRESS

        handle_id = id(handle)

        def _work():
            status = query_status(path)

            def _apply():
                # Check if Nautilus cancelled this request while we were querying.
                with self._lock:
                    if handle_id in self._cancelled:
                        self._cancelled.discard(handle_id)
                        return GLib.SOURCE_REMOVE

                if status == "local":
                    file_info.add_emblem(_EMBLEM_LOCAL)
                elif status == "remote":
                    file_info.add_emblem(_EMBLEM_REMOTE)
                elif status == "synced":
                    file_info.add_emblem(_EMBLEM_SYNCED)

                Nautilus.info_provider_update_complete_invoke(
                    closure, provider, handle, Nautilus.OperationResult.COMPLETE)
                return GLib.SOURCE_REMOVE

            GLib.idle_add(_apply)

        _POOL.submit(_work)
        return Nautilus.OperationResult.IN_PROGRESS

    def cancel_update(self, provider, handle):
        """Called by Nautilus when it no longer needs the result (e.g. window closed)."""
        with self._lock:
            self._cancelled.add(id(handle))


# ── Menu provider ─────────────────────────────────────────────────────────────

class NcrsMenuProvider(GObject.GObject, Nautilus.MenuProvider):
    """Right-click menu items for ncRS-managed files (placeholder)."""

    def get_file_items(self, *args):
        # args is (files,) in Nautilus 4; variadic to stay compatible with 3.
        return []

    def get_background_items(self, *args):
        return []
