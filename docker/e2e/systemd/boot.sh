#!/bin/sh
# PID 1 wrapper: Docker mounts the (private-namespace) cgroup2 tree read-only
# without --privileged; systemd needs it writable to build its own tree and
# delegate one to the user manager. CAP_SYS_ADMIN allows the remount. Then
# systemd takes over as PID 1.
if ! touch /sys/fs/cgroup/.rw-probe 2>/dev/null; then
    mount -o remount,rw /sys/fs/cgroup || echo "boot: cannot remount /sys/fs/cgroup rw" >&2
fi
rm -f /sys/fs/cgroup/.rw-probe 2>/dev/null
exec /sbin/init "$@"
