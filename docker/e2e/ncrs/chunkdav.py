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

WebDAV token locks (LOCK / UNLOCK) are kept here the way Nextcloud's files_lock keeps
them: the timeout counts from when the lock was taken (a refresh replaces it), a PUT,
MOVE or DELETE of a locked file without its token in the If header gets 423, a MOVE
onto it drops the lock with the file it replaces, and PROPFIND reports it (nc:lock,
lock-owner-type 2). Logged as LOCK / REFRESH / UNLOCK / LOCKED423.

With --fail-chunk-file PATH, a network blip can be armed while the proxy runs: while
PATH exists and holds "STATUS COUNT", the next COUNT chunk PUTs after a session's first
chunk (index 1 on, so an early upload is hit and not a whole-file upload's chunk 0) get
STATUS (logged as FAILC); the file is removed once COUNT is used up. A chunk PUT names
no file, hence a file armed right before the write it is meant for.

With --expire-session MARKER:COUNT, the first COUNT assembling MOVEs whose Destination
contains MARKER find their session gone, as Nextcloud's cleanup of uploads unfinished
for a day leaves it: the session is deleted and the MOVE gets 404 (logged as EXPIRED).

Usage: chunkdav.py <listen-port> <upstream-host> <log-file>
                   [--slow MARKER:SECONDS] [--slow-write MARKER:SECONDS]
                   [--fail-write MARKER:STATUS:COUNT] [--locked MARKER:OWNER:TYPE]
                   [--fail-chunk-file PATH] [--expire-session MARKER:COUNT]
"""
import base64
import http.client
import http.server
import os
import re
import shutil
import sys
import threading
import time
import urllib.parse
import uuid

PORT, UPSTREAM, LOG = int(sys.argv[1]), sys.argv[2], sys.argv[3]
SLOW_MARK, SLOW_SECS = None, 0.0
WSLOW_MARK, WSLOW_SECS = None, 0.0
FAIL_MARK, FAIL_STATUS, FAIL_LEFT = None, 0, 0
LOCK_MARK, LOCK_OWNER, LOCK_TYPE = None, "", "0"
FAIL_CHUNK_FILE = None
EXPIRE_MARK, EXPIRE_LEFT = None, 0
for flag, spec in zip(sys.argv[4::2], sys.argv[5::2]):
    if flag == "--fail-chunk-file":
        FAIL_CHUNK_FILE = spec
        continue
    if flag == "--expire-session":
        EXPIRE_MARK, count = spec.rsplit(":", 1)
        EXPIRE_LEFT = int(count)
        continue
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
LOCKS = {}   # decoded path -> {"token", "created", "timeout", "owner", "user"}


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
        m = re.search(r'\["([^"]*)"\]', self.headers.get("If", ""))
        want = m.group(1) if m else self.headers.get("If-Match")
        want = want.replace("W/", "").strip('"') if want else None
        if want is None:
            return False
        have = self.current_etag(path)
        if have == want:
            return False
        log("PRECOND %s %s wants %s, is %s" % (self.command, urllib.parse.unquote(path), want, have))
        return True

    def live_lock(self, path):
        lk = LOCKS.get(urllib.parse.unquote(path))
        if lk and time.time() < lk["created"] + lk["timeout"]:
            return lk
        LOCKS.pop(urllib.parse.unquote(path), None)
        return None

    def lock_refused(self, path):
        """A write of a token-locked file without that token."""
        lk = self.live_lock(path)
        if lk and lk["token"] not in self.headers.get("If", ""):
            log("LOCKED423 %s %s" % (self.command, urllib.parse.unquote(path)))
            return True
        return False

    def dav_lock(self, path, data):
        key = urllib.parse.unquote(path)
        secs = re.search(r"Second-(\d+)", self.headers.get("Timeout", ""))
        secs = int(secs.group(1)) if secs else 0
        lk = self.live_lock(path)
        if lk and lk["token"] in self.headers.get("If", ""):
            lk["timeout"] = secs            # files_lock: counted from when it was taken
            log("REFRESH %s %d" % (key, secs))
            return self.reply(200, [("Content-Type", "application/xml")], b"<?xml version=\"1.0\"?><d:prop xmlns:d=\"DAV:\"/>")
        if lk:
            return self.reply(423)
        owner = re.search(rb"<(?:\w+:)?owner>(.*?)</(?:\w+:)?owner>", data, re.S)
        token = "opaquelocktoken:%s" % uuid.uuid4()
        user = base64.b64decode(self.headers.get("Authorization", "Basic ")[6:] or b"").split(b":")[0].decode()
        LOCKS[key] = {"token": token, "created": time.time(), "timeout": secs,
                      "owner": owner.group(1).decode() if owner else "", "user": user}
        log("LOCK %s %d %s" % (key, secs, token))
        return self.reply(200, [("Lock-Token", "<%s>" % token), ("Content-Type", "application/xml")],
                          b"<?xml version=\"1.0\"?><d:prop xmlns:d=\"DAV:\"/>")

    def dav_unlock(self, path):
        key = urllib.parse.unquote(path)
        lk = self.live_lock(path)
        token = self.headers.get("Lock-Token", "").strip().strip("<>")
        if lk and lk["token"] == token:
            LOCKS.pop(key, None)
            log("UNLOCK %s" % key)
            return self.reply(204)
        return self.reply(409)

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
        if self.command == "LOCK":
            return self.dav_lock(path, data)
        if self.command == "UNLOCK":
            return self.dav_unlock(path)
        if self.command in ("PUT", "DELETE", "MOVE") and not path.startswith(UPLOADS) and self.lock_refused(path):
            return self.reply(423)
        if self.command == "MOVE" and dest and self.lock_refused(dest):
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
        if self.command == "MOVE" and status in (200, 201, 204) and dest:
            # The file the MOVE replaced took its lock with it (Nextcloud locks by file id).
            LOCKS.pop(urllib.parse.unquote(dest), None)
        if self.command == "DELETE" and status in (200, 204):
            LOCKS.pop(urllib.parse.unquote(path), None)
        if (LOCK_MARK or LOCKS) and self.command == "PROPFIND" and status == 207:
            rd = self.inject_locks(rd)
        self.reply(status, rh, rd)

    def inject_locks(self, body):
        """Adds Nextcloud's lock properties to every response for a locked file."""
        text = body.decode("utf-8")
        ns = 'xmlns:nc="http://nextcloud.org/ns"'
        props = ('<nc:lock %s>1</nc:lock><nc:lock-owner %s>%s</nc:lock-owner>'
                 '<nc:lock-owner-displayname %s>%s</nc:lock-owner-displayname>'
                 '<nc:lock-owner-type %s>%s</nc:lock-owner-type>') % (ns, ns, LOCK_OWNER, ns, LOCK_OWNER, ns, LOCK_TYPE)

        def token_props(lk):
            return ('<nc:lock %s>1</nc:lock><nc:lock-owner %s>%s</nc:lock-owner>'
                    '<nc:lock-owner-displayname %s>%s</nc:lock-owner-displayname>'
                    '<nc:lock-owner-type %s>2</nc:lock-owner-type>') % (ns, ns, lk["user"], ns, lk["owner"], ns)

        def one(m):
            r = m.group(0)
            href = re.search(r"<(?:\w+:)?href>([^<]*)</", r)
            if not href:
                return r
            where = urllib.parse.unquote(href.group(1))
            if LOCK_MARK and LOCK_MARK in where:
                add = props
            elif self.live_lock(href.group(1)):
                add = token_props(self.live_lock(href.group(1)))
            else:
                return r
            return re.sub(r"(<(?:\w+:)?prop\b[^>]*>)", lambda p: p.group(1) + add, r, count=1)
        return re.sub(r"<(\w+:)?response\b[^>]*>.*?</(\w+:)?response>", one, text, flags=re.S).encode("utf-8")

    def armed_chunk_failure(self, index):
        """The status an armed --fail-chunk-file gives this chunk PUT, None if not armed."""
        if not FAIL_CHUNK_FILE or int(index) < 1:
            return None
        with log_lock:
            try:
                status, count = open(FAIL_CHUNK_FILE).read().split()
            except (OSError, ValueError):
                return None
            left = int(count) - 1
            if left > 0:
                with open(FAIL_CHUNK_FILE, "w") as f:
                    f.write("%s %d\n" % (status, left))
            else:
                os.remove(FAIL_CHUNK_FILE)
            return int(status)

    def uploads(self, rel, data):
        # rel = "<user>/<transfer-id>[/<chunk>|/.file]"
        global EXPIRE_LEFT
        parts = [urllib.parse.unquote(p) for p in rel.strip("/").split("/")]
        if len(parts) < 2 or any(p in ("", ".", "..") for p in parts):
            return self.reply(400)
        session = os.path.join(STORE, parts[0], parts[1])
        log("%s %s %d" % (self.command, "/".join(parts[1:]), len(data)))
        if self.command == "MKCOL" and len(parts) == 2:
            os.makedirs(session, exist_ok=True)
            return self.reply(201)
        if self.command == "PUT" and len(parts) == 3 and re.fullmatch(r"\d+", parts[2]):
            failed = self.armed_chunk_failure(parts[2])
            if failed:
                log("FAILC %s -> %d" % ("/".join(parts[1:]), failed))
                return self.reply(failed)
            if not os.path.isdir(session):
                return self.reply(404)
            with open(os.path.join(session, parts[2]), "wb") as f:
                f.write(data)
            return self.reply(201)
        if self.command == "DELETE" and len(parts) == 2:
            shutil.rmtree(session, ignore_errors=True)
            return self.reply(204)
        if self.command == "MOVE" and len(parts) == 3 and parts[2] == ".file":
            target = urllib.parse.unquote(urllib.parse.urlsplit(self.headers.get("Destination", "")).path)
            with log_lock:
                expire = EXPIRE_MARK is not None and EXPIRE_MARK in target and EXPIRE_LEFT > 0
                if expire:
                    EXPIRE_LEFT -= 1
            if expire:
                shutil.rmtree(session, ignore_errors=True)
                log("EXPIRED %s -> %s" % (parts[1], target))
                return self.reply(404)
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
            if self.lock_refused(dest):
                return self.reply(423)
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
