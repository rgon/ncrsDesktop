# Daemon restart survival

Programs keep files open on the mount for a long time (LibreOffice holds its
document open read-write for the whole session). If the daemon restarts, a
fresh mount gives every such descriptor ENOTCONN. Restart survival keeps the
mount and those descriptors alive while one daemon hands over to the next.

Code: `ncrs_core/src/handoff.rs` (the connection), `ncrs_core/src/state_snapshot.rs`
(node ids and file handles), `ncrs_core/src/sd_watchdog.rs`, the fuser patch in
`PATCHES.md` ("fuser: reattach"), `packaging/ncrs.service`. Tested by e2e
scenario 32 against `docker/e2e/ncrs/fake_systemd.py`, and against the real
systemd by `scripts/e2e-systemd.sh` (see "Testing with the real systemd").

## How it works

The kernel keeps a FUSE connection alive while any process holds its
`/dev/fuse` descriptor. While no daemon reads it, requests queue in the kernel
and programs block; nothing fails.

- **Supervisor: systemd.** ncrs runs as the user unit `ncrs.service` (the GUI
  starts it with `systemctl --user start`). After mounting, the daemon puts a
  dup of the descriptor in systemd's fd store (`sd_notify FDSTORE=1`,
  `FileDescriptorStoreMax=4`). After a crash, an OOM kill, a watchdog kill or a
  `systemctl --user restart`, systemd starts the daemon again and passes the
  descriptor back in `LISTEN_FDS`. The new daemon checks it against
  `/proc/self/mountinfo` (same device number as the fuse mount on the mount
  point) and runs its session on it without a new FUSE_INIT. Passing a
  stored descriptor does not take it out of the store, so the new daemon
  does not store it again (systemd only spots a re-sent copy as a duplicate
  where `kcmp(2)` is allowed; elsewhere, e.g. under Docker's default seccomp
  profile, it would become a second entry).
- **SIGTERM** (what systemd sends on stop and restart): the daemon pauses the
  request scheduler, waits up to 3 s for running requests, writes the exact
  handle snapshot if they all finished, writes the log out and exits 0
  without unmounting. On a restart the next daemon takes the descriptor from
  the store; on a stop systemd drops the store and the mount ends.
- **Planned restart** (IPC `RESTART_HANDOFF`, e.g. after an upgrade): the
  daemon drains the same way and re-executes itself with the descriptor left
  open across exec. The PID stays the same, so systemd sees no restart. If the
  exec fails, the snapshot is removed and the scheduler resumes at once.
- **Node ids and handles.** The kernel still holds the old daemon's node ids
  (dentries) and file handles (open files). `state_snapshot` logs them to
  `<cache_dir>/handoff.log` and the successor replays it. Handle records go out
  before their reply. Node-id records are queued and written once a second; a
  reservation of node ids, written ahead in blocks of 4096, makes the
  successor number past anything the dead daemon could have handed out, so a
  lost record makes that id unknown (ENOENT), never someone else's file. A
  drained hand-over also writes `handoff-handles.json` (dirty flags, streamed
  upload progress); the successor reads it once and deletes it. Without it,
  write handles are judged by their staging file's identity, and a handle that
  was mid-way through a streamed upload comes back failed (EIO) rather than
  guessed.
- **Passthrough backing ids** belong to the connection, not to a process, so
  files the previous daemon opened in passthrough mode keep reading directly
  from their cache copies. Their ids are part of the handle log; the
  successor closes each one once its last restored handle is released. The
  ioctls need CAP_SYS_ADMIN, which only the `ncrs-passthrough` helper holds:
  the old helper exits with its daemon (its socket closes on exec, and it
  has `PR_SET_PDEATHSIG`), and the successor spawns a new one on the
  taken-over descriptor and lets it adopt those ids before its first open.
- **RESEND.** Requests the previous daemon read but never answered are
  re-queued with `FUSE_NOTIFY_RESEND` (Linux 6.9+), sent before the new session
  reads anything, so no request runs twice.
- **Watchdog** (`WatchdogSec=90`): a thread pings every 30 s while the global
  locks (`cache`, `open_files`, the journal) can each be taken within 5 s and
  the FUSE pool keeps completing work. A hung daemon is killed and restarted by
  systemd, and the mount waits for the new one. One slow transfer does not stop
  the pings.
- **Bounds.** `StartLimitBurst=5` in `StartLimitIntervalSec=120` fails a daemon
  that keeps dying. A failed or stopped unit drops the fd store
  (`FileDescriptorStorePreserve=restart`), and the mount then fails ENOTCONN.
  A daemon that takes the connection over but cannot start serving it (bad
  config, keyring, setup error) aborts the connection itself (fusectl
  `abort`), so programs get ENOTCONN at once. The next start then finds no
  descriptor in the store and the mount point ENOTCONN, and detaches it
  (`fusermount -uz`): held descriptors keep failing ENOTCONN, new lookups see
  the empty mount-point directory. When automatic restarts run into the start
  limit, systemd logs "Start request repeated too quickly" and the unit is
  `failed` with the last failure as its `Result` (`exit-code`), not
  `start-limit-hit`.

## Not covered

- **No systemd.** Without `NOTIFY_SOCKET` nothing else holds the descriptor:
  a crash gives ENOTCONN, as before this feature. Planned restarts still work.
  A small descriptor-holder process (one that keeps a dup and restarts the
  daemon) could cover this case. It is not implemented, and could be added
  later.
- **`systemctl --user stop ncrs.service`** (also logout) ends the mount: that
  is the deliberate stop.
- **Package upgrades** do not restart the daemon by themselves. A postinst
  hook that sends `RESTART_HANDOFF` (or `systemctl --user restart`) to running
  user daemons is future work.
- Kernels older than 6.9 lack `FUSE_NOTIFY_RESEND`: a request in flight at the
  moment of a crash then stays blocked until its program is interrupted.
- **Downgrading while a large file streams.** Streamed staging files keep the
  file's own offsets with sent chunks punched out. An older binary refuses to
  finish a queued streamed upload in that layout rather than assemble a wrong
  file: nothing is corrupted, but that copy has to be made again. A re-exec
  onto a binary that predates restart survival also starts it with SIGTERM
  blocked, so its `stop` waits for systemd's SIGKILL.
- **Staging filesystems without hole punching** (ecryptfs, vfat, some FUSE
  homes): a big file is staged in full even when the disk is low.

## Testing with the real systemd

`scripts/e2e-systemd.sh` boots systemd 255 (Ubuntu 24.04, as on the desktop)
as PID 1 of a container, installs `packaging/ncrs.service` unchanged as a user
unit for a lingering test user, and runs `docker/e2e/systemd/test.sh` as that
user: `systemctl --user start`, IPC `RESTART_HANDOFF`, `systemctl --user
restart`, `kill -9`, a watchdog kill of a `SIGSTOP`ped daemon with a read in
flight (drop-in `WatchdogSec=10`), a start limit hit by a daemon that fails
after taking the connection over, then `reset-failed` + start and `stop`. Each
step is checked against `systemctl --user show` (`MainPID`, `NRestarts`,
`ActiveState`, `Result`, `NFileDescriptorStore`) and `journalctl --user -u
ncrs`, while the test shell holds a writer and a reader open on the mount.

```sh
# the bookworm-built daemon packed as docker/e2e/ncrs/ncrs.deb (verify skill), then
scripts/e2e-systemd.sh          # ~1 min; KEEP=1 leaves the containers up
```

The container runs without `--privileged`: `--cap-add SYS_ADMIN --device
/dev/fuse --security-opt apparmor=unconfined --cgroupns=private --tmpfs /run
--tmpfs /run/lock --tmpfs /tmp`, and a two-line PID 1 wrapper remounts its
cgroup2 tree read-write before exec'ing `/sbin/init`. No host path is mounted.
The unit's daemon logs at `warn` by default; the test sets `RUST_LOG=info` in
the user manager's environment (`systemctl --user set-environment`) to see the
hand-off lines. It is not in CI yet. The e2e job already has what it needs to build and
start it (Docker with `SYS_ADMIN` + `/dev/fuse`, the prebuilt `.deb`), and it
would add about two minutes, but two things are unverified there: systemd
booting in the runner's Docker (cgroup2 remount), and the runner's kernel —
the watchdog step needs `FUSE_NOTIFY_RESEND` (Linux 6.9+); on an older kernel
the in-flight read stays blocked and that step fails.
