#!/usr/bin/env python3
"""Nextcloud chunked-upload front for the e2e WebDAV server.

rclone has no `remote.php/dav/uploads/` collection, so against it the daemon's
streamed chunked upload never starts and every large write falls back to a
whole-file PUT. This proxy adds just that extension (MKCOL a session, PUT
numbered chunks, MOVE `.file` to assemble, DELETE to abort) and forwards every
other request to the real server unchanged, so a mount pointed at it exercises
the streaming path. Each uploads request is logged, one line each, so a
scenario can prove the chunks really went this way.

With --slow MARKER:SECONDS, a GET or PROPFIND whose path contains MARKER is held
that long before it is forwarded (logged as SLOW), so a scenario can keep one
request deliberately stuck on the network while it checks everything else.

With --slow-write MARKER:SECONDS, a PUT or MOVE whose path or Destination contains
MARKER (the assembling MOVE of a chunked upload included) is held the same way
(logged as SLOWW), which keeps a save's upload and rename-MOVE pending for a while
the way a large file on a real network does.

Preconditions are checked here the way Nextcloud checks them, whatever the server
behind does: If-Match on GET and PUT, and the tagged `If: <url> (["etag"])` form on
a MOVE (which names the destination). A failed one is 412, logged as PRECOND.

With --fail-write MARKER:STATUS:COUNT, the first COUNT PUT/MOVE requests whose
(decoded) path or Destination contains MARKER get STATUS instead (logged as FAILW).

With --locked MARKER:OWNER:TYPE, every file whose path contains MARKER is locked on
the "server": PROPFIND reports nc:lock for it, held by OWNER with lock-owner-type
TYPE, and a PUT or MOVE onto it gets 423.

Usage: chunkdav.py <listen-port> <upstream-host> <log-file>
                   [--slow MARKER:SECONDS] [--slow-write MARKER:SECONDS]
                   [--fail-write MARKER:STATUS:COUNT] [--locked MARKER:OWNER:TYPE]
"""
import http.client
import http.server
import os
import re
import shutil
import sys
import threading
import time
import urllib.parse

PORT, UPSTREAM, LOG = int(sys.argv[1]), sys.argv[2], sys.argv[3]
SLOW_MARK, SLOW_SECS = None, 0.0
WSLOW_MARK, WSLOW_SECS = None, 0.0
FAIL_MARK, FAIL_STATUS, FAIL_LEFT = None, 0, 0
LOCK_MARK, LOCK_OWNER, LOCK_TYPE = None, "", "0"
for flag, spec in zip(sys.argv[4::2], sys.argv[5::2]):
    if flag == "--fail-write":
        FAIL_MARK, status, count = spec.rsplit(":", 2)
        FAIL_STATUS, FAIL_LEFT = int(status), int(count)
        continue
    if flag == "--locked":
        LOCK_MARK, LOCK_OWNER, LOCK_TYPE = spec.rsplit(":", 2)
        continue
    mark, _, secs = spec.rpartition(":")
    if flag == "--slow":
        SLOW_MARK, SLOW_SECS = mark, float(secs)
    elif flag == "--slow-write":
        WSLOW_MARK, WSLOW_SECS = mark, float(secs)
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

    def current_etag(self, path):
        """The upstream's etag of `path` now, None when it does not exist."""
        status, rh, _ = self.upstream("HEAD", path, {"Host": UPSTREAM, "Authorization": self.headers.get("Authorization", "")}, b"")
        if status == 404:
            return None
        tag = dict((k.lower(), v) for k, v in rh).get("etag", "")
        return tag.replace("W/", "").strip('"')

    def stored_etag(self, path, rh):
        """Nextcloud answers a PUT with the etag of the file as stored; rclone with the one
        it had before applying X-OC-Mtime, which no later request ever sees."""
        tag = self.current_etag(path)
        rh = [(k, v) for k, v in rh if k.lower() != "etag"]
        return rh + [("ETag", '"%s"' % tag)] if tag else rh

    def precondition_failed(self, path):
        """Whether this request's If-Match / tagged If header names another version than
        the one `path` (the destination, for a MOVE) is now."""
        if self.command == "MOVE":
            m = re.search(r'\(\["([^"]*)"\]\)', self.headers.get("If", ""))
            want = m.group(1) if m else None
        else:
            want = self.headers.get("If-Match")
            want = want.replace("W/", "").strip('"') if want else None
        if want is None:
            return False
        have = self.current_etag(path)
        if have == want:
            return False
        log("PRECOND %s %s wants %s, is %s" % (self.command, urllib.parse.unquote(path), want, have))
        return True

    def handle_one(self):
        global FAIL_LEFT
        data = self.body()
        path = urllib.parse.urlsplit(self.path).path
        dest = urllib.parse.urlsplit(self.headers.get("Destination", "")).path
        if WSLOW_MARK and self.command in ("PUT", "MOVE"):
            if WSLOW_MARK in path or WSLOW_MARK in dest:
                log("SLOWW %s %s" % (self.command, dest if self.command == "MOVE" else path))
                time.sleep(WSLOW_SECS)
        if FAIL_MARK and self.command in ("PUT", "MOVE"):
            where = urllib.parse.unquote(dest if self.command == "MOVE" else path)
            with log_lock:
                hit = FAIL_MARK in where and FAIL_LEFT > 0
                if hit:
                    FAIL_LEFT -= 1
            if hit:
                log("FAILW %s %s -> %d" % (self.command, where, FAIL_STATUS))
                return self.reply(FAIL_STATUS)
        if LOCK_MARK and self.command in ("PUT", "MOVE"):
            target = urllib.parse.unquote(dest if self.command == "MOVE" else path)
            if LOCK_MARK in target:
                log("LOCKED %s %s" % (self.command, target))
                return self.reply(423)
        if path.startswith(UPLOADS):
            return self.uploads(path[len(UPLOADS):], data)
        target = dest if self.command == "MOVE" else path
        if self.command in ("GET", "PUT", "MOVE") and self.precondition_failed(target):
            return self.reply(412)
        if SLOW_MARK and SLOW_MARK in path and self.command in ("GET", "PROPFIND"):
            log("SLOW %s %s" % (self.command, path))
            time.sleep(SLOW_SECS)
        if self.command == "GET":
            # Counted by the checks that a file is downloaded once, not twice.
            log("GET %s %s" % (path, self.headers.get("Range", "whole")))
        # Checked above; the server behind may read a tagged If as a lock-token test.
        headers = {k: v for k, v in self.headers.items() if k.lower() not in HOP | {"if", "if-match"}}
        headers["Host"] = UPSTREAM
        # reqwest sends header names in lower case; a MOVE whose Destination still
        # names this proxy is refused by the server (502, another host).
        dest = [k for k in headers if k.lower() == "destination"]
        if dest:
            d = urllib.parse.urlsplit(headers.pop(dest[0]))
            headers["Destination"] = urllib.parse.urlunsplit(("http", UPSTREAM, d.path, d.query, ""))
        status, rh, rd = self.upstream(self.command, self.path, headers, data)
        if self.command == "PUT" and status in (200, 201, 204):
            rh = self.stored_etag(path, rh)
        if LOCK_MARK and self.command == "PROPFIND" and status == 207:
            rd = self.inject_locks(rd)
        self.reply(status, rh, rd)

    def inject_locks(self, body):
        """Adds Nextcloud's lock properties to every response for a locked file."""
        text = body.decode("utf-8")
        ns = 'xmlns:nc="http://nextcloud.org/ns"'
        props = ('<nc:lock %s>1</nc:lock><nc:lock-owner %s>%s</nc:lock-owner>'
                 '<nc:lock-owner-displayname %s>%s</nc:lock-owner-displayname>'
                 '<nc:lock-owner-type %s>%s</nc:lock-owner-type>') % (ns, ns, LOCK_OWNER, ns, LOCK_OWNER, ns, LOCK_TYPE)

        def one(m):
            r = m.group(0)
            href = re.search(r"<(?:\w+:)?href>([^<]*)</", r)
            if not href or LOCK_MARK not in urllib.parse.unquote(href.group(1)):
                return r
            return re.sub(r"(<(?:\w+:)?prop\b[^>]*>)", lambda p: p.group(1) + props, r, count=1)
        return re.sub(r"<(\w+:)?response\b[^>]*>.*?</(\w+:)?response>", one, text, flags=re.S).encode("utf-8")

    def uploads(self, rel, data):
        # rel = "<user>/<transfer-id>[/<chunk>|/.file]"
        parts = [urllib.parse.unquote(p) for p in rel.strip("/").split("/")]
        if len(parts) < 2 or any(p in ("", ".", "..") for p in parts):
            return self.reply(400)
        session = os.path.join(STORE, parts[0], parts[1])
        log("%s %s %d" % (self.command, "/".join(parts[1:]), len(data)))
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
            dest = urllib.parse.urlsplit(self.headers.get("Destination", "")).path
            if self.precondition_failed(dest):
                return self.reply(412)
            status, rh, rd = self.upstream("PUT", dest, headers, bytes(whole))
            if status in (200, 201, 204):
                shutil.rmtree(session, ignore_errors=True)
            log("ASSEMBLED %s %d bytes -> %s (HTTP %d)" % (parts[1], len(whole), dest, status))
            if status in (200, 201, 204):
                rh = self.stored_etag(dest, rh)
            return self.reply(status, [(k, v) for k, v in rh if k.lower() == "etag"], rd)
        return self.reply(405)

    do_GET = do_HEAD = do_PUT = do_DELETE = do_MKCOL = do_MOVE = do_COPY = handle_one
    do_PROPFIND = do_PROPPATCH = do_OPTIONS = do_LOCK = do_UNLOCK = do_POST = handle_one


if __name__ == "__main__":
    os.makedirs(STORE, exist_ok=True)
    http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
