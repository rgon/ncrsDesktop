# syntax=docker/dockerfile:1
#
# Real systemd as PID 1, running ncrs as the packaged user unit, for the
# restart-survival e2e test (scripts/e2e-systemd.sh). Ubuntu 24.04 ships
# systemd 255 — what the desktop runs; FileDescriptorStorePreserve= needs
# >= 254, so Debian bookworm's 252 would not do.
#
# The build context is a directory scripts/e2e-systemd.sh stages with just:
# Dockerfile, ncrs.deb (the prebuilt daemon, see the verify skill),
# ncrs.service (packaging/ncrs.service, unchanged), chunkdav.py, test.sh,
# chunkdav.service, boot.sh.
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        systemd systemd-sysv dbus dbus-user-session libpam-systemd \
        fuse3 libfuse3-3 libssl3 ca-certificates libglib2.0-0 \
        python3 curl netcat-openbsd procps coreutils \
    && rm -rf /var/lib/apt/lists/* \
    && echo user_allow_other >> /etc/fuse.conf \
    # Nothing a container needs; some of these fail without hardware.
    && systemctl mask systemd-udevd.service systemd-udevd-control.socket systemd-udevd-kernel.socket \
        systemd-modules-load.service systemd-remount-fs.service getty.target console-getty.service \
        systemd-networkd-wait-online.service

# The daemon, where the unit's ExecStart expects it.
COPY ncrs.deb /tmp/ncrs.deb
RUN dpkg-deb --extract /tmp/ncrs.deb /tmp/deb && install -m 0755 /tmp/deb/usr/bin/ncrs /usr/bin/ncrs \
    && rm -rf /tmp/deb /tmp/ncrs.deb
COPY ncrs.service /usr/lib/systemd/user/ncrs.service

# The WebDAV front (chunked uploads + a slow marker) runs as a system service
# next to it; it forwards to the rclone container on the network as "webdav".
COPY chunkdav.py /e2e/chunkdav.py
COPY chunkdav.service /etc/systemd/system/chunkdav.service
RUN systemctl enable chunkdav.service
COPY test.sh boot.sh /e2e/
RUN chmod +x /e2e/chunkdav.py /e2e/test.sh /e2e/boot.sh \
    && touch /var/log/chunkdav.log && chmod 0644 /var/log/chunkdav.log \
    && mkdir -p /var/log/journal

# The test user: lingering, so logind starts user@1000.service at boot; in
# systemd-journal, so `journalctl --user` reads the journal (persistent, via
# /var/log/journal above, so user units get their own user-1000.journal).
RUN userdel -r ubuntu 2>/dev/null; \
    useradd -m -u 1000 -s /bin/bash -G systemd-journal tester \
    && mkdir -p /var/lib/systemd/linger && touch /var/lib/systemd/linger/tester \
    && install -d -o tester -g tester -m 0700 /home/tester/.config /home/tester/.config/ncrs \
    && install -d -o tester -g tester /home/tester/Nextcloud \
    && printf '%s\n' \
        'url: "http://127.0.0.1:8090/remote.php/dav/files/testuser/"' \
        'username: "testuser"' \
        'password: "testpass"' \
        'user: "testuser"' \
        'mount_point: "/home/tester/Nextcloud"' \
        'allow_insecure_http: true' \
        'http3: false' \
        'optimistic_listing: false' \
        'auto_keep_cached_files: false' \
        > /home/tester/.config/ncrs/config.yaml \
    && chown tester:tester /home/tester/.config/ncrs/config.yaml && chmod 0600 /home/tester/.config/ncrs/config.yaml

STOPSIGNAL SIGRTMIN+3
CMD ["/e2e/boot.sh"]
