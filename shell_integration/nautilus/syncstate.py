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
  STATUS <abs-path>  → local | synced | remote | downloading | unknown[,shared]
  DETAIL <abs-path>  → status\\tsharing\\tpermissions\\towner\\tsize  (tab-separated)
  WEBURL <abs-path>  → https://…  (Nextcloud web link)
  KEEP   <abs-path>  → ok
  CHANGES            → tab-separated abs-paths whose status changed (drains queue)
"""

import os
import socket
import sys
import threading
import time
import traceback
from concurrent.futures import ThreadPoolExecutor

import gi
gi.require_version("Nautilus", "4.0")
from gi.repository import Gio, GLib, GObject, Nautilus  # noqa: E402

# ── Emblem names (standard XDG / FreeDesktop icon names) ─────────────────────
_EMBLEM_LOCAL  = "emblem-default"       # green tick
_EMBLEM_REMOTE = "emblem-downloads"     # cloud / down-arrow
_EMBLEM_SYNCED = "emblem-synchronizing" # circular arrows
_EMBLEM_SHARED = "emblem-shared"        # people / shared

SOCKET_TIMEOUT = 0.5  # seconds
_MAX_RECV = 4096

_POOL = ThreadPoolExecutor(max_workers=4, thread_name_prefix="ncrs-nautilus")

_PERM_FLAGS = {
    "R": "Read",
    "G": "Read",
    "W": "Write",
    "C": "Create",
    "D": "Delete",
    "N": "Rename/Move",
    "V": "Move",
    "M": "Modify",
    "S": "Share",
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
    except (OSError, ValueError):
        pass
    return None


def _send_command(cmd: str) -> str:
    sp = _sock_path()
    if not os.path.exists(sp):
        return "error: daemon not running"
    t0 = time.monotonic()
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.settimeout(SOCKET_TIMEOUT)
            s.connect(sp)
            s.sendall(f"{cmd}\n".encode())
            buf = b""
            while b"\n" not in buf and len(buf) < _MAX_RECV:
                chunk = s.recv(64)
                if not chunk:
                    break
                buf += chunk
            result = buf.decode(errors="replace").strip()
            elapsed = (time.monotonic() - t0) * 1000
            if elapsed > 50:
                print(f"[ncrs-nautilus] {cmd}: {elapsed:.0f}ms", file=sys.stderr)
            return result
    except (OSError, socket.timeout):
        elapsed = (time.monotonic() - t0) * 1000
        print(f"[ncrs-nautilus] {cmd}: TIMEOUT ({elapsed:.0f}ms)", file=sys.stderr)
        return "error: socket timeout"


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
    "local": "Local",
    "synced": "Synced",
    "remote": "Remote",
    "downloading": "Downloading",
    "unknown": "",
}


class NcrsInfoProvider(GObject.GObject, Nautilus.InfoProvider):
    def __init__(self):
        super().__init__()
        self._cancelled: set[int] = set()
        self._lock = threading.Lock()
        self._mount = _load_mount_point()
        GLib.timeout_add_seconds(2, self._poll_changes)

    def _poll_changes(self) -> bool:
        try:
            _POOL.submit(self._do_poll_changes)
        except Exception:
            _log_error("_poll_changes submit")
        return True

    def _do_poll_changes(self):
        try:
            resp = _send_command("CHANGES")
            if not resp or resp.startswith("error"):
                return
            paths = resp.split("\t")

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

    def update_file_info(self, file_info):
        return Nautilus.OperationResult.COMPLETE

    def update_file_info_full(self, provider, handle, closure, file_info):
        try:
            if not self._mount:
                return Nautilus.OperationResult.COMPLETE

            if file_info.get_uri_scheme() != "file":
                return Nautilus.OperationResult.COMPLETE

            path = file_info.get_location().get_path()
            if path is None or not (path == self._mount or path.startswith(self._mount + "/")):
                return Nautilus.OperationResult.COMPLETE
        except Exception:
            _log_error("update_file_info_full (pre-check)")
            return Nautilus.OperationResult.COMPLETE

        handle_id = id(handle)

        def _work():
            try:
                detail = _send_command(f"DETAIL {path}")
            except Exception:
                _log_error(f"DETAIL({path})")
                detail = "unknown\t\t\t\t0"

            def _apply():
                try:
                    with self._lock:
                        if handle_id in self._cancelled:
                            self._cancelled.discard(handle_id)
                            return GLib.SOURCE_REMOVE

                    parts = detail.split("\t")
                    sync = parts[0] if len(parts) > 0 else "unknown"
                    sharing = parts[1] if len(parts) > 1 else ""
                    perms = parts[2] if len(parts) > 2 else ""
                    owner = parts[3] if len(parts) > 3 else ""
                    size_str = parts[4] if len(parts) > 4 else "0"

                    # Emblems
                    if sync == "local":
                        file_info.add_emblem(_EMBLEM_LOCAL)
                    elif sync == "synced":
                        file_info.add_emblem(_EMBLEM_SYNCED)
                    elif sync == "downloading":
                        file_info.add_emblem(_EMBLEM_REMOTE)
                    if sharing:
                        file_info.add_emblem(_EMBLEM_SHARED)

                    # Columns
                    file_info.add_string_attribute("ncrs_sync", _SYNC_LABELS.get(sync, ""))
                    file_info.add_string_attribute("ncrs_sharing", sharing)
                    file_info.add_string_attribute("ncrs_permissions", _human_perms(perms))
                    file_info.add_string_attribute("ncrs_owner", owner)
                    try:
                        size_val = int(size_str)
                        file_info.add_string_attribute("ncrs_size", _human_size(size_val) if size_val > 0 else "")
                    except ValueError:
                        file_info.add_string_attribute("ncrs_size", "")

                    Nautilus.info_provider_update_complete_invoke(
                        closure, provider, handle, Nautilus.OperationResult.COMPLETE)
                except Exception:
                    _log_error(f"_apply({path})")
                return GLib.SOURCE_REMOVE

            GLib.idle_add(_apply)

        _POOL.submit(_work)
        return Nautilus.OperationResult.IN_PROGRESS

    def cancel_update(self, provider, handle):
        try:
            with self._lock:
                self._cancelled.add(id(handle))
        except Exception:
            _log_error("cancel_update")


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

            keep = Nautilus.MenuItem(
                name="NcrsMenuProvider::KeepLocally",
                label="Keep Locally",
                tip="Download and keep a local copy of the selected files",
            )
            keep.connect("activate", self._on_keep_locally, paths)

            view_web = Nautilus.MenuItem(
                name="NcrsMenuProvider::ViewInWeb",
                label="View in Nextcloud Web",
                tip="Open this file in the Nextcloud web interface",
            )
            view_web.connect("activate", self._on_view_in_web, paths)

            return [keep, view_web]
        except Exception:
            _log_error("get_file_items")
            return []

    def _on_view_in_web(self, _menu_item, paths):
        def _do():
            try:
                import subprocess
                for path in paths:
                    url = _send_command(f"WEBURL {path}")
                    if url.startswith("http"):
                        subprocess.Popen(["xdg-open", url], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            except Exception:
                _log_error("_on_view_in_web")
        _POOL.submit(_do)

    def _on_keep_locally(self, _menu_item, paths):
        def _do():
            try:
                for path in paths:
                    _send_command(f"KEEP {path}")
            except Exception:
                _log_error("_on_keep_locally")
        _POOL.submit(_do)

    def get_background_items(self, *args):
        return []
