---
name: verify
description: Run the ncrs FUSE daemon end-to-end via the Docker e2e data-loss suite to confirm a change works against a live mount.
---

# Verifying ncrs changes end-to-end

The runtime surface is `scripts/e2e.sh`: it builds the `ncrs` CLI daemon, FUSE-mounts
a live rclone WebDAV backend inside Docker, and runs `docker/e2e/ncrs/scenarios.sh`
(14 data-loss scenarios, incl. server-outage offline-edit persistence). Exit 0 =
all scenarios passed. Needs `docker` + `/dev/fuse`.

## Fast path (what CI does) — PREBUILT_BINARY=1

Do NOT let the Dockerfile compile from source (default `PREBUILT_BINARY=0`): the
`rust:1-bookworm` image lacks **mold**, and `.cargo/config.toml` forces
`-C link-arg=-fuse-ld=mold`, so the in-container build dies with
`collect2: cannot find 'ld'`. CI instead injects a prebuilt `.deb`.

Two gotchas when building the binary yourself:
1. **mold** — the linker flag above must resolve. Host has it; a bookworm build
   container must `apt-get install -y mold`.
2. **GLIBC** — the runtime image is `debian:bookworm-slim` (glibc 2.36). A binary
   built on a newer host (Ubuntu, glibc 2.38/2.39) fails at daemon start with
   `version 'GLIBC_2.39' not found`. Build inside a **bookworm** container.

Recipe (incremental after the first ~7 min full build; ~30 s thereafter):

```bash
# 1. build a bookworm-compatible binary (separate target dir to avoid host glibc mixing)
docker run --rm -v "$PWD":/src -w /src -e CARGO_TARGET_DIR=/src/target-bookworm rust:1-bookworm \
  bash -c "apt-get update -qq && apt-get install -y -qq --no-install-recommends mold libfuse3-dev pkg-config libssl-dev && cargo build --release -p ncrs_core --bin ncrs"

# 2. pack a minimal .deb — the Dockerfile only extracts /usr/bin/ncrs from it
d=$(mktemp -d); mkdir -p "$d/pkg/usr/bin" "$d/pkg/DEBIAN"
cp target-bookworm/release/ncrs "$d/pkg/usr/bin/ncrs"
printf 'Package: ncrs\nVersion: 0.0.0-e2e\nArchitecture: amd64\nMaintainer: e2e <e2e@local>\nDescription: e2e\n' > "$d/pkg/DEBIAN/control"
dpkg-deb --build "$d/pkg" docker/e2e/ncrs/ncrs.deb

# 3. run the suite
PREBUILT_BINARY=1 scripts/e2e.sh
```

Re-running after editing only `scenarios.sh` needs just step 3 (the image rebuild
picks up the new script; the `.deb` is unchanged).

## Interactive probing (drive individual reads on a live mount)

To poke the mount by hand instead of the whole suite, bind-mount a script that
mounts + `sleep infinity` over `/e2e/scenarios.sh`, then `docker exec`:

```bash
docker compose -f docker/e2e/docker-compose.yml up -d webdav
docker run -d --name ncrsprobe --network e2e_default \
  --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor:unconfined \
  -e WEBDAV_HOST=webdav -e RUST_LOG=info \
  -v /path/to/probe.sh:/e2e/scenarios.sh:ro e2e-ncrs
# probe.sh: mount is at /mnt/ncrs; entrypoint mounts before running the script
docker exec ncrsprobe sh -c 'head -c16 /mnt/ncrs/somefile | od -An -tx1'
```

Useful reads when debugging the MIME-detect intercept / page-cache behavior:
`dd iflag=noatime` (triggers the intercept), `dd iflag=direct` (bypasses the page
cache — proves whether truncation is cache poisoning).

## Cleanup

`target-bookworm/` is written by root inside the container — remove it with
`docker run --rm -v "$PWD":/src rust:1-bookworm rm -rf /src/target-bookworm`.
Also `rm -f docker/e2e/ncrs/ncrs.deb` and `git checkout -- Cargo.lock` (a host
`cargo build` rewrites the workspace version in the lockfile).
