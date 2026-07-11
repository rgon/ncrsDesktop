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

class _InfoProvider:    pass
class _MenuProvider:    pass
class _ColumnProvider:  pass

_GLib_stub = types.SimpleNamespace(
    SOURCE_REMOVE=False,
    idle_add=lambda f, *a: None,
    timeout_add_seconds=lambda *a: None,
    markup_escape_text=lambda s: s,
)
_GObject_stub = types.SimpleNamespace(GObject=_GObjectBase)
_Nautilus_stub = types.SimpleNamespace(
    InfoProvider=_InfoProvider,
    MenuProvider=_MenuProvider,
    ColumnProvider=_ColumnProvider,
    OperationResult=types.SimpleNamespace(COMPLETE=0, IN_PROGRESS=1, FAILED=2),
    info_provider_update_complete_invoke=lambda *a: None,
    Column=lambda **kw: None,
    MenuItem=lambda **kw: types.SimpleNamespace(connect=lambda *a: None),
    FileInfo=types.SimpleNamespace(lookup=lambda *a: None),
)
_Gio_stub = types.SimpleNamespace(
    File=types.SimpleNamespace(new_for_path=lambda p: None),
)
_Gtk_stub = types.SimpleNamespace(
    Window=_GObjectBase,
    Box=_GObjectBase,
    Button=_GObjectBase,
    Entry=_GObjectBase,
    Label=_GObjectBase,
    ScrolledWindow=_GObjectBase,
    Spinner=_GObjectBase,
    Orientation=types.SimpleNamespace(VERTICAL=0, HORIZONTAL=1),
    Align=types.SimpleNamespace(START=0),
)

for _name, _stub in (
    ("GLib", _GLib_stub),
    ("GObject", _GObject_stub),
    ("Nautilus", _Nautilus_stub),
    ("Gio", _Gio_stub),
    ("Gtk", _Gtk_stub),
):
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

    def test_no_mount_point_key_returns_none(self):
        cfg = os.path.join(self.tmpdir, "config.yaml")
        with open(cfg, "w") as f:
            f.write('url: "https://cloud.example.com"\nusername: alice\n')
        self.assertIsNone(syncstate._load_mount_point(cfg))

    def test_similar_key_not_matched(self):
        """mount_point_override: must NOT match the mount_point: parser."""
        cfg = os.path.join(self.tmpdir, "config.yaml")
        with open(cfg, "w") as f:
            f.write('mount_point_override: "/other"\n')
        self.assertIsNone(syncstate._load_mount_point(cfg))

    def test_xdg_config_home_env_path(self):
        """Default path uses XDG_CONFIG_HOME when set."""
        cfg_dir = os.path.join(self.tmpdir, "ncrs")
        os.makedirs(cfg_dir)
        cfg = os.path.join(cfg_dir, "config.yaml")
        with open(cfg, "w") as f:
            f.write('mount_point: "/home/user/cloud"\n')
        old = os.environ.get("XDG_CONFIG_HOME")
        try:
            os.environ["XDG_CONFIG_HOME"] = self.tmpdir
            result = syncstate._load_mount_point()   # no explicit path → uses env
            self.assertEqual(result, "/home/user/cloud")
        finally:
            if old is None:
                os.environ.pop("XDG_CONFIG_HOME", None)
            else:
                os.environ["XDG_CONFIG_HOME"] = old


class TestHumanPerms(unittest.TestCase):

    def test_known_flags(self):
        result = syncstate._human_perms("RGWCD")
        labels = result.split(", ")
        self.assertIn("Read", labels)
        self.assertIn("Write", labels)
        self.assertIn("Create", labels)
        self.assertIn("Delete", labels)

    def test_empty_string(self):
        self.assertEqual(syncstate._human_perms(""), "")

    def test_unknown_flag_falls_through_to_raw(self):
        """A string of entirely unknown chars should return the raw string."""
        result = syncstate._human_perms("XYZ")
        self.assertEqual(result, "XYZ")

    def test_duplicate_flags_not_repeated(self):
        """Repeated flag chars must not produce duplicate labels."""
        result = syncstate._human_perms("GGW")
        labels = result.split(", ")
        self.assertEqual(labels.count("Read"), 1, "Read should appear only once")

    def test_shared_flag(self):
        result = syncstate._human_perms("S")
        self.assertIn("Shared", result)


class TestHumanSize(unittest.TestCase):

    def test_bytes(self):
        self.assertEqual(syncstate._human_size(0), "0 B")
        self.assertEqual(syncstate._human_size(1023), "1023 B")

    def test_kib_boundary(self):
        self.assertEqual(syncstate._human_size(1024), "1.0 KiB")

    def test_mib(self):
        self.assertEqual(syncstate._human_size(1024 * 1024), "1.0 MiB")

    def test_gib(self):
        self.assertEqual(syncstate._human_size(1024 ** 3), "1.0 GiB")

    def test_fractional(self):
        result = syncstate._human_size(1536)   # 1.5 KiB
        self.assertEqual(result, "1.5 KiB")


class TestPersistentConnRetry(unittest.TestCase):

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp(prefix="ncrs_conn_test_")
        self.sock_path = os.path.join(self.tmpdir, "ncrs.sock")

    def tearDown(self):
        import shutil
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_reconnects_after_server_closes_connection(self):
        """
        _PersistentConn must reconnect and succeed if the server closes the
        connection after the first reply (e.g., daemon restart).
        """
        request_count = [0]

        srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        srv.bind(self.sock_path)
        srv.listen(5)
        srv.settimeout(3)

        def _serve():
            try:
                while True:
                    try:
                        conn, _ = srv.accept()
                    except socket.timeout:
                        break
                    request_count[0] += 1
                    data = b""
                    while b"\n" not in data:
                        chunk = conn.recv(256)
                        if not chunk:
                            break
                        data += chunk
                    conn.sendall(b"synced\n")
                    conn.close()   # close after first reply → forces reconnect
            finally:
                srv.close()

        threading.Thread(target=_serve, daemon=True).start()

        # Monkey-patch the socket path so _PersistentConn connects to our server.
        original = syncstate._sock_path
        syncstate._sock_path = lambda: self.sock_path
        try:
            conn = syncstate._PersistentConn()
            first  = conn.send("STATUS /a")
            second = conn.send("STATUS /b")  # triggers reconnect
            self.assertEqual(first,  "synced")
            self.assertEqual(second, "synced")
            self.assertGreaterEqual(request_count[0], 2,
                "server should have received at least 2 connections")
        finally:
            syncstate._sock_path = original


class TestUploadingStatus(unittest.TestCase):
    """Verify the 'uploading' status string is wired up in syncstate."""

    def test_uploading_label_present(self):
        """'uploading' must appear in _SYNC_LABELS so Nautilus column shows text."""
        self.assertIn("uploading", syncstate._SYNC_LABELS,
                      "_SYNC_LABELS must contain 'uploading'")

    def test_uploading_label_value(self):
        self.assertEqual(syncstate._SYNC_LABELS.get("uploading"), "Uploading")

    def test_emblem_uploading_constant_defined(self):
        """_EMBLEM_UPLOADING must be defined (non-empty string)."""
        self.assertTrue(
            hasattr(syncstate, "_EMBLEM_UPLOADING"),
            "syncstate must export _EMBLEM_UPLOADING",
        )
        self.assertIsInstance(syncstate._EMBLEM_UPLOADING, str)
        self.assertTrue(syncstate._EMBLEM_UPLOADING,
                        "_EMBLEM_UPLOADING must not be empty")

    def test_uploading_emblem_same_family_as_remote(self):
        """_EMBLEM_REMOTE and _EMBLEM_UPLOADING must both be non-empty icon names."""
        self.assertTrue(hasattr(syncstate, "_EMBLEM_REMOTE"))
        self.assertIsInstance(syncstate._EMBLEM_REMOTE, str)
        self.assertTrue(syncstate._EMBLEM_REMOTE)


class _FakeFileInfo:
    """Records emblems and attributes set by the extension."""

    def __init__(self, path, scheme="file"):
        self._path = path
        self._scheme = scheme
        self.emblems = []
        self.attrs = {}

    def get_uri_scheme(self):
        return self._scheme

    def get_location(self):
        return types.SimpleNamespace(get_path=lambda: self._path)

    def add_emblem(self, name):
        self.emblems.append(name)

    def add_string_attribute(self, key, value):
        self.attrs[key] = value


class TestParseDetailDir(unittest.TestCase):
    def test_parses_records(self):
        resp = "a.txt\tkept\tShared by you\tRGDNVW\tAlice\t100"
        resp += "\x1e" + "sub\tremote\t\t\t\t4096"
        parsed = syncstate._parse_detaildir(resp)
        self.assertEqual(parsed["a.txt"], ("kept", "Shared by you", "RGDNVW", "Alice", "100"))
        self.assertEqual(parsed["sub"], ("remote", "", "", "", "4096"))

    def test_empty_response(self):
        self.assertEqual(syncstate._parse_detaildir(""), {})

    def test_skips_malformed_records(self):
        parsed = syncstate._parse_detaildir("short\tonly\ttwo\x1ea\tkept\ts\tp\to\t5")
        self.assertNotIn("short", parsed)
        self.assertIn("a", parsed)


class TestInfoProviderSyncCache(unittest.TestCase):
    """The InfoProvider must answer synchronously (COMPLETE) and warm the whole
    directory with a single DETAILDIR call rather than one query per file."""

    def setUp(self):
        # Run pool tasks and idle callbacks inline so the test is deterministic.
        self._orig_submit = syncstate._POOL.submit
        self._orig_idle = syncstate.GLib.idle_add
        self._orig_send = syncstate._send_command
        syncstate._POOL.submit = lambda fn, *a: (fn(*a), None)[1]
        syncstate.GLib.idle_add = lambda fn, *a: (fn(*a), None)[1]
        # Reset module cache
        with syncstate._cache_lock:
            syncstate._dir_cache.clear()
            syncstate._dir_cache_ts.clear()
            syncstate._dir_inflight.clear()
            syncstate._dir_pending.clear()

    def tearDown(self):
        syncstate._POOL.submit = self._orig_submit
        syncstate.GLib.idle_add = self._orig_idle
        syncstate._send_command = self._orig_send

    def _make_provider(self, mount="/mnt/ncrs"):
        prov = syncstate.NcrsInfoProvider.__new__(syncstate.NcrsInfoProvider)
        prov._mount = mount
        prov._mount_prefix = mount + "/"
        prov._poll_running = False
        prov._poll_skip = 0
        return prov

    def test_one_detaildir_call_warms_all_children(self):
        calls = []

        def fake_send(cmd):
            calls.append(cmd)
            if cmd.startswith("DETAILDIR "):
                return (
                    "a.txt\tkept\t\tRGDNVW\tAlice\t100"
                    "\x1e" + "b.txt\tremote\t\t\t\t0"
                )
            return "error"

        syncstate._send_command = fake_send
        prov = self._make_provider()

        # First request for a.txt: cache miss → COMPLETE + one DETAILDIR fetch.
        fi_a = _FakeFileInfo("/mnt/ncrs/dir/a.txt")
        res = prov.update_file_info_full(None, None, None, fi_a)
        self.assertEqual(res, syncstate.Nautilus.OperationResult.COMPLETE)

        # Exactly one DETAILDIR was issued for the parent directory.
        self.assertEqual(calls, ["DETAILDIR /mnt/ncrs/dir"])

        # A second file in the same dir must NOT trigger another socket call.
        fi_b = _FakeFileInfo("/mnt/ncrs/dir/b.txt")
        res = prov.update_file_info_full(None, None, None, fi_b)
        self.assertEqual(res, syncstate.Nautilus.OperationResult.COMPLETE)
        self.assertEqual(calls, ["DETAILDIR /mnt/ncrs/dir"], "cache hit: no extra query")

        # And b.txt was served synchronously from the warmed cache.
        self.assertEqual(fi_b.attrs.get("ncrs_sync"), "Remote")

    def test_cache_hit_applies_metadata(self):
        syncstate._send_command = lambda cmd: "f\tkept\tShared by you\tRGDNVW\tAlice\t2048"
        prov = self._make_provider()
        # First call is a cache miss: it paints nothing but warms the directory.
        prov.update_file_info_full(None, None, None, _FakeFileInfo("/mnt/ncrs/dir/f"))
        # Nautilus re-requests after the invalidation; now it is a cache hit.
        fi = _FakeFileInfo("/mnt/ncrs/dir/f")
        prov.update_file_info_full(None, None, None, fi)
        self.assertEqual(fi.attrs.get("ncrs_sync"), "Kept locally")
        self.assertEqual(fi.attrs.get("ncrs_owner"), "Alice")
        self.assertEqual(fi.attrs.get("ncrs_permissions"), syncstate._human_perms("RGDNVW"))
        self.assertIn(syncstate._EMBLEM_SHARED, fi.emblems)

    def test_outside_mount_returns_complete_without_query(self):
        calls = []
        syncstate._send_command = lambda cmd: calls.append(cmd) or "error"
        prov = self._make_provider()
        fi = _FakeFileInfo("/home/user/elsewhere/x.txt")
        res = prov.update_file_info_full(None, None, None, fi)
        self.assertEqual(res, syncstate.Nautilus.OperationResult.COMPLETE)
        self.assertEqual(calls, [], "files outside the mount must not hit the daemon")

    def test_non_file_scheme_ignored(self):
        calls = []
        syncstate._send_command = lambda cmd: calls.append(cmd) or "error"
        prov = self._make_provider()
        fi = _FakeFileInfo("/mnt/ncrs/dir/a.txt", scheme="recent")
        res = prov.update_file_info_full(None, None, None, fi)
        self.assertEqual(res, syncstate.Nautilus.OperationResult.COMPLETE)
        self.assertEqual(calls, [])

    def test_invalidation_scoped_to_requested_children(self):
        # A huge directory must repaint only the files Nautilus actually asked
        # about, never every child. Defer the fetch so several requests pile up
        # into the pending set before it lands.
        submitted = []
        syncstate._POOL.submit = lambda fn, *a: submitted.append((fn, a))
        looked_up = []
        orig_lookup = syncstate.Nautilus.FileInfo.lookup
        orig_newpath = syncstate.Gio.File.new_for_path
        syncstate.Nautilus.FileInfo.lookup = lambda gfile: looked_up.append(gfile) or None
        syncstate.Gio.File.new_for_path = lambda p: p
        try:
            children = [f"f{i}" for i in range(1000)]
            syncstate._send_command = lambda cmd: "\x1e".join(
                f"{n}\tremote\t\t\t\t0" for n in children
            )
            prov = self._make_provider()
            # Only two of the 1000 children are requested (as if visible).
            prov.update_file_info_full(None, None, None, _FakeFileInfo("/mnt/ncrs/dir/f1"))
            prov.update_file_info_full(None, None, None, _FakeFileInfo("/mnt/ncrs/dir/f2"))
            self.assertEqual(len(submitted), 1, "one DETAILDIR fetch for the directory")
            fn, args = submitted[0]
            fn(*args)  # run the deferred fetch; idle_add runs inline
            # Exactly the two requested children were invalidated, not all 1000.
            self.assertEqual(set(looked_up), {"/mnt/ncrs/dir/f1", "/mnt/ncrs/dir/f2"})
        finally:
            syncstate.Nautilus.FileInfo.lookup = orig_lookup
            syncstate.Gio.File.new_for_path = orig_newpath

    def test_dir_cache_is_bounded(self):
        # Browsing many directories must not grow the cache without bound.
        orig_max = syncstate._DIR_CACHE_MAX
        syncstate._DIR_CACHE_MAX = 8
        try:
            # each DETAILDIR returns one child so every dir warms deterministically
            syncstate._send_command = lambda cmd: "child\tremote\t\t\t\t0"
            prov = self._make_provider()
            for i in range(50):
                prov.update_file_info_full(
                    None, None, None, _FakeFileInfo(f"/mnt/ncrs/dir{i}/child")
                )
            with syncstate._cache_lock:
                self.assertLessEqual(len(syncstate._dir_cache), syncstate._DIR_CACHE_MAX)
                # cache and timestamp maps stay consistent
                self.assertEqual(
                    set(syncstate._dir_cache), set(syncstate._dir_cache_ts)
                )
                # the most recently fetched directory is retained
                self.assertIn("/mnt/ncrs/dir49", syncstate._dir_cache)
        finally:
            syncstate._DIR_CACHE_MAX = orig_max


class TestDaemonVersionCheck(unittest.TestCase):
    """The extension announces its protocol version and warns on a mismatch or
    an old daemon that doesn't understand VERSION."""

    def setUp(self):
        self._orig_send = syncstate._send_command

    def tearDown(self):
        syncstate._send_command = self._orig_send

    def _run_check(self, reply):
        sent = []
        syncstate._send_command = lambda cmd: (sent.append(cmd), reply)[1]
        prov = syncstate.NcrsInfoProvider.__new__(syncstate.NcrsInfoProvider)
        import io
        import contextlib
        buf = io.StringIO()
        with contextlib.redirect_stderr(buf):
            prov._check_daemon_version()
        return sent, buf.getvalue()

    def test_announces_own_protocol_version(self):
        sent, _ = self._run_check(f"{syncstate.PROTOCOL_VERSION}\t0.1.10")
        self.assertEqual(sent, [f"VERSION {syncstate.PROTOCOL_VERSION}"])

    def test_matching_version_no_warning(self):
        _, err = self._run_check(f"{syncstate.PROTOCOL_VERSION}\t0.1.10")
        self.assertNotIn("warning", err.lower())
        self.assertIn(f"protocol v{syncstate.PROTOCOL_VERSION}", err)

    def test_mismatched_version_warns(self):
        _, err = self._run_check(f"{syncstate.PROTOCOL_VERSION + 1}\t9.9.9")
        self.assertIn("warning", err.lower())
        self.assertIn("does not match", err)

    def test_old_daemon_without_version_command_warns(self):
        # Pre-VERSION daemons reply "unknown" to an unrecognised command.
        _, err = self._run_check("unknown")
        self.assertIn("warning", err.lower())
        self.assertIn("older build", err)

    def test_connection_error_warns(self):
        _, err = self._run_check("error: connection failed")
        self.assertIn("warning", err.lower())


class TestTargetedChangeRefresh(unittest.TestCase):
    """A change refreshes just its own cache entry (one DETAIL), never a whole
    directory — unless many entries in one directory change at once."""

    def setUp(self):
        self._orig_send = syncstate._send_command
        with syncstate._cache_lock:
            syncstate._dir_cache.clear()
            syncstate._dir_cache_ts.clear()
            syncstate._dir_inflight.clear()
            syncstate._dir_pending.clear()

    def tearDown(self):
        syncstate._send_command = self._orig_send
        with syncstate._cache_lock:
            syncstate._dir_cache.clear()
            syncstate._dir_cache_ts.clear()

    def test_patch_cache_entry_updates_single_record(self):
        syncstate._dir_cache["/d"] = {"f": ("remote", "", "", "", "0")}
        syncstate._dir_cache_ts["/d"] = 123.0
        calls = []
        syncstate._send_command = lambda cmd: calls.append(cmd) or "kept\t\tRGDNVW\tAlice\t100"
        self.assertTrue(syncstate._patch_cache_entry("/d/f"))
        self.assertEqual(
            syncstate._dir_cache["/d"]["f"], ("kept", "", "RGDNVW", "Alice", "100")
        )
        self.assertEqual(calls, ["DETAIL /d/f"])
        # The rest of the directory's cache — and its freshness — is untouched.
        self.assertEqual(syncstate._dir_cache_ts["/d"], 123.0)

    def test_patch_cache_entry_noop_when_dir_not_cached(self):
        calls = []
        syncstate._send_command = lambda cmd: calls.append(cmd) or "kept\t\t\t\t0"
        self.assertFalse(syncstate._patch_cache_entry("/x/y"))
        self.assertEqual(calls, [], "must not query the daemon for an uncached dir")

    def test_small_change_set_is_targeted(self):
        syncstate._dir_cache["/d"] = {
            "a": ("remote", "", "", "", "0"),
            "b": ("remote", "", "", "", "0"),
        }
        syncstate._dir_cache_ts["/d"] = 500.0
        calls = []
        syncstate._send_command = lambda cmd: calls.append(cmd) or "kept\t\tRG\tAlice\t7"
        syncstate._refresh_changed_paths(["/d/a", "/d/b"])
        self.assertEqual(sorted(calls), ["DETAIL /d/a", "DETAIL /d/b"])
        self.assertNotIn("DETAILDIR /d", calls)
        self.assertIn("/d", syncstate._dir_cache_ts, "dir stays fresh, not refetched")
        self.assertEqual(syncstate._dir_cache["/d"]["a"][0], "kept")

    def test_bulk_change_set_falls_back_to_whole_dir(self):
        syncstate._dir_cache["/d"] = {}
        syncstate._dir_cache_ts["/d"] = 500.0
        calls = []
        syncstate._send_command = lambda cmd: calls.append(cmd) or "kept\t\t\t\t0"
        many = [f"/d/f{i}" for i in range(syncstate._CHANGE_PATCH_MAX + 1)]
        syncstate._refresh_changed_paths(many)
        # Past the threshold: no per-file DETAIL; the dir is marked stale so the
        # next access does a single DETAILDIR.
        self.assertEqual(calls, [])
        self.assertNotIn("/d", syncstate._dir_cache_ts)

    def test_refresh_uncached_dir_is_noop(self):
        calls = []
        syncstate._send_command = lambda cmd: calls.append(cmd) or "kept\t\t\t\t0"
        syncstate._refresh_changed_paths(["/nope/a", "/nope/b"])
        self.assertEqual(calls, [])

    def test_poll_changes_patches_entry_without_refetching_dir(self):
        # End-to-end through the poll: a CHANGES entry refreshes just that entry
        # and the directory's cache timestamp is preserved (no DETAILDIR).
        parent = "/mnt/ncrs/dir"
        syncstate._dir_cache[parent] = {"f": ("remote", "", "", "", "0")}
        ts = 999.0
        syncstate._dir_cache_ts[parent] = ts
        calls = []

        def fake(cmd):
            calls.append(cmd)
            if cmd == "FILE_CHANGES":
                return ""
            if cmd == "CHANGES":
                return f"{parent}/f"
            if cmd.startswith("DETAIL "):
                return "kept\tShared by you\tRGDNVW\tAlice\t100"
            return "error"

        syncstate._send_command = fake
        prov = syncstate.NcrsInfoProvider.__new__(syncstate.NcrsInfoProvider)
        prov._mount = "/mnt/ncrs"
        prov._mount_prefix = "/mnt/ncrs/"
        prov._poll_running = True
        prov._poll_skip = 0
        prov._do_poll_changes()

        self.assertEqual(syncstate._dir_cache[parent]["f"][0], "kept")
        self.assertEqual(syncstate._dir_cache[parent]["f"][3], "Alice")
        self.assertEqual(syncstate._dir_cache_ts[parent], ts, "dir not marked stale")
        self.assertIn(f"DETAIL {parent}/f", calls)
        self.assertNotIn(f"DETAILDIR {parent}", calls)


if __name__ == "__main__":
    unittest.main()
