# syntax=docker/dockerfile:1
#
# Builds the headless ncrs CLI daemon and runs the end-to-end data-loss suite
# against a real (non-Nextcloud) WebDAV server. Build context is the repo root.
#
# Pass --build-arg PREBUILT_BINARY=1 (with docker/e2e/ncrs/ncrs.deb in the context)
# to skip the Rust compilation stage and extract the binaries from a pre-built .deb.

ARG PREBUILT_BINARY=0

# ---- build from source (default, PREBUILT_BINARY=0) -------------------------
FROM rust:1-bookworm AS build-0
RUN apt-get update && apt-get install -y --no-install-recommends \
        libfuse3-dev pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release -p ncrs_core -p ncrs_passthrough --bin ncrs --bin ncrs-passthrough \
    && cp target/release/ncrs /ncrs \
    && cp target/release/ncrs-passthrough /ncrs-passthrough

# ---- pre-built .deb injected via build context (PREBUILT_BINARY=1) ----------
FROM debian:bookworm-slim AS build-1
COPY docker/e2e/ncrs/ncrs.deb /ncrs.deb
RUN dpkg-deb --extract /ncrs.deb /deb && cp /deb/usr/bin/ncrs /ncrs \
    && cp /deb/usr/libexec/ncrs/ncrs-passthrough /ncrs-passthrough

# ---- select which path was used ---------------------------------------------
FROM build-${PREBUILT_BINARY} AS binary

# ---- runtime: FUSE + the daemon + the test harness --------------------------
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        fuse3 libfuse3-3 libssl3 ca-certificates curl coreutils iptables procps netcat-openbsd \
        libglib2.0-0 python3 libcap2-bin \
    && rm -rf /var/lib/apt/lists/* \
    && echo user_allow_other >> /etc/fuse.conf \
    # Scenario 33 runs a daemon as an ordinary user, as a desktop session does.
    && useradd -m -u 1500 -s /bin/sh ptuser

COPY --from=binary /ncrs /usr/local/bin/ncrs
# The passthrough helper where the packages put it, with the one capability the
# packages grant (to it alone; the daemon gets none — scenario 33).
COPY --from=binary /ncrs-passthrough /usr/libexec/ncrs/ncrs-passthrough
RUN chmod 0755 /usr/libexec/ncrs/ncrs-passthrough \
    && setcap cap_sys_admin=ep /usr/libexec/ncrs/ncrs-passthrough
COPY docker/e2e/ncrs/entrypoint.sh docker/e2e/ncrs/scenarios.sh docker/e2e/ncrs/chunkdav.py docker/e2e/ncrs/fake_systemd.py /e2e/
RUN chmod +x /usr/local/bin/ncrs /e2e/entrypoint.sh /e2e/scenarios.sh /e2e/chunkdav.py /e2e/fake_systemd.py

ENTRYPOINT ["/e2e/entrypoint.sh"]
