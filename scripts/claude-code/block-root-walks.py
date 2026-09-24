#!/usr/bin/env python3
"""Claude Code PreToolUse hook: refuse filesystem walks that would crawl a
network mount such as the ncrs ~/Nextcloud FUSE mount.

Coding agents like to run `find / -name foo` or `rg pattern ~`. On a machine
with a network filesystem mounted under $HOME, that recursive walk becomes one
WebDAV PROPFIND per directory against your Nextcloud server, for as long as the
command runs (Claude Code backgrounds long commands rather than killing them).

Blocks:
  * Bash: find/du/rg/grep -r/fd/ls -R/tree/ncdu/rsync/tar/chmod -R/chown -R/
    cp -r/zip -r whose starting point is /, ~, $HOME, or a directory above the
    mount, unless the command stays on one filesystem (-xdev, -mount,
    --one-file-system, -x) or explicitly prunes/excludes the mount.
  * Glob/Grep tools whose search path is /, ~, or $HOME (or a parent of it).

Install: copy to ~/.claude/hooks/ and register it for "Bash|Glob|Grep" under
hooks.PreToolUse in ~/.claude/settings.json (see README "Note for coding agent
users"). Set NCRS_WALK_GUARD_MOUNTS to a colon-separated list to guard more
mounts; the default is ~/Nextcloud.
"""
import json
import os
import re
import shlex
import sys

HOME = os.path.expanduser("~")
MOUNTS = [
    os.path.realpath(os.path.expanduser(m))
    for m in os.environ.get("NCRS_WALK_GUARD_MOUNTS", "~/Nextcloud").split(":")
    if m
]

# Walkers and the flags that make them recursive (None = always recursive).
WALKERS = {
    "find": None,
    "du": None,
    "rg": None,
    "fd": None,
    "fdfind": None,
    "tree": None,
    "ncdu": None,
    "locate": None,
    "grep": {"-r", "-R", "--recursive", "--dereference-recursive"},
    "egrep": {"-r", "-R"},
    "ls": {"-R", "--recursive"},
    "chmod": {"-R", "--recursive"},
    "chown": {"-R", "--recursive"},
    "cp": {"-r", "-R", "-a", "--recursive", "--archive"},
    "rsync": {"-r", "-a", "--recursive", "--archive"},
    "tar": None,
    "zip": {"-r"},
}
ONE_FS_FLAGS = {"-xdev", "-mount", "--one-file-system"}
# `-x` means "one filesystem" only for these (for grep it means --line-regexp).
SHORT_X_ONE_FS = {"du", "tree", "ncdu", "cp", "rsync"}
REASON = (
    "Blocked: this command would recursively walk {start}, which contains the "
    "network FUSE mount {mount} (every directory becomes a request to the "
    "Nextcloud server, and backgrounded walks run for hours). Stay on one "
    "filesystem instead: `find {start} -xdev ...`, `rg --one-file-system ...`, "
    "`du -x ...`, or search a narrower directory (e.g. the project, /usr, "
    "~/.cargo). If you really need the mount, name a specific subdirectory of it."
)


def resolve(path: str, cwd: str) -> str:
    p = os.path.expanduser(os.path.expandvars(path))
    first_glob = min((p.index(ch) for ch in "*?[" if ch in p), default=None)
    if first_glob is not None:
        # A glob walks every match inside the directory holding its first
        # wildcard segment: `~/*` → ~, `/home/rg*` → /home, `*.rs` → cwd.
        before = p[:first_glob]
        p = (before[: before.rfind("/")] or "/") if "/" in before else "."
    return os.path.realpath(os.path.join(cwd, p))


def covers_mount(path: str, cwd: str) -> str | None:
    """Return the guarded mount that a walk starting at `path` would enter."""
    p = resolve(path, cwd)
    for m in MOUNTS:
        if m == p:
            return None  # an explicit walk of the mount root is a deliberate choice
        if m.startswith(p.rstrip("/") + "/"):
            return m
    return None


# find (and friends) use single-dash long options (-maxdepth, -xdev), so their
# flags must never be split into letters.
LONG_SINGLE_DASH = {"find"}


def flag_set(prog: str, args: list[str]) -> set[str]:
    out = set()
    for a in args:
        if a == "--":
            break
        if a.startswith("--"):
            out.add(a.split("=", 1)[0])
        elif a.startswith("-") and len(a) > 1:
            out.add(a)
            if prog not in LONG_SINGLE_DASH and a[1:].isalpha():
                out.update(f"-{c}" for c in a[1:])  # bundled short flags: -rn → -r -n
    return out


def mentions_mount_exclusion(cmd: str) -> bool:
    return any(
        os.path.basename(m) in cmd
        and re.search(r"-prune|--exclude|-not\s+-path|!\s+-path|--glob\s*[\"']?!|-g\s*[\"']?!", cmd)
        for m in MOUNTS
    )


WRAPPERS = ("sudo", "nice", "ionice", "timeout", "time", "env", "command", "exec", "xargs", "nohup", "stdbuf")


def check_segment(tokens: list[str], full_cmd: str, cwd: str) -> tuple[str, str] | None:
    # Skip wrappers so `sudo find /`, `timeout 60 find /`, `nice find /` are seen.
    while tokens and os.path.basename(tokens[0]) in WRAPPERS:
        tokens = tokens[1:]
        while tokens and (tokens[0].startswith("-") or re.fullmatch(r"[\d.]+[smhd]?|\w+=\S*", tokens[0])):
            tokens = tokens[1:]
    if not tokens:
        return None
    prog = os.path.basename(tokens[0])
    if prog not in WALKERS:
        return None
    args = tokens[1:]
    flags = flag_set(prog, args)
    recursive_flags = WALKERS[prog]
    if recursive_flags is not None and not (flags & recursive_flags):
        return None
    if flags & ONE_FS_FLAGS or (prog in SHORT_X_ONE_FS and "-x" in flags):
        return None
    if mentions_mount_exclusion(full_cmd):
        return None
    candidates = [a for a in args if not a.startswith("-")]
    if prog == "find":
        # find's paths come before the first expression token.
        candidates = []
        for a in args:
            if a.startswith(("-", "(", "!")):
                break
            candidates.append(a)
        candidates = candidates or ["."]
    elif prog in ("rg", "grep", "egrep", "fd", "fdfind"):
        # the first positional is the pattern; with no path they search the cwd.
        candidates = candidates[1:] or ["."]
    elif prog in ("du", "tree", "ncdu", "ls") and not candidates:
        candidates = ["."]
    max_depth = None
    for flag in ("-maxdepth", "-d", "--max-depth", "-L", "--level"):
        if flag in args[:-1]:
            try:
                max_depth = int(args[args.index(flag) + 1])
            except ValueError:
                pass
    for c in candidates:
        m = covers_mount(c, cwd)
        if not m:
            continue
        start = resolve(c, cwd)
        # Depth at which the walk would list the mount's *contents*.
        depth_inside = len(os.path.relpath(m, start).split(os.sep)) + 1
        if max_depth is not None and max_depth < depth_inside:
            continue
        return start, m
    return None


def split_segments(cmd: str) -> list[list[str]]:
    segs, cur = [], []
    try:
        lex = shlex.shlex(cmd, posix=True, punctuation_chars=";&|()")
        lex.whitespace_split = True
        for tok in lex:
            if tok and set(tok) <= set(";&|()"):
                if cur:
                    segs.append(cur)
                cur = []
            else:
                cur.append(tok)
    except ValueError:
        return [cmd.split()]
    if cur:
        segs.append(cur)
    return segs


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except Exception:
        return 0
    tool = payload.get("tool_name", "")
    ti = payload.get("tool_input") or {}
    hit = None
    if tool == "Bash":
        cmd = ti.get("command", "")
        cwd = payload.get("cwd") or os.getcwd()
        for seg in split_segments(cmd):
            if seg and seg[0] in ("cd", "pushd"):
                cwd = resolve(seg[1] if len(seg) > 1 else "~", cwd)
                continue
            hit = check_segment(seg, cmd, cwd)
            if hit:
                break
    elif tool in ("Glob", "Grep"):
        cwd = payload.get("cwd") or os.getcwd()
        path = ti.get("path") or cwd
        m = covers_mount(path, cwd)
        if m:
            hit = (path, m)
    if not hit:
        return 0
    start, mount = hit
    print(json.dumps({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": REASON.format(start=start, mount=mount),
        }
    }))
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception:
        sys.exit(0)  # fail open: a guard bug must never block unrelated commands
