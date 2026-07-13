"""
Tests for the ncrs-thumbnailer script.

Run with:  python3 -m pytest shell_integration/thumbnailer/test_ncrs_thumbnailer.py -v
       or: python3 -m unittest shell_integration/thumbnailer/test_ncrs_thumbnailer.py

Requires: python3-gi, gir1.2-gdkpixbuf-2.0  (same deps as the thumbnailer itself)
"""
import hashlib
import importlib.util
import importlib.machinery
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

# ── Import thumbnailer as a module ────────────────────────────────────────────
_SCRIPT = os.path.join(os.path.dirname(__file__), 'ncrs-thumbnailer')
_spec = importlib.util.spec_from_loader(
    'ncrs_thumbnailer',
    importlib.machinery.SourceFileLoader('ncrs_thumbnailer', _SCRIPT),
)
_mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_mod)
ipc  = _mod.ipc
main = _mod.main


# ── Helpers ───────────────────────────────────────────────────────────────────

def _make_png(path, width=100, height=80):
    pb = GdkPixbuf.Pixbuf.new(GdkPixbuf.Colorspace.RGB, False, 8, width, height)
    pb.fill(0xaabbccff)
    pb.savev(path, 'png', [], [])


def _ipc_server(sock_path, responses, extra_action=None):
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
    with patch.object(sys, 'argv', argv), \
         patch.dict(os.environ, env_overrides or {}, clear=False):
        try:
            main()
            return 0
        except SystemExit as e:
            return int(e.code) if e.code is not None else 0


# ── Tests ─────────────────────────────────────────────────────────────────────

class TestNcrsThumbnailer(unittest.TestCase):

    def setUp(self):
        self.tmpdir  = tempfile.mkdtemp(prefix='ncrs_doc_thumb_test_')
        self.sock    = os.path.join(self.tmpdir, 'ncrs.sock')
        self.xdg_dir = os.path.join(self.tmpdir, 'thumbnails', 'normal')
        self.dst     = os.path.join(self.tmpdir, 'out.png')
        self.src     = os.path.join(self.tmpdir, 'document.pdf')
        open(self.src, 'wb').close()
        self.src_uri = 'file://' + self.src
        self.xdg_path = os.path.join(
            self.xdg_dir,
            hashlib.md5(self.src_uri.encode()).hexdigest() + '.png',
        )
        os.makedirs(self.xdg_dir, exist_ok=True)

    def tearDown(self):
        import shutil
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def _argv(self, size=256):
        return ['ncrs-thumbnailer', str(size), self.src_uri, self.dst]

    def _env(self):
        return {'XDG_RUNTIME_DIR': self.tmpdir, 'XDG_CACHE_HOME': self.tmpdir}

    # ── 1. XDG cache hit ──────────────────────────────────────────────────────

    def test_xdg_hit_exits_0(self):
        _make_png(self.xdg_path)
        self.assertEqual(_run_main(self._argv(), self._env()), 0)

    def test_xdg_hit_writes_output(self):
        _make_png(self.xdg_path)
        _run_main(self._argv(), self._env())
        self.assertTrue(os.path.exists(self.dst))

    def test_xdg_hit_writes_uri_metadata(self):
        _make_png(self.xdg_path)
        _run_main(self._argv(), self._env())
        pb = GdkPixbuf.Pixbuf.new_from_file(self.dst)
        self.assertEqual(pb.get_option('tEXt::Thumb::URI'), self.src_uri)

    def test_xdg_hit_scales_down_large_image(self):
        _make_png(self.xdg_path, width=800, height=600)
        _run_main(self._argv(size=256), self._env())
        pb = GdkPixbuf.Pixbuf.new_from_file(self.dst)
        self.assertLessEqual(max(pb.get_width(), pb.get_height()), 256)

    def test_corrupted_xdg_cache_falls_through(self):
        with open(self.xdg_path, 'wb') as f:
            f.write(b'not a png')
        # No socket → STATUS None → local path → evince-thumbnailer → OSError → 1
        result = subprocess.CompletedProcess([], 1, stdout=b'', stderr=b'')
        with patch('subprocess.run', return_value=result):
            self.assertEqual(_run_main(self._argv(), self._env()), 1)

    # ── 2a. Remote routing ────────────────────────────────────────────────────

    def test_remote_sends_thumbnail_command(self):
        received = []
        _ipc_server(self.sock, {
            f'STATUS {self.src}': 'remote',
            f'THUMBNAIL {self.src}': 'error',
        }, extra_action=received.append)
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
        self.assertEqual(_run_main(self._argv(), self._env()), 0)

    def test_remote_thumbnail_error_exits_1(self):
        _ipc_server(self.sock, {
            f'STATUS {self.src}': 'remote',
            f'THUMBNAIL {self.src}': 'error',
        })
        self.assertEqual(_run_main(self._argv(), self._env()), 1)

    def test_downloading_treated_as_remote(self):
        _ipc_server(self.sock, {
            f'STATUS {self.src}': 'downloading',
            f'THUMBNAIL {self.src}': 'error',
        })
        self.assertEqual(_run_main(self._argv(), self._env()), 1)

    # ── 2b. Local routing: evince delegation ─────────────────────────────────

    def test_local_delegates_to_evince(self):
        called_with = []
        result = subprocess.CompletedProcess([], 0)
        def capture(*args, **kwargs):
            called_with.extend(args[0])
            return result
        _ipc_server(self.sock, {f'STATUS {self.src}': 'kept'})
        with patch('subprocess.run', side_effect=capture):
            _run_main(self._argv(size=128), self._env())
        self.assertIn('evince-thumbnailer', called_with)
        self.assertIn(self.src_uri, called_with)
        self.assertIn(self.dst, called_with)
        self.assertIn('128', called_with)

    def test_local_evince_success_exits_0(self):
        result = subprocess.CompletedProcess([], 0)
        _ipc_server(self.sock, {f'STATUS {self.src}': 'synced'})
        with patch('subprocess.run', return_value=result):
            self.assertEqual(_run_main(self._argv(), self._env()), 0)

    def test_local_evince_failure_exits_1(self):
        result = subprocess.CompletedProcess([], 1)
        _ipc_server(self.sock, {f'STATUS {self.src}': 'cached'})
        with patch('subprocess.run', return_value=result):
            self.assertEqual(_run_main(self._argv(), self._env()), 1)

    def test_local_evince_not_installed_exits_1(self):
        _ipc_server(self.sock, {f'STATUS {self.src}': 'kept'})
        with patch('subprocess.run', side_effect=OSError('not found')):
            self.assertEqual(_run_main(self._argv(), self._env()), 1)

    def test_local_evince_timeout_exits_1(self):
        _ipc_server(self.sock, {f'STATUS {self.src}': 'synced'})
        with patch('subprocess.run', side_effect=subprocess.TimeoutExpired('evince-thumbnailer', 60)):
            self.assertEqual(_run_main(self._argv(), self._env()), 1)

    def test_no_daemon_falls_through_to_evince(self):
        # No socket → STATUS returns None → treated as non-remote → evince path.
        result = subprocess.CompletedProcess([], 0)
        with patch('subprocess.run', return_value=result):
            code = _run_main(self._argv(), self._env())
        self.assertEqual(code, 0)

    # ── URI decoding ──────────────────────────────────────────────────────────

    def test_percent_encoded_uri_decoded_for_ipc(self):
        src = os.path.join(self.tmpdir, 'my report.pdf')
        open(src, 'wb').close()
        uri = 'file://' + src.replace(' ', '%20')
        received = []
        _ipc_server(self.sock, {
            f'STATUS {src}': 'remote',
            f'THUMBNAIL {src}': 'error',
        }, extra_action=received.append)
        _run_main(['ncrs-thumbnailer', '256', uri,
                   os.path.join(self.tmpdir, 'out2.png')], self._env())
        self.assertIn(f'STATUS {src}', received)


if __name__ == '__main__':
    unittest.main()
