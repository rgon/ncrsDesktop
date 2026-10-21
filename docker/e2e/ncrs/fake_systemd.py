#!/usr/bin/env python3
"""A stand-in for systemd supervising one service, for e2e scenario 32.

The e2e container has no systemd, but restart survival depends on what systemd
does for packaging/ncrs.service. This supervises ONE process the same way:

- a datagram notify socket exported as NOTIFY_SOCKET (NotifyAccess=main: only
  the main PID's messages count, checked with SCM_CREDENTIALS); handles
  READY=1, STOPPING=1, WATCHDOG=1, FDSTORE=1 + FDNAME= (descriptors attached
  with SCM_RIGHTS, at most FileDescriptorStoreMax=4) and FDSTOREREMOVE=1;
- WATCHDOG_USEC / WATCHDOG_PID for the child; when the pings stop for that
  long the child gets SIGABRT, then SIGKILL, and is restarted;
- on every start the stored descriptors are passed as fd 3.. with LISTEN_FDS,
  LISTEN_FDNAMES and LISTEN_PID (the child's own pid, set after fork);
- Restart=on-failure after RestartSec: a non-zero exit, an unclean signal or
  a watchdog kill restarts; exit 0 (or SIGTERM/SIGINT/SIGHUP/SIGPIPE) means
  stopped, and the fd store is dropped (FileDescriptorStorePreserve=restart);
- StartLimitBurst= starts within StartLimitIntervalSec= make the unit fail,
  which drops the fd store too;
- a control socket: `restart` (SIGTERM, wait, start again, fd store kept),
  `stop` (SIGTERM, wait, drop the fd store, exit) and `status`.

Every event is logged, one line each, to the --log file.

Usage:
  fake_systemd.py run --dir DIR --log FILE [--watchdog-sec S] [--restart-sec S]
                      [--burst N] [--interval S] -- CMD [ARGS...]
  fake_systemd.py ctl DIR restart|stop|status
"""
import argparse
import array
import fcntl
import os
import select
import signal
import socket
import struct
import sys
import time

CLEAN_SIGNALS = {signal.SIGTERM, signal.SIGINT, signal.SIGHUP, signal.SIGPIPE}
FD_STORE_MAX = 4
SCM_CREDENTIALS = 2


class Supervisor:
    def __init__(self, a):
        self.a = a
        self.logf = open(a.log, "a", buffering=1)
        os.makedirs(a.dir, exist_ok=True)
        self.notify_path = os.path.join(a.dir, "notify")
        self.ctl_path = os.path.join(a.dir, "control")
        for p in (self.notify_path, self.ctl_path):
            try:
                os.unlink(p)
            except FileNotFoundError:
                pass
        self.notify = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        self.notify.bind(self.notify_path)
        self.notify.setsockopt(socket.SOL_SOCKET, socket.SO_PASSCRED, 1)
        self.ctl = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.ctl.bind(self.ctl_path)
        self.ctl.listen(4)
        self.store = {}  # FDNAME -> fd
        self.pid = None
        self.last_ping = 0.0
        self.starts = []
        self.state = "inactive"

    def log(self, msg):
        self.logf.write("%.3f %s\n" % (time.time(), msg))

    # ── the fd store ──────────────────────────────────────────────────────────
    def drop_store(self, why):
        for name, fd in self.store.items():
            os.close(fd)
            self.log("FDSTORE dropped %s (%s)" % (name, why))
        self.store = {}

    # ── starting ──────────────────────────────────────────────────────────────
    def start(self):
        now = time.time()
        self.starts = [t for t in self.starts if now - t < self.a.interval]
        if len(self.starts) >= self.a.burst:
            self.log("start limit hit (%d starts in %ds) — unit failed" % (len(self.starts), self.a.interval))
            self.state = "failed"
            self.drop_store("unit failed")
            return False
        self.starts.append(now)
        names = list(self.store.keys())
        fds = [self.store[n] for n in names]
        env = dict(os.environ)
        env["NOTIFY_SOCKET"] = self.notify_path
        for k in ("LISTEN_FDS", "LISTEN_FDNAMES", "LISTEN_PID", "WATCHDOG_USEC", "WATCHDOG_PID"):
            env.pop(k, None)
        if self.a.watchdog_sec > 0:
            env["WATCHDOG_USEC"] = str(int(self.a.watchdog_sec * 1_000_000))
        if fds:
            env["LISTEN_FDS"] = str(len(fds))
            env["LISTEN_FDNAMES"] = ":".join(names)
        pid = os.fork()
        if pid == 0:
            try:
                # Out of the way first, so 3.. can be filled without clobbering a source.
                high = [fcntl.fcntl(fd, fcntl.F_DUPFD, 100) for fd in fds]
                for i, fd in enumerate(high):
                    os.dup2(fd, 3 + i, inheritable=True)
                    os.close(fd)
                me = str(os.getpid())
                if fds:
                    env["LISTEN_PID"] = me
                if self.a.watchdog_sec > 0:
                    env["WATCHDOG_PID"] = me
                os.execvpe(self.a.cmd[0], self.a.cmd, env)
            except BaseException as e:  # noqa: BLE001 - child must never return
                sys.stderr.write("fake_systemd: exec failed: %s\n" % e)
            os._exit(127)
        self.pid = pid
        self.last_ping = time.time()  # the watchdog runs from the start
        self.state = "active"
        self.log("started pid %d (LISTEN_FDS=%d %s)" % (pid, len(fds), ":".join(names)))
        return True

    # ── stopping ──────────────────────────────────────────────────────────────
    def wait_exit(self, timeout):
        """Wait for the main process; returns its wait status or None."""
        end = time.time() + timeout
        while time.time() < end:
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                return status
            self.drain_notify(0.05)
        return None

    def kill_and_reap(self, first, timeout):
        os.kill(self.pid, first)
        status = self.wait_exit(timeout)
        if status is None:
            self.log("pid %d did not exit after %s — SIGKILL" % (self.pid, signal.Signals(first).name))
            os.kill(self.pid, signal.SIGKILL)
            _, status = os.waitpid(self.pid, 0)
        return status

    def describe(self, status):
        if os.WIFEXITED(status):
            return "exit %d" % os.WEXITSTATUS(status), os.WEXITSTATUS(status) == 0
        sig = os.WTERMSIG(status)
        return "signal %s" % signal.Signals(sig).name, sig in CLEAN_SIGNALS

    def after_exit(self, status, forced_failure=False):
        what, clean = self.describe(status)
        self.log("pid %d ended: %s" % (self.pid, what))
        self.pid = None
        if clean and not forced_failure:
            self.state = "inactive"
            self.log("service stopped (clean exit, Restart=on-failure does not restart)")
            self.drop_store("service stopped")
            return
        self.state = "activating"
        self.log("service failed — restarting in %ss" % self.a.restart_sec)
        self.sleep_serving(self.a.restart_sec)
        self.start()

    def sleep_serving(self, secs):
        end = time.time() + secs
        while time.time() < end:
            self.drain_notify(min(0.1, max(0.0, end - time.time())))

    # ── notify socket ─────────────────────────────────────────────────────────
    def drain_notify(self, timeout):
        r, _, _ = select.select([self.notify], [], [], timeout)
        if r:
            self.on_notify()

    def on_notify(self):
        fds = array.array("i")
        msg, anc, _, _ = self.notify.recvmsg(4096, socket.CMSG_SPACE(FD_STORE_MAX * 4 * 4) + socket.CMSG_SPACE(12))
        sender = None
        for level, kind, data in anc:
            if level == socket.SOL_SOCKET and kind == socket.SCM_RIGHTS:
                fds.frombytes(data[: len(data) - (len(data) % fds.itemsize)])
            elif level == socket.SOL_SOCKET and kind == SCM_CREDENTIALS:
                sender = struct.unpack("iII", data[:12])[0]
        fds = list(fds)
        for fd in fds:
            os.set_inheritable(fd, False)  # only passed on as LISTEN_FDS, never leaked
        if sender is None or sender != self.pid:
            self.log("ignored notification from pid %s (main pid %s, NotifyAccess=main): %r" % (sender, self.pid, msg))
            for fd in fds:
                os.close(fd)
            return
        fields = dict(l.split("=", 1) for l in msg.decode(errors="replace").split("\n") if "=" in l)
        if fields.get("WATCHDOG") == "1":
            self.last_ping = time.time()
        if fields.get("READY") == "1":
            self.log("READY=1 from pid %d" % sender)
        if fields.get("STOPPING") == "1":
            self.log("STOPPING=1 from pid %d" % sender)
        name = fields.get("FDNAME", "stored")
        if fields.get("FDSTOREREMOVE") == "1":
            fd = self.store.pop(name, None)
            if fd is not None:
                os.close(fd)
            self.log("FDSTOREREMOVE %s (%s)" % (name, "removed" if fd is not None else "not stored"))
        if fields.get("FDSTORE") == "1":
            for fd in fds:
                if name in self.store:
                    os.close(self.store.pop(name))  # systemd keeps one copy per name
                if len(self.store) >= FD_STORE_MAX:
                    self.log("FDSTORE %s refused: store full" % name)
                    os.close(fd)
                    continue
                self.store[name] = fd
                self.log("FDSTORE %s kept" % name)
            fds = []
        for fd in fds:
            os.close(fd)

    # ── control socket ────────────────────────────────────────────────────────
    def on_control(self):
        conn, _ = self.ctl.accept()
        with conn:
            cmd = conn.recv(256).decode().strip()
            self.log("control: %s" % cmd)
            if cmd == "status":
                conn.sendall(("state=%s pid=%s store=%s\n" % (self.state, self.pid or "", ",".join(self.store))).encode())
            elif cmd == "restart":
                if self.pid:
                    status = self.kill_and_reap(signal.SIGTERM, 15)
                    self.log("pid %d ended for restart: %s (fd store kept)" % (self.pid, self.describe(status)[0]))
                    self.pid = None
                self.start()
                conn.sendall(("ok pid=%s\n" % (self.pid or "")).encode())
            elif cmd == "stop":
                if self.pid:
                    status = self.kill_and_reap(signal.SIGTERM, 15)
                    self.log("pid %d ended for stop: %s" % (self.pid, self.describe(status)[0]))
                    self.pid = None
                self.state = "inactive"
                self.drop_store("service stopped")
                conn.sendall(b"ok\n")
                return "exit"
            else:
                conn.sendall(b"unknown command\n")
        return None

    # ── main loop ─────────────────────────────────────────────────────────────
    def run(self):
        self.log("fake systemd up: watchdog %ss, RestartSec %ss, StartLimitBurst %d/%ds" % (
            self.a.watchdog_sec, self.a.restart_sec, self.a.burst, self.a.interval))
        self.start()
        while True:
            r, _, _ = select.select([self.notify, self.ctl], [], [], 0.1)
            if self.notify in r:
                self.on_notify()
            if self.ctl in r and self.on_control() == "exit":
                self.log("fake systemd exiting")
                return 0
            if self.pid:
                pid, status = os.waitpid(self.pid, os.WNOHANG)
                if pid:
                    self.after_exit(status)
                    continue
                if self.a.watchdog_sec > 0 and time.time() - self.last_ping > self.a.watchdog_sec:
                    self.log("watchdog timeout: no WATCHDOG=1 from pid %d for %ss — SIGABRT" % (self.pid, self.a.watchdog_sec))
                    status = self.kill_and_reap(signal.SIGABRT, 3)
                    self.after_exit(status, forced_failure=True)


def ctl(dirname, cmd):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(60)
    s.connect(os.path.join(dirname, "control"))
    s.sendall(cmd.encode())
    out = b""
    while True:
        b = s.recv(4096)
        if not b:
            break
        out += b
    sys.stdout.write(out.decode())
    return 0


def main():
    if len(sys.argv) >= 4 and sys.argv[1] == "ctl":
        return ctl(sys.argv[2], sys.argv[3])
    p = argparse.ArgumentParser()
    p.add_argument("mode", choices=["run"])
    p.add_argument("--dir", required=True)
    p.add_argument("--log", required=True)
    p.add_argument("--watchdog-sec", type=float, default=6)
    p.add_argument("--restart-sec", type=float, default=1)
    p.add_argument("--burst", type=int, default=5)
    p.add_argument("--interval", type=int, default=120)
    p.add_argument("cmd", nargs="+")
    a = p.parse_args()
    return Supervisor(a).run()


if __name__ == "__main__":
    sys.exit(main())
