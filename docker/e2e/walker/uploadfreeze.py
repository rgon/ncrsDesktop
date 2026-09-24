#!/usr/bin/env python3
"""The walker harness's "upload-freeze" scenario (review of 2026-09-24, step 8).

Copies a large file into the mount while the proxy throttles every PUT
(faultproxy CHUNKING=1, PUT_BPS, PUT_LATENCY_MS), and probes a cached hot file
every 100 ms meanwhile: stat, a 64 KiB read, and a listing of its directory. A
daemon that PUTs each filled 10 MB chunk on the FUSE dispatch thread (from
write()) freezes every probe for as long as a chunk takes to upload; one that
uploads from a pool keeps the probes fast. Afterwards the uploaded file is read
back from the server, past the mount, and must hash to what was copied.

Usage: uploadfreeze.py <mount> <results-dir> <done-flag>
Env:   UPLOAD_MB (200), HOT_DIR (probe/hot), HOT_FILE (hot.bin), DAV_URL,
       WEBDAV_USER/WEBDAV_PASS, PROBE_P99_MS (200), PROBE_MAX_MS (1000),
       UPLOAD_WAIT_S (600: how long the server copy may take to appear)
Writes <results-dir>/uploadfreeze.json; sampler.py summarize turns it into criteria.
"""
import base64
import errno
import hashlib
import json
import os
import sys
import threading
import time
import urllib.request

MOUNT, RES, DONE = sys.argv[1], sys.argv[2], sys.argv[3]
HOT_DIR = os.path.join(MOUNT, os.environ.get("HOT_DIR", "probe/hot"))
HOT_FILE = os.path.join(HOT_DIR, os.environ.get("HOT_FILE", "hot.bin"))
UPLOAD_MB = int(os.environ.get("UPLOAD_MB", "200"))
DAV_URL = os.environ.get("DAV_URL", "http://faultproxy:8080/remote.php/dav/files/testuser/")
AUTH = base64.b64encode(("%s:%s" % (os.environ.get("WEBDAV_USER", "testuser"),
                                    os.environ.get("WEBDAV_PASS", "testpass"))).encode()).decode()
SRC = "/tmp/uploadfreeze.bin"
DEST_REL = "upload/big.bin"

probes = []          # (t since start, ms, error or None, per-step ms of a slow probe)
inflight = {}
last_steps = {}
lock = threading.Lock()
stop = threading.Event()
T0 = time.time()


def probe_once():
    steps = {}
    t = time.time()
    os.stat(HOT_FILE)
    steps["stat"] = time.time() - t
    t = time.time()
    with open(HOT_FILE, "rb") as f:
        f.read(64 * 1024)
    steps["open+read"] = time.time() - t
    t = time.time()
    os.listdir(HOT_DIR)
    steps["ls"] = time.time() - t
    last_steps.clear()
    last_steps.update({k: round(v * 1000, 1) for k, v in steps.items()})


def probe_loop():
    while not stop.is_set():
        t = time.time()
        with lock:
            inflight["probe"] = t
        err = None
        try:
            probe_once()
        except OSError as e:
            err = errno.errorcode.get(e.errno, str(e.errno))
        with lock:
            inflight.pop("probe", None)
            ms = (time.time() - t) * 1000
            probes.append((round(t - T0, 2), round(ms, 2), err, dict(last_steps) if ms > 200 else None))
        time.sleep(max(0.0, 0.1 - (time.time() - t)))


def make_source():
    h = hashlib.sha256()
    with open(SRC, "wb") as f:
        for _ in range(UPLOAD_MB):
            b = os.urandom(1024 * 1024)
            h.update(b)
            f.write(b)
    return h.hexdigest()


def server_sha():
    req = urllib.request.Request(DAV_URL + DEST_REL, headers={"Authorization": "Basic " + AUTH})
    h, n = hashlib.sha256(), 0
    with urllib.request.urlopen(req, timeout=120) as r:
        while True:
            b = r.read(1024 * 1024)
            if not b:
                break
            h.update(b)
            n += len(b)
    return n, h.hexdigest()


def pct(xs, q):
    return xs[min(len(xs) - 1, int(len(xs) * q))] if xs else None


def main():
    want = make_source()
    os.listdir(HOT_DIR)
    held = os.open(HOT_DIR, os.O_RDONLY | os.O_DIRECTORY)
    with open(HOT_FILE, "rb") as f:
        f.read(64 * 1024)  # warm
    os.makedirs(os.path.join(MOUNT, "upload"), exist_ok=True)
    threading.Thread(target=probe_loop, daemon=True).start()
    time.sleep(1)

    t = time.time()
    copy_err = None
    try:
        with open(SRC, "rb") as src, open(os.path.join(MOUNT, DEST_REL), "wb") as dst:
            while True:
                b = src.read(1024 * 1024)
                if not b:
                    break
                dst.write(b)
    except OSError as e:
        copy_err = errno.errorcode.get(e.errno, str(e.errno))
    copy_s = round(time.time() - t, 2)
    time.sleep(1)
    stop.set()
    time.sleep(0.3)
    now = time.time()
    with lock:
        stuck = {k: round(now - v, 2) for k, v in inflight.items()}
        lat = sorted(p[1] for p in probes)
        if "probe" in stuck:
            lat.append(stuck["probe"] * 1000)
            lat.sort()
        probe_errors = [p[2] for p in probes if p[2]]
        worst = sorted(probes, key=lambda p: -p[1])[:10]
    os.close(held)

    # The server copy: the finish is asynchronous, so wait for it to land.
    got, got_len, tries = None, None, 0
    deadline = time.time() + float(os.environ.get("UPLOAD_WAIT_S", "600"))
    while time.time() < deadline:
        tries += 1
        try:
            got_len, got = server_sha()
            if got == want:
                break
        except Exception:
            pass
        time.sleep(2)
    try:
        os.unlink(SRC)
    except OSError:
        pass

    p99_max = float(os.environ.get("PROBE_P99_MS", "200"))
    max_max = float(os.environ.get("PROBE_MAX_MS", "1000"))
    out = {
        "upload_mb": UPLOAD_MB,
        "copy_s": copy_s,
        "copy_error": copy_err,
        "probes": len(lat),
        "probe_p50_ms": pct(lat, 0.50),
        "probe_p99_ms": pct(lat, 0.99),
        "probe_max_ms": lat[-1] if lat else None,
        "probe_errors": len(probe_errors),
        "probe_error_kinds": sorted(set(probe_errors)),
        "worst_probes": worst,
        "still_blocked_at_end_s": stuck,
        "server_len": got_len,
        "server_sha256": got,
        "want_sha256": want,
        "hash_checks": tries,
        "criteria": [
            ["copy completed", copy_err is None, copy_err],
            ["hot-file probe p99 < %d ms" % p99_max, bool(lat) and pct(lat, 0.99) < p99_max, pct(lat, 0.99)],
            ["hot-file probe max < %d ms" % max_max, bool(lat) and lat[-1] < max_max, lat[-1] if lat else None],
            ["hot-file probes never failed", not probe_errors, sorted(set(probe_errors))],
            ["server copy hashes to what was written", got == want, {"len": got_len, "sha256": got}],
        ],
    }
    with open(os.path.join(RES, "uploadfreeze.json"), "w") as f:
        json.dump(out, f, indent=1)
    print("[uploadfreeze] %d MB in %ss (err=%s) probes=%d p50=%sms p99=%sms max=%sms errors=%d | server %s bytes, hash %s" % (
        UPLOAD_MB, copy_s, copy_err, out["probes"], out["probe_p50_ms"], out["probe_p99_ms"], out["probe_max_ms"],
        out["probe_errors"], got_len, "match" if got == want else "MISMATCH"))
    open(DONE, "w").close()
    os._exit(0)


if __name__ == "__main__":
    main()
