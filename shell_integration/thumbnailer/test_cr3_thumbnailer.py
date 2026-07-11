"""
Tests for the cr3-thumbnailer script.

Run with:  python3 -m pytest shell_integration/thumbnailer/test_cr3_thumbnailer.py -v
       or: python3 -m unittest shell_integration/thumbnailer/test_cr3_thumbnailer.py

Requires: python3-gi, gir1.2-gdkpixbuf-2.0  (same deps as the thumbnailer itself)
"""
import hashlib
import importlib.util
import os
import socket
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

import gi
gi.require_version('GdkPixbuf', '2.0')
from gi.repository import GdkPixbuf

# ── Import thumbnailer as a module (main() guard prevents execution) ──────────
# spec_from_file_location returns None for extension-less files; use loader directly.
_SCRIPT = os.path.join(os.path.dirname(__file__), 'cr3-thumbnailer')
_loader = importlib.util.spec_from_loader(
    'cr3_thumbnailer',
    importlib.machinery.SourceFileLoader('cr3_thumbnailer', _SCRIPT),
)
_mod = importlib.util.module_from_spec(_loader)
_loader.loader.exec_module(_mod)
ipc  = _mod.ipc
main = _mod.main


# ── Helpers ───────────────────────────────────────────────────────────────────

def _make_png(path, width=100, height=80):
    """Write a small RGB PNG to path."""
    pb = GdkPixbuf.Pixbuf.new(GdkPixbuf.Colorspace.RGB, False, 8, width, height)
    pb.fill(0xaabbccff)
    pb.savev(path, 'png', [], [])


def _make_jpeg_bytes(width=50, height=40):
    """Return minimal valid JPEG bytes."""
    pb = GdkPixbuf.Pixbuf.new(GdkPixbuf.Colorspace.RGB, False, 8, width, height)
    pb.fill(0x336699ff)
    _ok, buf = pb.save_to_bufferv('jpeg', [], [])
    return bytes(buf)


def _ipc_server(sock_path, responses, extra_action=None):
    """
    Start a Unix socket server in a daemon thread.

    *responses*: dict mapping IPC command strings (e.g. 'STATUS /a/b') to reply strings.
    *extra_action*: optional callable(cmd) called before replying (e.g. to write a file).
    Returns the server thread.
    """
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(sock_path)
    srv.listen(8)
    srv.settimeout(2)

    def _serve():
        try:
            while True:
                try:
                    conn, _ = srv.accept()
                except socket.timeout:
                    break
                with conn:
                    data = b''
                    while b'\n' not in data:
                        chunk = conn.recv(256)
                        if not chunk:
                            break
                        data += chunk
                    cmd = data.decode(errors='replace').strip()
                    if extra_action:
                        extra_action(cmd)
                    reply = responses.get(cmd, 'unknown')
                    conn.sendall((reply + '\n').encode())
        finally:
            srv.close()

    t = threading.Thread(target=_serve, daemon=True)
    t.start()
    return t


def _run_main(argv, env_overrides=None):
    """
    Call main() with the given argv and optional env overrides.
    Returns the integer exit code (0 if main() returned normally).
    """
    env = os.environ.copy()
    if env_overrides:
        env.update(env_overrides)

    with patch.object(sys, 'argv', argv), \
         patch.dict(os.environ, env_overrides or {}, clear=False):
        try:
            main()
            return 0
        except SystemExit as e:
            return int(e.code) if e.code is not None else 0


# ── Tests: ipc() ─────────────────────────────────────────────────────────────

class TestIpc(unittest.TestCase):

    def setUp(self):
        self.tmpdir  = tempfile.mkdtemp(prefix='ncrs_thumb_test_')
        self.sock    = os.path.join(self.tmpdir, 'ncrs.sock')

    def tearDown(self):
        import shutil
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def _env(self):
        return {'XDG_RUNTIME_DIR': self.tmpdir}

    def test_returns_none_when_no_socket(self):
        with patch.dict(os.environ, self._env()):
            self.assertIsNone(ipc('STATUS /x/y'))

    def test_returns_server_response(self):
        _ipc_server(self.sock, {'STATUS /mnt/ncrs/file.cr3': 'synced'})
        with patch.dict(os.environ, self._env()):
            self.assertEqual(ipc('STATUS /mnt/ncrs/file.cr3'), 'synced')

    def test_returns_none_on_timeout(self):
        # Server that accepts but never replies.
        srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        srv.bind(self.sock)
        srv.listen(1)
        def _hang():
            try:
                conn, _ = srv.accept()
                import time; time.sleep(5)
                conn.close()
            finally:
                srv.close()
        threading.Thread(target=_hang, daemon=True).start()
        with patch.dict(os.environ, self._env()):
            self.assertIsNone(ipc('STATUS /x', timeout=0.2))

    def test_empty_xdg_runtime_dir_falls_back_to_run_user(self):
        # With XDG_RUNTIME_DIR='', the `or` guard must use /run/user/<uid> rather
        # than '' (which would produce a relative path 'ncrs.sock' and fail silently).
        attempted = []
        def capture_connect(self_s, addr):
            attempted.append(addr)
            raise ConnectionRefusedError
        with patch.dict(os.environ, {'XDG_RUNTIME_DIR': ''}), \
             patch.object(socket.socket, 'connect', capture_connect):
            ipc('STATUS /x', timeout=0.1)
        expected = f'/run/user/{os.getuid()}/ncrs.sock'
        self.assertTrue(attempted, 'connect should have been called')
        self.assertEqual(attempted[0], expected)

    def test_multiple_sequential_queries(self):
        _ipc_server(self.sock, {
            'STATUS /a': 'local',
            'STATUS /b': 'remote',
        })
        with patch.dict(os.environ, self._env()):
            self.assertEqual(ipc('STATUS /a'), 'local')
            self.assertEqual(ipc('STATUS /b'), 'remote')
            self.assertEqual(ipc('STATUS /c'), 'unknown')


# ── Tests: main() routing ─────────────────────────────────────────────────────

class TestMainRouting(unittest.TestCase):

    def setUp(self):
        self.tmpdir   = tempfile.mkdtemp(prefix='ncrs_thumb_test_')
        self.sock     = os.path.join(self.tmpdir, 'ncrs.sock')
        self.xdg_dir  = os.path.join(self.tmpdir, 'thumbnails', 'normal')
        self.dst      = os.path.join(self.tmpdir, 'out.png')
        # A real source file so os.stat() in write_thumbnail() succeeds.
        self.src      = os.path.join(self.tmpdir, 'photo.cr3')
        open(self.src, 'wb').close()
        self.src_uri  = 'file://' + self.src
        self.xdg_path = os.path.join(
            self.xdg_dir,
            hashlib.md5(self.src_uri.encode()).hexdigest() + '.png',
        )
        os.makedirs(self.xdg_dir, exist_ok=True)

    def tearDown(self):
        import shutil
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def _argv(self, size=256):
        return ['cr3-thumbnailer', str(size), self.src_uri, self.dst]

    def _env(self):
        return {
            'XDG_RUNTIME_DIR': self.tmpdir,
            'XDG_CACHE_HOME':  self.tmpdir,
        }

    # ── 1. XDG cache hit ──────────────────────────────────────────────────────

    def test_xdg_hit_exits_0(self):
        _make_png(self.xdg_path)
        code = _run_main(self._argv(), self._env())
        self.assertEqual(code, 0)

    def test_xdg_hit_writes_output(self):
        _make_png(self.xdg_path)
        _run_main(self._argv(), self._env())
        self.assertTrue(os.path.exists(self.dst))

    def test_xdg_hit_writes_uri_metadata(self):
        _make_png(self.xdg_path)
        _run_main(self._argv(), self._env())
        pb = GdkPixbuf.Pixbuf.new_from_file(self.dst)
        self.assertEqual(pb.get_option('tEXt::Thumb::URI'), self.src_uri)

    def test_xdg_hit_writes_mtime_metadata(self):
        _make_png(self.xdg_path)
        _run_main(self._argv(), self._env())
        pb = GdkPixbuf.Pixbuf.new_from_file(self.dst)
        self.assertIsNotNone(pb.get_option('tEXt::Thumb::MTime'))

    def test_xdg_hit_scales_down_large_image(self):
        _make_png(self.xdg_path, width=800, height=600)
        _run_main(self._argv(size=256), self._env())
        pb = GdkPixbuf.Pixbuf.new_from_file(self.dst)
        self.assertLessEqual(max(pb.get_width(), pb.get_height()), 256)

    def test_xdg_hit_skips_small_image_scaling(self):
        _make_png(self.xdg_path, width=100, height=80)
        _run_main(self._argv(size=256), self._env())
        pb = GdkPixbuf.Pixbuf.new_from_file(self.dst)
        self.assertEqual(pb.get_width(), 100)
        self.assertEqual(pb.get_height(), 80)

    def test_corrupted_xdg_cache_falls_through(self):
        # Write garbage to the xdg cache path; should not exit 0 on this alone.
        with open(self.xdg_path, 'wb') as f:
            f.write(b'not a png')
        # No socket → STATUS returns None → falls through to exiftool → exits 1
        code = _run_main(self._argv(), self._env())
        self.assertEqual(code, 1)

    # ── 2b. Remote routing ─────────────────────────────────────────────────────

    def test_remote_status_sends_thumbnail_command(self):
        received = []
        def record(cmd): received.append(cmd)
        _ipc_server(self.sock, {
            f'STATUS {self.src}': 'remote',
            f'THUMBNAIL {self.src}': 'error',
        }, extra_action=record)
        _run_main(self._argv(), self._env())
        self.assertIn(f'THUMBNAIL {self.src}', received)

    def test_remote_thumbnail_ok_with_cache_exits_0(self):
        def write_cache(cmd):
            if cmd.startswith('THUMBNAIL '):
                _make_png(self.xdg_path)
        _ipc_server(self.sock, {
            f'STATUS {self.src}': 'remote',
            f'THUMBNAIL {self.src}': 'ok',
        }, extra_action=write_cache)
        code = _run_main(self._argv(), self._env())
        self.assertEqual(code, 0)

    def test_remote_thumbnail_ok_missing_cache_exits_1(self):
        # Daemon says 'ok' but doesn't write the cache file.
        _ipc_server(self.sock, {
            f'STATUS {self.src}': 'remote',
            f'THUMBNAIL {self.src}': 'ok',
        })
        code = _run_main(self._argv(), self._env())
        self.assertEqual(code, 1)

    def test_remote_thumbnail_error_exits_1(self):
        _ipc_server(self.sock, {
            f'STATUS {self.src}': 'remote',
            f'THUMBNAIL {self.src}': 'error',
        })
        code = _run_main(self._argv(), self._env())
        self.assertEqual(code, 1)

    def test_downloading_status_treated_as_remote(self):
        _ipc_server(self.sock, {
            f'STATUS {self.src}': 'downloading',
            f'THUMBNAIL {self.src}': 'error',
        })
        code = _run_main(self._argv(), self._env())
        self.assertEqual(code, 1)

    # ── 2a. Local routing ─────────────────────────────────────────────────────

    def test_local_exiftool_success_exits_0(self):
        jpeg = _make_jpeg_bytes()
        result = subprocess.CompletedProcess([], 0, stdout=jpeg, stderr=b'')
        _ipc_server(self.sock, {f'STATUS {self.src}': 'synced'})
        with patch('subprocess.run', return_value=result):
            code = _run_main(self._argv(), self._env())
        self.assertEqual(code, 0)

    def test_local_exiftool_writes_output(self):
        jpeg = _make_jpeg_bytes()
        result = subprocess.CompletedProcess([], 0, stdout=jpeg, stderr=b'')
        _ipc_server(self.sock, {f'STATUS {self.src}': 'kept'})
        with patch('subprocess.run', return_value=result):
            _run_main(self._argv(), self._env())
        self.assertTrue(os.path.exists(self.dst))

    def test_local_exiftool_missing_exits_1(self):
        _ipc_server(self.sock, {f'STATUS {self.src}': 'synced'})
        with patch('subprocess.run', side_effect=OSError('not found')):
            code = _run_main(self._argv(), self._env())
        self.assertEqual(code, 1)

    def test_local_exiftool_nonzero_exits_1(self):
        result = subprocess.CompletedProcess([], 1, stdout=b'', stderr=b'err')
        _ipc_server(self.sock, {f'STATUS {self.src}': 'synced'})
        with patch('subprocess.run', return_value=result):
            code = _run_main(self._argv(), self._env())
        self.assertEqual(code, 1)

    def test_local_exiftool_empty_output_exits_1(self):
        result = subprocess.CompletedProcess([], 0, stdout=b'', stderr=b'')
        _ipc_server(self.sock, {f'STATUS {self.src}': 'synced'})
        with patch('subprocess.run', return_value=result):
            code = _run_main(self._argv(), self._env())
        self.assertEqual(code, 1)

    def test_local_exiftool_timeout_exits_1(self):
        _ipc_server(self.sock, {f'STATUS {self.src}': 'cached'})
        with patch('subprocess.run', side_effect=subprocess.TimeoutExpired('exiftool', 30)):
            code = _run_main(self._argv(), self._env())
        self.assertEqual(code, 1)

    # ── None / daemon-down fallthrough ────────────────────────────────────────

    def test_none_status_falls_through_to_exiftool(self):
        # No socket → STATUS returns None → treated as non-remote → exiftool path.
        jpeg = _make_jpeg_bytes()
        result = subprocess.CompletedProcess([], 0, stdout=jpeg, stderr=b'')
        with patch('subprocess.run', return_value=result):
            code = _run_main(self._argv(), self._env())
        self.assertEqual(code, 0)

    def test_unknown_status_falls_through_to_exiftool(self):
        _ipc_server(self.sock, {f'STATUS {self.src}': 'unknown'})
        jpeg = _make_jpeg_bytes()
        result = subprocess.CompletedProcess([], 0, stdout=jpeg, stderr=b'')
        with patch('subprocess.run', return_value=result):
            code = _run_main(self._argv(), self._env())
        self.assertEqual(code, 0)

    # ── URI decoding ──────────────────────────────────────────────────────────

    def test_percent_encoded_uri_decoded_for_ipc(self):
        src_with_spaces = os.path.join(self.tmpdir, 'my photo.cr3')
        open(src_with_spaces, 'wb').close()
        encoded_uri = 'file://' + src_with_spaces.replace(' ', '%20')
        received = []
        def record(cmd): received.append(cmd)
        _ipc_server(self.sock, {
            f'STATUS {src_with_spaces}': 'remote',
            f'THUMBNAIL {src_with_spaces}': 'error',
        }, extra_action=record)
        xdg_path = os.path.join(
            self.xdg_dir,
            hashlib.md5(encoded_uri.encode()).hexdigest() + '.png',
        )
        _run_main(['cr3-thumbnailer', '256', encoded_uri,
                   os.path.join(self.tmpdir, 'out2.png')], self._env())
        self.assertIn(f'STATUS {src_with_spaces}', received)


if __name__ == '__main__':
    unittest.main()
