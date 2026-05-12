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

SOCKET_TIMEOUT = 0.15  # seconds; daemon replies instantly (HashMap lookup)

# One shared pool so we don't spawn unbounded threads for large directories.
_POOL = ThreadPoolExecutor(max_workers=4, thread_name_prefix="ncrs-nautilus")


def _sock_path() -> str:
    runtime = os.environ.get("XDG_RUNTIME_DIR") or f"/run/user/{os.getuid()}"
    return os.path.join(runtime, "ncrs.sock")


def _load_mount_point(config_path: str | None = None) -> str | None:
    """Read mount_point from the ncRS config YAML (simple line parse)."""
    if config_path is None:
        config_home = os.environ.get("XDG_CONFIG_HOME") or os.path.expanduser("~/.config")
        config_path = os.path.join(config_home, "ncrs", "config.yaml")
    try:
        with open(config_path) as f:
            for line in f:
                stripped = line.strip()
                if stripped.startswith("mount_point:"):
                    val = stripped[len("mount_point:"):].strip().strip('"').strip("'")
                    if val:
                        return val.rstrip("/")
    except OSError:
        pass
    return None


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
        self._mount = _load_mount_point()

    # Synchronous fast path: called for items already in cache.
    # We return COMPLETE immediately without doing any I/O; the async full
    # path below is what does real work.
    def update_file_info(self, file_info):
        return Nautilus.OperationResult.COMPLETE

    # Async path: Nautilus calls this and expects IN_PROGRESS while we work,
    # then update_complete_invoke when we're done.
    def update_file_info_full(self, provider, handle, closure, file_info):
        if not self._mount:
            return Nautilus.OperationResult.COMPLETE

        if file_info.get_uri_scheme() != "file":
            return Nautilus.OperationResult.COMPLETE

        path = file_info.get_location().get_path()
        if path is None or not (path == self._mount or path.startswith(self._mount + "/")):
            return Nautilus.OperationResult.COMPLETE

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
                elif status == "synced":
                    file_info.add_emblem(_EMBLEM_SYNCED)
                elif status == "downloading":
                    file_info.add_emblem(_EMBLEM_REMOTE)

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


# ── IPC command helper ────────────────────────────────────────────────────────

def _send_command(cmd: str) -> str:
    sp = _sock_path()
    if not os.path.exists(sp):
        return "error: daemon not running"
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.settimeout(SOCKET_TIMEOUT)
            s.connect(sp)
            s.sendall(f"{cmd}\n".encode())
            buf = b""
            while b"\n" not in buf:
                chunk = s.recv(64)
                if not chunk:
                    break
                buf += chunk
            return buf.decode(errors="replace").strip()
    except (OSError, socket.timeout):
        return "error: socket timeout"


# ── Menu provider ─────────────────────────────────────────────────────────────

class NcrsMenuProvider(GObject.GObject, Nautilus.MenuProvider):
    """Right-click menu items for ncRS-managed files."""

    def __init__(self):
        super().__init__()
        self._mount = _load_mount_point()

    def get_file_items(self, *args):
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

        item = Nautilus.MenuItem(
            name="NcrsMenuProvider::KeepLocally",
            label="Keep Locally",
            tip="Download and keep a local copy of the selected files",
        )
        item.connect("activate", self._on_keep_locally, paths)
        return [item]

    def _on_keep_locally(self, _menu_item, paths):
        def _do():
            for path in paths:
                _send_command(f"KEEP {path}")
        _POOL.submit(_do)

    def get_background_items(self, *args):
        return []
