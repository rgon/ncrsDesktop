#!/usr/bin/env bash
# Populate the rclone data volume with a large directory tree, written directly
# to the volume (not over WebDAV) so seeding 20k dirs takes seconds.
#
# Runs inside a throwaway container with the volume mounted at $SEED_ROOT
# (default /data). Env:
#   DIRS            total directories to create (default 20000; e.g. 2000 = quick)
#   MAX_DEPTH       deepest nesting level (default 18)
#   FILES_PER_DIR   files per directory (default 3; the first has a few bytes,
#                   the rest are empty so the volume stays small on disk)
#   SEED            RNG seed (default 1) — same inputs give the same tree
#   SEED_ROOT       where to write (default /data)
set -euo pipefail

export DIRS="${DIRS:-20000}" MAX_DEPTH="${MAX_DEPTH:-18}" \
       FILES_PER_DIR="${FILES_PER_DIR:-3}" SEED="${SEED:-1}" \
       SEED_ROOT="${SEED_ROOT:-/data}"

if [ -f "$SEED_ROOT/.walker-seed" ] && [ "$(cat "$SEED_ROOT/.walker-seed")" = "$DIRS/$MAX_DEPTH/$FILES_PER_DIR/$SEED" ]; then
    echo "[seed] volume already seeded ($(cat "$SEED_ROOT/.walker-seed")), skipping"
    exit 0
fi
rm -rf "${SEED_ROOT:?}"/* "${SEED_ROOT:?}"/.[!.]* 2>/dev/null || true

python3 - <<'PY'
import os, random, sys, time

root = os.environ["SEED_ROOT"]
target = int(os.environ["DIRS"])
max_depth = int(os.environ["MAX_DEPTH"])
fpd = int(os.environ["FILES_PER_DIR"])
rng = random.Random(int(os.environ["SEED"]))
made = 0
t0 = time.time()

def mk(rel):
    """Create one directory (and its files); returns False once the budget is spent."""
    global made
    if made >= target:
        return False
    p = os.path.join(root, rel)
    os.makedirs(p, exist_ok=True)
    made += 1
    for i in range(fpd):
        with open(os.path.join(p, "file%d.txt" % i), "wb") as f:
            if i == 0:
                f.write(rel.encode() + b"\n")
    return True

def depth(rel):
    return rel.count("/") + 1

# 1. Awkward names: HTTP-status-looking and error-looking names, spaces, unicode.
special = ["err 401", "403-forbidden", "build2404", "timeout", "Not Found",
           "500 Internal", "with  two  spaces", "Fotos año 2024", "日本語 フォルダ",
           "emoji 📁 dir", "trailing.dot.", "#hash & amp", "percent %20 literal"]
for s in special:
    mk(s)
    for sub in ("401", "timeout", "Not Found", "sub dir"):
        mk(f"{s}/{sub}")

# 2. A deep chain down to max_depth.
chain = "deep"
mk(chain)
for d in range(2, max_depth + 1):
    chain += f"/d{d:02d}"
    mk(chain)

# 3. .git-like trees (objects/00..ff fan-out + refs + hooks).
for repo in ("projects/app", "projects/lib"):
    for part in ("", "/.git", "/.git/refs", "/.git/refs/heads", "/.git/refs/tags",
                 "/.git/hooks", "/.git/info", "/.git/logs", "/.git/logs/refs",
                 "/.git/objects", "/.git/objects/pack", "/.git/objects/info", "/src", "/docs"):
        mk(repo + part)
    for i in range(256):
        if not mk(f"{repo}/.git/objects/{i:02x}"):
            break

# 4. node_modules-like wide dirs: 500 packages, some with lib/dist children.
budget_nm = max(0, min(target - made, target // 4))
nm_root = "projects/app/node_modules"
mk(nm_root)
start = made
for i in range(500):
    if made - start >= budget_nm:
        break
    pkg = f"{nm_root}/pkg-{i:03d}"
    mk(pkg)
    if i % 5 == 0:
        mk(pkg + "/lib")
        mk(pkg + "/dist")

# 5. Fill the rest with a random tree (BFS, depth-capped).
frontier = ["tree"]
mk("tree")
n = 0
while made < target and frontier:
    parent = frontier.pop(rng.randrange(len(frontier))) if rng.random() < 0.3 else frontier.pop(0)
    kids = rng.randint(2, 9)
    for _ in range(kids):
        n += 1
        name = f"{parent}/n{n}"
        if not mk(name):
            break
        if depth(name) < max_depth:
            frontier.append(name)
    if not frontier and made < target:
        frontier.append("tree")  # widen the top level again

# 6. Outside the DIRS budget: what the "no-freeze" scenario probes (nofreeze.py).
#    probe/hot holds a file read every 100 ms; probe/slowdir is the directory
#    the proxy stalls (SLOW_PATH_SUBSTR=slowdir), holding a file that exists.
for d in ("probe/hot", "probe/slowdir"):
    os.makedirs(os.path.join(root, d), exist_ok=True)
with open(os.path.join(root, "probe/hot/hot.bin"), "wb") as f:
    f.write(rng.randbytes(256 * 1024))
for i in range(20):
    with open(os.path.join(root, "probe/hot/f%02d.txt" % i), "wb") as f:
        f.write(b"hot %d\n" % i)
with open(os.path.join(root, "probe/slowdir/present.txt"), "wb") as f:
    f.write(b"present\n")

with open(os.path.join(root, ".walker-seed"), "w") as f:
    f.write("%s/%s/%s/%s" % (os.environ["DIRS"], os.environ["MAX_DEPTH"], os.environ["FILES_PER_DIR"], os.environ["SEED"]))
print(f"[seed] created {made} dirs ({made * fpd} files) in {time.time() - t0:.1f}s under {root}")
PY
# rclone serves as root; make sure it can read everything.
chmod -R a+rX "$SEED_ROOT"
du -sh "$SEED_ROOT" 2>/dev/null | sed 's/^/[seed] volume size: /'
