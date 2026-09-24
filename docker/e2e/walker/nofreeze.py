#!/usr/bin/env python3
"""The walker harness's "no-freeze" scenario (review of 2026-09-24).

While the walkers crawl the mount (with a small dir cache, so listings are being
evicted all the time) and another thread keeps stat'ing a file inside a
directory the proxy stalls (SLOW_PATH_SUBSTR / SLOW_MS), this probes a hot file
every 100 ms: stat, a 64 KiB read, and a listing of its directory. A daemon that
answers cache misses on the FUSE dispatch thread freezes every probe behind the
stalled listing; one that answers them from a pool keeps the probes fast.

The hot directory is held open for the whole run, so its listing is pinned in
the dir cache no matter how hard the crawl evicts.

Usage: nofreeze.py <mount> <results-dir> <done-flag>
Env:   HOT_DIR (probe/hot), HOT_FILE (hot.bin), SLOW_STAT (probe/slowdir/present.txt),
       PROBE_P99_MS (200), PROBE_MAX_MS (1000), SLOW_STAT_MAX_S (17 = PROPFIND_TIMEOUT + 2)
Writes <results-dir>/nofreeze.json; sampler.py summarize turns it into criteria.
"""
import errno
import json
import os
import sys
import threading
import time

MOUNT, RES, DONE = sys.argv[1], sys.argv[2], sys.argv[3]
HOT_DIR = os.path.join(MOUNT, os.environ.get("HOT_DIR", "probe/hot"))
HOT_FILE = os.path.join(HOT_DIR, os.environ.get("HOT_FILE", "hot.bin"))
SLOW_STAT = os.path.join(MOUNT, os.environ.get("SLOW_STAT", "probe/slowdir/present.txt"))

probes = []          # (t since start, ms, error or None)
slow = []            # (t since start, seconds, outcome)
inflight = {}        # name -> start time of a call still blocked in the kernel
last_steps = {}      # step -> ms of the most recent probe
lock = threading.Lock()
T0 = time.time()


def done():
    return os.path.exists(DONE)


def timed(name, fn):
    with lock:
        inflight[name] = time.time()
    try:
        fn()
        return None
    except OSError as e:
        return errno.errorcode.get(e.errno, str(e.errno))
    finally:
        with lock:
            inflight.pop(name, None)


def probe_once():
    # Per-step times of the last probe, so a slow one says which call stalled.
    steps = {}
    t = time.time()
    os.stat(HOT_FILE)
    steps["stat"] = time.time() - t
    t = time.time()
    f = open(HOT_FILE, "rb")
    steps["open"] = time.time() - t
    t = time.time()
    f.read(64 * 1024)
    steps["read"] = time.time() - t
    t = time.time()
    f.close()
    steps["close"] = time.time() - t
    t = time.time()
    os.listdir(HOT_DIR)
    steps["ls"] = time.time() - t
    last_steps.clear()
    last_steps.update({k: round(v * 1000, 1) for k, v in steps.items()})


def probe_loop():
    while not done():
        t = time.time()
        err = timed("probe", probe_once)
        ms = (time.time() - t) * 1000
        with lock:
            probes.append((round(t - T0, 2), round(ms, 2), err, dict(last_steps) if ms > 200 else None))
        time.sleep(max(0.0, 0.1 - (time.time() - t)))


def slow_loop():
    while not done():
        t = time.time()
        err = timed("slow", lambda: os.stat(SLOW_STAT))
        with lock:
            slow.append((round(t - T0, 2), round(time.time() - t, 2), err or "ok"))
        time.sleep(0.2)


def pct(xs, q):
    return xs[min(len(xs) - 1, int(len(xs) * q))] if xs else None


def main():
    # Warm the hot directory and pin its listing with an open handle.
    os.listdir(HOT_DIR)
    held = os.open(HOT_DIR, os.O_RDONLY | os.O_DIRECTORY)
    for fn in (probe_loop, slow_loop):
        threading.Thread(target=fn, daemon=True).start()
    while not done():
        time.sleep(0.5)
    time.sleep(0.5)
    now = time.time()
    with lock:
        # A call still blocked at the end counts at its age so far.
        stuck = {k: round(now - v, 2) for k, v in inflight.items()}
        lat = sorted(p[1] for p in probes)
        if "probe" in stuck:
            lat.append(stuck["probe"] * 1000)
            lat.sort()
        probe_errors = [p[2] for p in probes if p[2]]
        outcomes = {}
        for _, _, o in slow:
            outcomes[o] = outcomes.get(o, 0) + 1
        slow_max = max([s for _, s, _ in slow] + ([stuck["slow"]] if "slow" in stuck else []), default=None)
        worst = sorted(probes, key=lambda p: -p[1])[:10]
    os.close(held)
    p99_max = float(os.environ.get("PROBE_P99_MS", "200"))
    max_max = float(os.environ.get("PROBE_MAX_MS", "1000"))
    slow_bound = float(os.environ.get("SLOW_STAT_MAX_S", "17"))
    out = {
        "probes": len(lat),
        "probe_p50_ms": pct(lat, 0.50),
        "probe_p99_ms": pct(lat, 0.99),
        "probe_max_ms": lat[-1] if lat else None,
        "probe_errors": len(probe_errors),
        "probe_error_kinds": sorted(set(probe_errors)),
        "worst_probes": worst,
        "slow_stats": len(slow),
        "slow_stat_max_s": slow_max,
        "slow_stat_outcomes": outcomes,
        "still_blocked_at_end_s": stuck,
        "criteria": [
            ["hot-file probe p99 < %d ms" % p99_max, bool(lat) and pct(lat, 0.99) < p99_max, pct(lat, 0.99)],
            ["hot-file probe max < %d ms" % max_max, bool(lat) and lat[-1] < max_max, lat[-1] if lat else None],
            ["hot-file probes never failed", not probe_errors, sorted(set(probe_errors))],
            ["slow stat returns within %.0f s" % slow_bound, slow_max is not None and slow_max <= slow_bound, slow_max],
            ["slow stat of an existing file never ENOENT", "ENOENT" not in outcomes and bool(slow), outcomes],
        ],
    }
    with open(os.path.join(RES, "nofreeze.json"), "w") as f:
        json.dump(out, f, indent=1)
    print("[nofreeze] probes=%d p50=%sms p99=%sms max=%sms errors=%d | slow stats=%d max=%ss outcomes=%s stuck=%s" % (
        out["probes"], out["probe_p50_ms"], out["probe_p99_ms"], out["probe_max_ms"], out["probe_errors"],
        out["slow_stats"], slow_max, outcomes, stuck))
    # Blocked daemon threads must not keep the interpreter alive.
    os._exit(0)


if __name__ == "__main__":
    main()
