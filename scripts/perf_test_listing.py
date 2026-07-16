#!/usr/bin/env python3
"""Directory-listing performance test for the ncrs FUSE mount.

Measures the three layers a file manager exercises when it lists a directory,
so a regression in any one of them is isolated instead of hidden in a single
wall-clock number:

  1. readdir      -- plain `ls` (one PROPFIND, served from the dir cache)
  2. getattr      -- stat() on every entry
  3. mime-detect  -- open(O_NOATIME) + read(16 KiB) on every file

Layer 3 is the one that regressed historically: GLib 2.80 sniffs the content
type of files with unknown/ambiguous extensions by opening them with O_NOATIME
and reading the first 16 KiB. On a remote mount each such read is a WebDAV
download (~300 ms) unless ncrs intercepts it and answers with synthetic magic
bytes. The kernel inflates GLib's 16 KiB read up to a 32 KiB read-ahead window,
so the intercept guard must allow reads up to 32768 bytes (see
`MIME_DETECT_MAX_READ` in ncrs_core/src/lib.rs).

Usage:
    scripts/perf_test_listing.py [DIR] [--budget SECONDS]

DIR defaults to ~/Nextcloud/logoclc/logoHouse. Exit code is non-zero if the
mime-detect layer exceeds --budget (default 1.0 s), so this doubles as a
regression check in CI or a pre-release smoke test.
"""
import argparse
import os
import sys
import time

DEFAULT_DIR = os.path.expanduser("~/Nextcloud/logoclc/logoHouse")


def time_it(fn):
    t0 = time.time()
    fn()
    return time.time() - t0


def measure_readdir(d):
    return time_it(lambda: os.listdir(d))


def measure_getattr(d, names):
    def run():
        for n in names:
            try:
                os.lstat(os.path.join(d, n))
            except OSError:
                pass
    return time_it(run)


def measure_mime_detect(d, files):
    """open(O_NOATIME)+read(16K) per file -- the GLib magic-byte path."""
    per_file = []
    for n in files:
        p = os.path.join(d, n)
        s = time.time()
        try:
            fd = os.open(p, os.O_RDONLY | os.O_NOATIME)
            os.read(fd, 16384)
            os.close(fd)
        except OSError:
            pass
        per_file.append((time.time() - s, n))
    return per_file


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("directory", nargs="?", default=DEFAULT_DIR)
    ap.add_argument("--budget", type=float, default=1.0,
                    help="max seconds allowed for the mime-detect layer (default 1.0)")
    args = ap.parse_args()

    d = args.directory
    if not os.path.isdir(d):
        print(f"ERROR: {d} is not a directory (is the ncrs mount up?)", file=sys.stderr)
        return 2

    # Warm the dir cache so the readdir PROPFIND is not charged to layer 1.
    os.listdir(d)
    time.sleep(0.2)

    names = os.listdir(d)
    files = [n for n in names if os.path.isfile(os.path.join(d, n))]
    print(f"Directory : {d}")
    print(f"Entries   : {len(names)} ({len(files)} files)\n")

    t_readdir = measure_readdir(d)
    t_getattr = measure_getattr(d, names)
    per_file = measure_mime_detect(d, files)
    t_mime = sum(t for t, _ in per_file)

    print(f"1. readdir      : {t_readdir*1000:8.1f} ms")
    print(f"2. getattr(all) : {t_getattr*1000:8.1f} ms  ({len(names)} entries)")
    print(f"3. mime-detect  : {t_mime*1000:8.1f} ms  ({len(files)} files, "
          f"{t_mime/max(len(files),1)*1000:.1f} ms/file)")

    slow = sorted((x for x in per_file if x[0] > 0.02), reverse=True)
    if slow:
        print(f"\n   {len(slow)} file(s) >20 ms (likely downloaded instead of intercepted):")
        for dt, n in slow[:10]:
            print(f"     {dt*1000:7.1f} ms  {n}")

    ok = t_mime <= args.budget
    print(f"\nRESULT: mime-detect {t_mime:.3f}s "
          f"{'<=' if ok else '>'} budget {args.budget:.3f}s -> "
          f"{'PASS' if ok else 'FAIL'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
