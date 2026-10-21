# Daemon restart survival

Programs keep files open on the mount for a long time (LibreOffice holds its
document open read-write for the whole session). If the daemon restarts, a
fresh mount gives every such descriptor ENOTCONN. Restart survival keeps the
mount and those descriptors alive while one daemon hands over to the next.

Code: `ncrs_core/src/handoff.rs` (the connection), `ncrs_core/src/state_snapshot.rs`
(node ids and file handles), `ncrs_core/src/sd_watchdog.rs`, the fuser patch in
`PATCHES.md` ("fuser: reattach"), `packaging/ncrs.service`. Tested by e2e
scenario 32 against `docker/e2e/ncrs/fake_systemd.py`.

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
  point) and runs its session on it without a new FUSE_INIT.
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
  `abort`), so programs get ENOTCONN at once.

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
