#!/usr/bin/env python3
"""Nextcloud chunked-upload front for the e2e WebDAV server.

rclone has no `remote.php/dav/uploads/` collection, so against it the daemon's
streamed chunked upload never starts and every large write falls back to a
whole-file PUT. This proxy adds just that extension (MKCOL a session, PUT
numbered chunks, MOVE `.file` to assemble, DELETE to abort) and forwards every
other request to the real server unchanged, so a mount pointed at it exercises
the streaming path. Each uploads request is logged, one line each, so a
scenario can prove the chunks really went this way.

Usage: chunkdav.py <listen-port> <upstream-host> <log-file>
"""
import http.client
import http.server
import os
import re
import shutil
import sys
import threading
import urllib.parse

PORT, UPSTREAM, LOG = int(sys.argv[1]), sys.argv[2], sys.argv[3]
STORE = "/tmp/chunkdav-sessions"
UPLOADS = "/remote.php/dav/uploads/"
HOP = {"connection", "keep-alive", "transfer-encoding", "te", "trailer", "upgrade", "host"}
log_lock = threading.Lock()


def log(line):
    with log_lock, open(LOG, "a") as f:
        f.write(line + "\n")


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def body(self):
        if self.headers.get("Transfer-Encoding", "").lower() == "chunked":
            out = bytearray()
            while True:
                size = int(self.rfile.readline().split(b";")[0].strip(), 16)
                if size == 0:
                    while self.rfile.readline() not in (b"\r\n", b"\n", b""):
                        pass
                    return bytes(out)
                out += self.rfile.read(size)
                self.rfile.readline()
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n) if n else b""

    def reply(self, status, headers=(), data=b""):
        self.send_response(status)
        for k, v in headers:
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(data)

    def upstream(self, method, path, headers, data):
        conn = http.client.HTTPConnection(UPSTREAM, 80, timeout=120)
        try:
            conn.request(method, path, body=data, headers=headers)
            r = conn.getresponse()
            return r.status, [(k, v) for k, v in r.getheaders() if k.lower() not in HOP | {"content-length"}], r.read()
        finally:
            conn.close()

    def handle_one(self):
        data = self.body()
        path = urllib.parse.urlsplit(self.path).path
        if path.startswith(UPLOADS):
            return self.uploads(path[len(UPLOADS):], data)
        headers = {k: v for k, v in self.headers.items() if k.lower() not in HOP}
        headers["Host"] = UPSTREAM
        if "Destination" in headers:
            d = urllib.parse.urlsplit(headers["Destination"])
            headers["Destination"] = urllib.parse.urlunsplit(("http", UPSTREAM, d.path, d.query, ""))
        status, rh, rd = self.upstream(self.command, self.path, headers, data)
        self.reply(status, rh, rd)

    def uploads(self, rel, data):
        # rel = "<user>/<transfer-id>[/<chunk>|/.file]"
        parts = [urllib.parse.unquote(p) for p in rel.strip("/").split("/")]
        if len(parts) < 2 or any(p in ("", ".", "..") for p in parts):
            return self.reply(400)
        session = os.path.join(STORE, parts[0], parts[1])
        log("%s %s" % (self.command, "/".join(parts[1:])))
        if self.command == "MKCOL" and len(parts) == 2:
            os.makedirs(session, exist_ok=True)
            return self.reply(201)
        if self.command == "PUT" and len(parts) == 3 and re.fullmatch(r"\d+", parts[2]):
            if not os.path.isdir(session):
                return self.reply(404)
            with open(os.path.join(session, parts[2]), "wb") as f:
                f.write(data)
            return self.reply(201)
        if self.command == "DELETE" and len(parts) == 2:
            shutil.rmtree(session, ignore_errors=True)
            return self.reply(204)
        if self.command == "MOVE" and len(parts) == 3 and parts[2] == ".file":
            if not os.path.isdir(session):
                return self.reply(404)
            whole = bytearray()
            for name in sorted(os.listdir(session)):
                with open(os.path.join(session, name), "rb") as f:
                    whole += f.read()
            headers = {"Content-Type": "application/octet-stream",
                       "Authorization": self.headers.get("Authorization", "")}
            if self.headers.get("X-OC-Mtime"):
                # Nextcloud takes the file's mtime from the assembling MOVE.
                headers["X-OC-Mtime"] = self.headers["X-OC-Mtime"]
            m = re.search(r'\(\["([^"]*)"\]\)', self.headers.get("If", ""))
            if m:
                headers["If-Match"] = '"%s"' % m.group(1)
            dest = urllib.parse.urlsplit(self.headers.get("Destination", "")).path
            status, rh, rd = self.upstream("PUT", dest, headers, bytes(whole))
            if status in (200, 201, 204):
                shutil.rmtree(session, ignore_errors=True)
            log("ASSEMBLED %s %d bytes -> %s (HTTP %d)" % (parts[1], len(whole), dest, status))
            return self.reply(status, [(k, v) for k, v in rh if k.lower() == "etag"], rd)
        return self.reply(405)

    do_GET = do_HEAD = do_PUT = do_DELETE = do_MKCOL = do_MOVE = do_COPY = handle_one
    do_PROPFIND = do_PROPPATCH = do_OPTIONS = do_LOCK = do_UNLOCK = do_POST = handle_one


if __name__ == "__main__":
    os.makedirs(STORE, exist_ok=True)
    http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
