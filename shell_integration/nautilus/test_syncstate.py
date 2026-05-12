"""
Unit tests for the ncRS Nautilus extension helper functions.

Run with:  python3 -m pytest shell_integration/nautilus/test_syncstate.py -v
       or: python3 -m unittest shell_integration/nautilus/test_syncstate.py
"""

import os
import socket
import tempfile
import threading
import unittest

# Import only the pure-Python parts; skip the gi/Nautilus import.
# We do this by loading the module source directly after monkey-patching
# the import so tests can run without Nautilus installed.
import sys
import types

# ── Provide stub gi/Nautilus so the module can be imported in CI ──────────────
if "gi" not in sys.modules:
    gi_stub = types.ModuleType("gi")
    def require_version(ns, ver): pass
    gi_stub.require_version = require_version
    sys.modules["gi"] = gi_stub

repo = sys.modules.get("gi.repository")
if repo is None:
    repo = types.ModuleType("gi.repository")
    sys.modules["gi.repository"] = repo

class _GObjectBase:
    """Minimal GObject stand-in."""
    def __init__(self, *a, **kw): pass

class _InfoProvider:  pass
class _MenuProvider:  pass

_GLib_stub = types.SimpleNamespace(
    SOURCE_REMOVE=False,
    idle_add=lambda f, *a: None,
)
_GObject_stub = types.SimpleNamespace(GObject=_GObjectBase)
_Nautilus_stub = types.SimpleNamespace(
    InfoProvider=_InfoProvider,
    MenuProvider=_MenuProvider,
    OperationResult=types.SimpleNamespace(COMPLETE=0, IN_PROGRESS=1, FAILED=2),
    info_provider_update_complete_invoke=lambda *a: None,
)

for _name, _stub in (("GLib", _GLib_stub), ("GObject", _GObject_stub), ("Nautilus", _Nautilus_stub)):
    if not hasattr(repo, _name):
        setattr(repo, _name, _stub)

# ── Now import the extension ──────────────────────────────────────────────────
sys.path.insert(0, os.path.dirname(__file__))
import importlib
syncstate = importlib.import_module("syncstate")
query_status = syncstate.query_status


# ── Helpers ───────────────────────────────────────────────────────────────────

def _start_mock_server(sock_path: str, responses: dict[str, str]) -> threading.Thread:
    """
    Minimal Unix socket server for testing.

    *responses* maps a path string to the status string the server should reply.
    Unknown paths get 'unknown'.
    """
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(sock_path)
    srv.listen(5)
    srv.settimeout(2)

    def _serve():
        try:
            while True:
                try:
                    conn, _ = srv.accept()
                except socket.timeout:
                    break
                with conn:
                    data = b""
                    while b"\n" not in data:
                        chunk = conn.recv(256)
                        if not chunk:
                            break
                        data += chunk
                    line = data.decode().strip()
                    if line.startswith("STATUS "):
                        path = line[len("STATUS "):]
                        reply = responses.get(path, "unknown")
                    else:
                        reply = "unknown"
                    conn.sendall((reply + "\n").encode())
        finally:
            srv.close()

    t = threading.Thread(target=_serve, daemon=True)
    t.start()
    return t


# ── Tests ─────────────────────────────────────────────────────────────────────

class TestQueryStatus(unittest.TestCase):

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp(prefix="ncrs_test_")
        self.sock_path = os.path.join(self.tmpdir, "ncrs.sock")

    def tearDown(self):
        import shutil
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_returns_unknown_when_no_socket(self):
        """Returns 'unknown' when the daemon socket does not exist."""
        status = query_status("/any/path", sock_path=self.sock_path)
        self.assertEqual(status, "unknown")

    def test_local_status(self):
        """Returns 'local' for a file the daemon marks as cached."""
        _start_mock_server(self.sock_path, {"/mnt/ncrs/report.pdf": "local"})
        status = query_status("/mnt/ncrs/report.pdf", sock_path=self.sock_path)
        self.assertEqual(status, "local")

    def test_remote_status(self):
        """Returns 'remote' for a file not yet downloaded."""
        _start_mock_server(self.sock_path, {"/mnt/ncrs/big.iso": "remote"})
        status = query_status("/mnt/ncrs/big.iso", sock_path=self.sock_path)
        self.assertEqual(status, "remote")

    def test_synced_status(self):
        _start_mock_server(self.sock_path, {"/mnt/ncrs/doc.txt": "synced"})
        status = query_status("/mnt/ncrs/doc.txt", sock_path=self.sock_path)
        self.assertEqual(status, "synced")

    def test_unknown_path_returns_unknown(self):
        """Daemon replies 'unknown' for a path it hasn't seen."""
        _start_mock_server(self.sock_path, {})  # no known paths
        status = query_status("/not/in/mount", sock_path=self.sock_path)
        self.assertEqual(status, "unknown")

    def test_multiple_queries_same_server(self):
        """The server can answer multiple sequential queries."""
        _start_mock_server(self.sock_path, {
            "/mnt/a": "local",
            "/mnt/b": "remote",
        })
        self.assertEqual(query_status("/mnt/a", sock_path=self.sock_path), "local")
        self.assertEqual(query_status("/mnt/b", sock_path=self.sock_path), "remote")
        self.assertEqual(query_status("/mnt/c", sock_path=self.sock_path), "unknown")

    def test_timeout_returns_unknown(self):
        """Returns 'unknown' if the daemon accepts but never replies."""
        # Server that accepts but hangs.
        srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        srv.bind(self.sock_path)
        srv.listen(1)

        def _hang():
            try:
                conn, _ = srv.accept()
                # Intentionally never send a reply; hold connection open briefly.
                import time; time.sleep(2)
                conn.close()
            finally:
                srv.close()

        threading.Thread(target=_hang, daemon=True).start()
        status = query_status("/mnt/ncrs/file", sock_path=self.sock_path)
        self.assertEqual(status, "unknown")


class TestLoadMountPoint(unittest.TestCase):

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp(prefix="ncrs_test_cfg_")

    def tearDown(self):
        import shutil
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_parses_mount_point(self):
        cfg = os.path.join(self.tmpdir, "config.yaml")
        with open(cfg, "w") as f:
            f.write('url: "https://cloud.example.com"\nmount_point: "/home/user/ncrs"\n')
        self.assertEqual(syncstate._load_mount_point(cfg), "/home/user/ncrs")

    def test_strips_trailing_slash(self):
        cfg = os.path.join(self.tmpdir, "config.yaml")
        with open(cfg, "w") as f:
            f.write('mount_point: "/home/user/ncrs/"\n')
        self.assertEqual(syncstate._load_mount_point(cfg), "/home/user/ncrs")

    def test_quoted_value(self):
        cfg = os.path.join(self.tmpdir, "config.yaml")
        with open(cfg, "w") as f:
            f.write('mount_point: "/home/user/my cloud"\n')
        self.assertEqual(syncstate._load_mount_point(cfg), "/home/user/my cloud")

    def test_missing_file_returns_none(self):
        self.assertIsNone(syncstate._load_mount_point("/nonexistent/config.yaml"))

    def test_empty_value_returns_none(self):
        cfg = os.path.join(self.tmpdir, "config.yaml")
        with open(cfg, "w") as f:
            f.write('mount_point: ""\n')
        self.assertIsNone(syncstate._load_mount_point(cfg))


if __name__ == "__main__":
    unittest.main()
