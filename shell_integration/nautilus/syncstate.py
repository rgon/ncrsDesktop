"""
ncRS Nautilus extension — decorates files under the ncRS mount point with
sync-state emblems so the user can tell at a glance which files are local
(downloaded to disk) and which are still remote-only (will download on open).

Requires:
  sudo apt install python3-nautilus
  (or the equivalent python3-gi + nautilus-python package for your distro)

Install:
  cp syncstate.py ~/.local/share/nautilus-python/extensions/
  nautilus -q          # restart Nautilus

The daemon must be running; it exposes a Unix socket at
$XDG_RUNTIME_DIR/ncrs.sock (usually /run/user/<UID>/ncrs.sock).

Protocol: send "STATUS <abs-path>\n", receive one of:
  local   — cached on disk, up to date
  synced  — cached but freshness not confirmed
  remote  — known file, not yet downloaded
  unknown — path not under mount point or daemon not seen it
"""

import os
import socket
import threading
from pathlib import Path

try:
    import gi
    gi.require_version("Nautilus", "4.0")
    from gi.repository import GObject, Nautilus
    NAUTILUS_VERSION = 4
except (ImportError, ValueError):
    try:
        import gi
        gi.require_version("Nautilus", "3.0")
        from gi.repository import GObject, Nautilus
        NAUTILUS_VERSION = 3
    except (ImportError, ValueError):
        raise ImportError("nautilus-python not found")

SOCKET_TIMEOUT = 0.5  # seconds — avoid hanging Nautilus on daemon absence
_EMBLEM_LOCAL   = "emblem-default"   # green tick (standard XDG emblem)
_EMBLEM_REMOTE  = "emblem-downloads" # cloud/down arrow
_EMBLEM_SYNCED  = "emblem-synchronizing"

def _sock_path() -> str:
    runtime = os.environ.get("XDG_RUNTIME_DIR") or f"/run/user/{os.getuid()}"
    return os.path.join(runtime, "ncrs.sock")


def query_status(path: str) -> str:
    """Ask the ncRS daemon for the sync status of *path*.  Returns 'unknown' on any error."""
    sock_path = _sock_path()
    if not os.path.exists(sock_path):
        return "unknown"
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.settimeout(SOCKET_TIMEOUT)
            s.connect(sock_path)
            s.sendall(f"STATUS {path}\n".encode())
            data = b""
            while True:
                chunk = s.recv(64)
                if not chunk:
                    break
                data += chunk
                if b"\n" in data:
                    break
            return data.decode().strip()
    except (OSError, socket.timeout):
        return "unknown"


class NcrsMenuProvider(GObject.GObject, Nautilus.MenuProvider):
    """Provides right-click menu items (placeholder for future actions)."""
    pass


class NcrsInfoProvider(GObject.GObject, Nautilus.InfoProvider):
    """Decorates files with sync-state emblems."""

    def update_file_info(self, file_info):
        """Called by Nautilus for each visible file."""
        if file_info.get_uri_scheme() != "file":
            return Nautilus.OperationResult.COMPLETE

        path = file_info.get_location().get_path()
        if path is None:
            return Nautilus.OperationResult.COMPLETE

        status = query_status(path)

        if status == "local":
            file_info.add_emblem(_EMBLEM_LOCAL)
        elif status == "remote":
            file_info.add_emblem(_EMBLEM_REMOTE)
        elif status == "synced":
            file_info.add_emblem(_EMBLEM_SYNCED)
        # unknown → no emblem (file not managed by ncRS)

        return Nautilus.OperationResult.COMPLETE

    def update_file_info_full(self, provider, handle, closure, file_info):
        """Async version called by newer Nautilus builds."""
        result = self.update_file_info(file_info)
        Nautilus.info_provider_update_complete_invoke(closure, provider, handle, result)
        return Nautilus.OperationResult.COMPLETE
