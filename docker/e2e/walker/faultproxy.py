#!/usr/bin/env python3
"""Fault-injecting WebDAV reverse proxy for the ncrs walker e2e harness.

Stdlib only. Sits between the ncrs daemon and the rclone WebDAV server and
answers a configurable share of `PROPFIND Depth: 1` requests with HTTP 500, so a
filesystem walk sees the same "server erroring on listings" pattern that leaked
threads in production.

Env:
  UPSTREAM          host:port of the real WebDAV server (default webdav:80)
  LISTEN_PORT       port to listen on (default 8080)
  FAULT_RATE        0..1 share of PROPFIND Depth:1 answered 500 (default 0)
  FAULT_MODE        hash   - deterministic: sha1(path) < rate, same dir always fails
                    random - independent coin flip per request
                    burst  - alternating BURST_SECS windows: ~all-500 (BURST_RATE)
                             then pass-through
  BURST_SECS        burst window length in seconds (default 10)
  BURST_RATE        fault share inside a burst window (default 0.95)
  FAULT_DEPTHS      comma list of PROPFIND Depth values subject to FAULT_RATE/burst
                    (default "1"; "0,1" also fails the per-dir etag probes the
                    daemon sends as Depth:0)
  FAULT_PATH_SUBSTR PROPFINDs whose path contains this always get 500
  FAULT_ROOT        1 = the root collection may be faulted too (Depth 0 and 1);
                    by default it never is, so the daemon's connectivity probe and
                    the mount's top-level listing stay healthy
  FAULT_MIN_DEPTH   only paths at least this many levels below the root collection
                    are faulted by FAULT_RATE/burst (default 2, so top-level dirs
                    like /tree stay listable and a hash-mode walk still reaches
                    the bulk of the tree; 0 = any depth)
  LATENCY_MS        delay added before every proxied reply (default 0)
  FAULT_ARMED       1 (default) = faults active from the start; 0 = no faults
                    until POST /__arm (run_walker.sh arms after the mount is up,
                    so FAULT_ROOT=1 tests a *running* daemon, not mount-time)
  QUIET             1 = no per-request log line

Control endpoints (not proxied):
  GET  /__stats   JSON counters
  POST /__reset   zero all counters (and restart the burst clock)
  POST /__arm     enable fault injection (and restart the burst clock)
"""
import hashlib
import http.client
import json
import os
import random
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import unquote, urlsplit

UPSTREAM = os.environ.get("UPSTREAM", "webdav:80")
UP_HOST, _, UP_PORT = UPSTREAM.partition(":")
UP_PORT = int(UP_PORT or 80)
LISTEN_PORT = int(os.environ.get("LISTEN_PORT", "8080"))
FAULT_RATE = float(os.environ.get("FAULT_RATE", "0") or 0)
FAULT_MODE = os.environ.get("FAULT_MODE", "hash") or "hash"
BURST_SECS = float(os.environ.get("BURST_SECS", "10") or 10)
BURST_RATE = float(os.environ.get("BURST_RATE", "0.95") or 0.95)
FAULT_PATH_SUBSTR = os.environ.get("FAULT_PATH_SUBSTR", "")
FAULT_ROOT = os.environ.get("FAULT_ROOT", "0") == "1"
FAULT_DEPTHS = {d.strip() for d in os.environ.get("FAULT_DEPTHS", "1").split(",") if d.strip()}
FAULT_MIN_DEPTH = int(os.environ.get("FAULT_MIN_DEPTH", "2") or 0)
LATENCY_MS = float(os.environ.get("LATENCY_MS", "0") or 0)
ROOT_PREFIX = os.environ.get("ROOT_PREFIX", "/remote.php/dav/files/testuser")
QUIET = os.environ.get("QUIET", "0") == "1"
ARMED = {"on": os.environ.get("FAULT_ARMED", "1") == "1", "at": time.time()}

HOP_BY_HOP = {
    "connection", "keep-alive", "proxy-authenticate", "proxy-authorization",
    "te", "trailers", "transfer-encoding", "upgrade",
}
CHUNK = 64 * 1024


class Stats:
    def __init__(self):
        self.lock = threading.Lock()
        self.reset()

    def reset(self):
        with getattr(self, "lock", threading.Lock()):
            self.started = time.time()
            self.total = 0
            self.by_method = {}
            self.by_status = {}
            self.propfind = 0
            self.propfind_depth = {}
            self.injected_500 = 0
            self.per_path = {}
            self.inflight = 0
            self.max_inflight = 0
            self.upstream_errors = 0

    def begin(self, method):
        with self.lock:
            self.total += 1
            self.by_method[method] = self.by_method.get(method, 0) + 1
            self.inflight += 1
            self.max_inflight = max(self.max_inflight, self.inflight)
            return self.inflight

    def end(self, status):
        with self.lock:
            self.inflight -= 1
            k = str(status)
            self.by_status[k] = self.by_status.get(k, 0) + 1

    def snapshot(self):
        with self.lock:
            top = sorted(self.per_path.items(), key=lambda kv: -kv[1])[:20]
            return {
                "uptime_s": round(time.time() - self.started, 1),
                "total": self.total,
                "by_method": dict(self.by_method),
                "by_status": dict(self.by_status),
                "propfind": self.propfind,
                "propfind_by_depth": dict(self.propfind_depth),
                "injected_500": self.injected_500,
                "unique_propfind_paths": len(self.per_path),
                "top_propfind_paths": [{"path": p, "count": c} for p, c in top],
                "inflight": self.inflight,
                "max_inflight": self.max_inflight,
                "upstream_errors": self.upstream_errors,
                "config": {
                    "fault_rate": FAULT_RATE, "fault_mode": FAULT_MODE,
                    "fault_path_substr": FAULT_PATH_SUBSTR, "fault_root": FAULT_ROOT,
                    "latency_ms": LATENCY_MS, "fault_min_depth": FAULT_MIN_DEPTH,
                    "fault_depths": sorted(FAULT_DEPTHS), "armed": ARMED["on"], "burst_secs": BURST_SECS,
                    "burst_rate": BURST_RATE,
                },
            }


STATS = Stats()


def norm_path(raw):
    p = unquote(urlsplit(raw).path)
    return p.rstrip("/") or "/"


def should_fault(method, depth, path):
    if method != "PROPFIND" or not ARMED["on"]:
        return False
    is_root = path == ROOT_PREFIX.rstrip("/")
    if is_root and not FAULT_ROOT:
        return False
    if FAULT_PATH_SUBSTR and FAULT_PATH_SUBSTR in path:
        return True
    if depth not in FAULT_DEPTHS and not (is_root and FAULT_ROOT):
        return False
    rel = path[len(ROOT_PREFIX.rstrip("/")):].strip("/")
    if not is_root and rel.count("/") + 1 < FAULT_MIN_DEPTH:
        return False
    if FAULT_MODE == "burst":
        window = int((time.time() - max(STATS.started, ARMED["at"])) // BURST_SECS)
        return window % 2 == 0 and random.random() < BURST_RATE
    if FAULT_RATE <= 0:
        return False
    if FAULT_MODE == "random":
        return random.random() < FAULT_RATE
    # hash: deterministic per path
    h = int.from_bytes(hashlib.sha1(path.encode()).digest()[:8], "big")
    return h / 2**64 < FAULT_RATE


def log(line):
    if not QUIET:
        sys.stdout.write(line + "\n")
        sys.stdout.flush()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "faultproxy"

    def log_message(self, fmt, *args):  # silence default access log
        pass

    # --- body helpers --------------------------------------------------------
    def _iter_request_body(self):
        te = self.headers.get("Transfer-Encoding", "").lower()
        if "chunked" in te:
            while True:
                line = self.rfile.readline()
                size = int(line.split(b";", 1)[0].strip() or b"0", 16)
                if size == 0:
                    # trailers until blank line
                    while self.rfile.readline() not in (b"\r\n", b"\n", b""):
                        pass
                    return
                remaining = size
                while remaining:
                    b = self.rfile.read(min(CHUNK, remaining))
                    if not b:
                        return
                    remaining -= len(b)
                    yield b
                self.rfile.readline()  # CRLF after chunk
        else:
            n = int(self.headers.get("Content-Length", "0") or 0)
            while n > 0:
                b = self.rfile.read(min(CHUNK, n))
                if not b:
                    return
                n -= len(b)
                yield b

    def _drain_body(self):
        for _ in self._iter_request_body():
            pass

    def _send_simple(self, code, body, ctype="text/plain"):
        data = body.encode() if isinstance(body, str) else body
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(data)

    # --- dispatch -------------------------------------------------------------
    def handle_one_request(self):
        try:
            super().handle_one_request()
        except (ConnectionResetError, BrokenPipeError):
            self.close_connection = True

    def __getattr__(self, name):
        if name.startswith("do_"):
            return self._proxy
        raise AttributeError(name)

    def _proxy(self):
        method = self.command
        path = norm_path(self.path)
        if path == "/__stats" and method == "GET":
            self._drain_body()
            self._send_simple(200, json.dumps(STATS.snapshot(), indent=1), "application/json")
            return
        if path == "/__arm" and method == "POST":
            self._drain_body()
            ARMED["on"], ARMED["at"] = True, time.time()
            log("faults armed")
            self._send_simple(200, "armed\n")
            return
        if path == "/__reset" and method == "POST":
            self._drain_body()
            STATS.reset()
            self._send_simple(200, "reset\n")
            return

        inflight = STATS.begin(method)
        t0 = time.time()
        depth = self.headers.get("Depth", "")
        status = 0
        faulted = False
        try:
            if method == "PROPFIND":
                with STATS.lock:
                    STATS.propfind += 1
                    STATS.propfind_depth[depth or "-"] = STATS.propfind_depth.get(depth or "-", 0) + 1
                    if depth == "1":
                        STATS.per_path[path] = STATS.per_path.get(path, 0) + 1
            faulted = should_fault(method, depth, path)
            if faulted:
                self._drain_body()
                if LATENCY_MS:
                    time.sleep(LATENCY_MS / 1000.0)
                with STATS.lock:
                    STATS.injected_500 += 1
                status = 500
                self._send_simple(500, "injected fault\n")
                return
            status = self._forward(method)
        finally:
            STATS.end(status)
            log("%s inflight=%d %s %s depth=%s -> %s%s %.0fms" % (
                time.strftime("%H:%M:%S", time.localtime(t0)) + ".%03d" % int((t0 % 1) * 1000),
                inflight, method, path, depth or "-", status,
                " (injected)" if faulted else "", (time.time() - t0) * 1000))

    def _forward(self, method):
        conn = http.client.HTTPConnection(UP_HOST, UP_PORT, timeout=300)
        try:
            has_body = ("Content-Length" in self.headers and self.headers.get("Content-Length") != "0") \
                or "chunked" in self.headers.get("Transfer-Encoding", "").lower()
            conn.putrequest(method, self.path, skip_host=True, skip_accept_encoding=True)
            for k, v in self.headers.items():
                lk = k.lower()
                if lk in HOP_BY_HOP or lk == "content-length":
                    continue
                conn.putheader(k, v)
            chunked_up = False
            if has_body:
                if "Content-Length" in self.headers:
                    conn.putheader("Content-Length", self.headers["Content-Length"])
                else:
                    conn.putheader("Transfer-Encoding", "chunked")
                    chunked_up = True
            elif method in ("PUT", "POST", "PROPFIND", "PROPPATCH", "LOCK"):
                conn.putheader("Content-Length", "0")
            conn.putheader("Connection", "close")
            conn.endheaders()
            if has_body:
                for b in self._iter_request_body():
                    if chunked_up:
                        conn.send(b"%x\r\n" % len(b) + b + b"\r\n")
                    else:
                        conn.send(b)
                if chunked_up:
                    conn.send(b"0\r\n\r\n")
            resp = conn.getresponse()
        except Exception as e:  # upstream unreachable / reset
            with STATS.lock:
                STATS.upstream_errors += 1
            try:
                self._send_simple(502, "upstream error: %s\n" % e)
            except Exception:
                pass
            conn.close()
            return 502

        try:
            if LATENCY_MS:
                time.sleep(LATENCY_MS / 1000.0)
            self.send_response(resp.status, resp.reason)
            length = resp.getheader("Content-Length")
            for k, v in resp.getheaders():
                lk = k.lower()
                if lk in HOP_BY_HOP or lk == "content-length":
                    continue
                self.send_header(k, v)
            no_body = method == "HEAD" or resp.status in (204, 304) or 100 <= resp.status < 200
            if no_body:
                if length is not None:
                    self.send_header("Content-Length", length)
                self.end_headers()
            elif length is not None:
                self.send_header("Content-Length", length)
                self.end_headers()
                while True:
                    b = resp.read(CHUNK)
                    if not b:
                        break
                    self.wfile.write(b)
            else:
                # upstream was chunked or close-delimited: re-chunk to client
                self.send_header("Transfer-Encoding", "chunked")
                self.end_headers()
                while True:
                    b = resp.read1(CHUNK) if hasattr(resp, "read1") else resp.read(CHUNK)
                    if not b:
                        break
                    self.wfile.write(b"%x\r\n" % len(b) + b + b"\r\n")
                self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
            return resp.status
        finally:
            conn.close()


class Server(ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 512


def main():
    log("faultproxy listening :%d -> %s mode=%s rate=%s substr=%r root=%s min_depth=%d latency=%sms" % (
        LISTEN_PORT, UPSTREAM, FAULT_MODE, FAULT_RATE, FAULT_PATH_SUBSTR, FAULT_ROOT, FAULT_MIN_DEPTH, LATENCY_MS))
    Server(("0.0.0.0", LISTEN_PORT), Handler).serve_forever()


if __name__ == "__main__":
    main()
