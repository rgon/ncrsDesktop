# syntax=docker/dockerfile:1
#
# Builds the headless ncrs CLI daemon and runs the end-to-end data-loss suite
# against a real (non-Nextcloud) WebDAV server. Build context is the repo root.
#
# Pass --build-arg PREBUILT_BINARY=1 (with docker/e2e/ncrs/ncrs.deb in the context)
# to skip the Rust compilation stage and extract the binary from a pre-built .deb.

ARG PREBUILT_BINARY=0

# ---- build from source (default, PREBUILT_BINARY=0) -------------------------
FROM rust:1-bookworm AS build-0
RUN apt-get update && apt-get install -y --no-install-recommends \
        libfuse3-dev pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release -p ncrs_core --bin ncrs \
    && cp target/release/ncrs /ncrs

# ---- pre-built .deb injected via build context (PREBUILT_BINARY=1) ----------
FROM debian:bookworm-slim AS build-1
COPY docker/e2e/ncrs/ncrs.deb /ncrs.deb
RUN dpkg-deb --extract /ncrs.deb /deb && cp /deb/usr/bin/ncrs /ncrs

# ---- select which path was used ---------------------------------------------
FROM build-${PREBUILT_BINARY} AS binary

# ---- runtime: FUSE + the daemon + the test harness --------------------------
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        fuse3 libfuse3-3 libssl3 ca-certificates curl coreutils \
    && rm -rf /var/lib/apt/lists/* \
    && echo user_allow_other >> /etc/fuse.conf

COPY --from=binary /ncrs /usr/local/bin/ncrs
COPY docker/e2e/ncrs/entrypoint.sh docker/e2e/ncrs/scenarios.sh /e2e/
RUN chmod +x /usr/local/bin/ncrs /e2e/entrypoint.sh /e2e/scenarios.sh

ENTRYPOINT ["/e2e/entrypoint.sh"]
