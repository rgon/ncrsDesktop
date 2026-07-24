#!/usr/bin/env python3
"""Verify (and audit) the ncrs MIME-detect intercept against real GLib.

When a file's extension is unknown to the freedesktop MIME database, GLib
sniffs its content. On the ncrs mount that sniff is answered not from the file
but from synthetic magic bytes derived from the server's `getcontenttype`
(see `mime_magic_bytes` in ncrs_core/src/lib.rs). If those bytes don't make
GLib arrive at the right category, the file is mis-identified — e.g. Samsung
`.srw` raws showed up as text/plain because the fallback returned `# text\n`.

This script closes that gap by testing the invariant directly: for each
content-type, it asks the real `ncrs` binary for the synthetic bytes it would
serve, feeds them through the *same* GLib call Nautilus uses
(`Gio.content_type_guess`), and checks the verdict lands in the right category
and is never text for a non-text type. The bytes come from the compiled Rust —
there is no second copy of the mapping to drift out of sync.

Two modes:

  verify (default)
      Run a fixed fixture list of content-types (every exact arm plus a
      category-fallback probe per top level). Exit non-zero on any violation,
      so this doubles as a regression guard in CI or a pre-release check.

  audit --dir-cache PATH   (or --from-live to auto-find the live cache)
      Walk a daemon dir_cache.json, collect every distinct content-type that
      actually occurs, and report the verdict + whether the file's extension is
      known to freedesktop (i.e. whether the intercept even fires). The real
      risk set is: verdict is wrong/text AND some extension is unknown.

Usage:
    scripts/mime_audit.py                         # verify invariant (CI gate)
    scripts/mime_audit.py --ncrs target/debug/ncrs
    scripts/mime_audit.py audit --from-live
    scripts/mime_audit.py audit --dir-cache ~/.cache/ncrs/<host>/dir_cache.json
"""
import argparse
import glob
import json
import os
import subprocess
import sys

try:
    import gi
    gi.require_version("Gio", "2.0")
    from gi.repository import Gio
except Exception as e:  # noqa: BLE001
    sys.exit(f"mime_audit: PyGObject/GLib required (python3-gi): {e}")


# Every exact arm in mime_magic_bytes(), plus a category-fallback probe per top
# level. Keep in step with lib.rs — a new arm should get a line here so verify
# mode guards it against real GLib.
FIXTURE = [
    "application/pdf",
    "image/jpeg", "image/png", "image/gif", "image/bmp", "image/tiff",
    "image/webp", "image/x-dcraw",               # camera raw (.srw/.cr2/…)
    "image/heic", "image/heif", "image/svg+xml", "image/x-icon",
    "video/mp4", "video/quicktime", "video/ogg",
    "audio/mp4", "audio/mpeg", "audio/flac", "audio/wav", "audio/ogg",
    "application/ogg",
    "application/postscript", "application/vnd.debian.binary-package",
    "application/zip", "application/gzip", "application/x-bzip2",
    "application/x-7z-compressed", "application/x-rar-compressed",
    "application/msword", "application/vnd.ms-excel",
    # Category-fallback probes: types with no exact arm must still land in-category.
    "image/x-unknown-format", "video/x-unknown-format",
    "audio/x-unknown-format", "application/x-unknown-binary",
    "application/octet-stream",
]

# application/* subtypes we deliberately keep classifiable as text (editable),
# so a text-based format with an unknown extension isn't shown as opaque binary.
TEXT_ALLOWED = {
    "application/json", "application/xml", "application/javascript",
    "application/yaml", "application/toml", "application/x-tex",
}

# (content-type, verdict) pairs that are acceptable despite a top-level shift.
# Ogg is a container: audio/video .ogg legitimately sniff to application/ogg,
# and those extensions are freedesktop-known so the intercept never fires anyway.
ACCEPT_EXCEPTIONS = {
    ("audio/ogg", "application/ogg"),
    ("video/ogg", "application/ogg"),
}


def find_ncrs(explicit):
    if explicit:
        return explicit
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    for cand in ("target/debug/ncrs", "target/release/ncrs", "/usr/bin/ncrs"):
        p = cand if os.path.isabs(cand) else os.path.join(root, cand)
        if os.path.exists(p):
            return p
    sys.exit("mime_audit: no ncrs binary found (build it or pass --ncrs PATH)")


def synth_bytes(ncrs, content_types):
    """Ask the real ncrs binary for the synthetic bytes of each content-type."""
    stdin = "\n".join(content_types) + "\n"
    out = subprocess.run(
        [ncrs, "--dump-mime-magic"], input=stdin,
        capture_output=True, text=True, check=True,
    ).stdout
    result = {}
    for line in out.splitlines():
        if "\t" not in line:
            continue
        ct, hexstr = line.split("\t", 1)
        result[ct] = bytes.fromhex(hexstr)
    return result


def guess(b):
    ct, _uncertain = Gio.content_type_guess(None, b)
    return ct


def expected_ok(ct, verdict):
    """Is GLib's verdict acceptable for this content-type?"""
    if (ct, verdict) in ACCEPT_EXCEPTIONS:
        return True
    top = ct.split("/", 1)[0]
    vtop = verdict.split("/", 1)[0]
    if top == "text" or ct in TEXT_ALLOWED:
        return vtop == "text"
    if top in ("image", "video", "audio"):
        # Media must stay in-category and must never degrade to text/binary.
        return vtop == top
    # Other application/* (and anything else): a binary verdict is fine, text is not.
    return vtop != "text"


def cmd_verify(args):
    ncrs = find_ncrs(args.ncrs)
    cts = FIXTURE + sorted(TEXT_ALLOWED)
    b = synth_bytes(ncrs, cts)
    failures = []
    for ct in cts:
        verdict = guess(b[ct])
        ok = expected_ok(ct, verdict)
        flag = "ok " if ok else "FAIL"
        print(f"  [{flag}] {ct:45s} -> {verdict}")
        if not ok:
            failures.append((ct, verdict))
    print()
    if failures:
        print(f"FAIL: {len(failures)} content-type(s) mis-classified:")
        for ct, v in failures:
            print(f"  {ct} -> {v}")
        return 1
    print(f"OK: {len(cts)} content-types all classify into the intended category "
          f"(no binary shown as text).")
    return 0


def freedesktop_knows_ext(ext):
    """True if freedesktop resolves this extension by glob, so GLib gets a
    confident answer from the name alone and never sniffs the content (meaning
    the intercept's synthetic bytes are irrelevant for that extension).

    Uses GLib's filename-only guess — the same glob logic Nautilus uses. An
    unknown extension falls back to application/octet-stream; a .bin-style glob
    that maps to octet-stream is still sniffable, so both count as "unknown".
    """
    if not ext:
        return False
    verdict, _ = Gio.content_type_guess(f"probe.{ext}", None)
    return verdict != "application/octet-stream"


def cmd_audit(args):
    path = args.dir_cache
    if args.from_live and not path:
        cands = glob.glob(os.path.expanduser(
            "~/.cache/ncrs/*/dir_cache.json"))
        if not cands:
            sys.exit("mime_audit: no live dir_cache.json under ~/.cache/ncrs")
        path = max(cands, key=os.path.getmtime)
        print(f"# live cache: {path}\n")
    if not path:
        sys.exit("mime_audit: audit needs --dir-cache PATH or --from-live")

    d = json.load(open(path))
    # content-type -> {count, extensions}
    types = {}
    for _dir, v in d.items():
        for f in v.get("files", []):
            ct = (f.get("content_type") or "").split(";")[0].strip()
            if not ct:
                continue
            ext = os.path.splitext(f["path"])[1].lstrip(".").lower()
            e = types.setdefault(ct, {"count": 0, "exts": set()})
            e["count"] += 1
            if ext:
                e["exts"].add(ext)

    ncrs = find_ncrs(args.ncrs)
    b = synth_bytes(ncrs, list(types))
    ext_known = {}  # memoized

    rows, risk_files = [], 0
    for ct, info in sorted(types.items(), key=lambda kv: -kv[1]["count"]):
        verdict = guess(b[ct])
        ok = expected_ok(ct, verdict)
        # Would the intercept ever fire? Only for extensions freedesktop lacks.
        exts = info["exts"] or {""}
        unknown_exts = []
        for ext in exts:
            if ext not in ext_known:
                ext_known[ext] = freedesktop_knows_ext(ext)
            if not ext_known[ext]:
                unknown_exts.append(ext)
        at_risk = (not ok) and bool(unknown_exts)
        if at_risk:
            risk_files += info["count"]
        rows.append((info["count"], ct, verdict, ok, unknown_exts, at_risk))

    print(f"{'count':>7}  {'content-type':45s} {'GLib verdict':28s} cat  risk-exts")
    print("-" * 100)
    for count, ct, verdict, ok, unknown_exts, at_risk in rows:
        marker = "RISK" if at_risk else ("ok " if ok else "gen")
        exts = ",".join(sorted(e for e in unknown_exts if e)) if at_risk else ""
        print(f"{count:>7}  {ct:45s} {verdict:28s} {marker:4s} {exts}")
    print("-" * 100)
    print(f"{len(types)} distinct content-types; "
          f"{risk_files} file(s) could still be mis-identified "
          f"(wrong category AND extension unknown to freedesktop).")
    return 1 if risk_files else 0


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--ncrs", help="path to the ncrs binary (default: auto-detect)")
    sub = ap.add_subparsers(dest="cmd")
    sub.add_parser("verify", help="test the classification invariant (default)")
    a = sub.add_parser("audit", help="audit a live/dumped dir_cache.json")
    a.add_argument("--dir-cache", help="path to a daemon dir_cache.json")
    a.add_argument("--from-live", action="store_true",
                   help="auto-find the most recent ~/.cache/ncrs/*/dir_cache.json")
    args = ap.parse_args()

    if args.cmd == "audit":
        sys.exit(cmd_audit(args))
    sys.exit(cmd_verify(args))


if __name__ == "__main__":
    main()
