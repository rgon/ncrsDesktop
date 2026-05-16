FROM rust:bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    libfuse3-dev pkg-config && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY ncrs_core/ ncrs_core/
COPY ncrs-gui/src-tauri/Cargo.toml ncrs-gui/src-tauri/Cargo.toml
RUN mkdir -p ncrs-gui/src-tauri/src && echo "fn main(){}" > ncrs-gui/src-tauri/src/main.rs && touch ncrs-gui/src-tauri/src/lib.rs
COPY Cargo.toml Cargo.lock ./

# Build only the standalone binary (not the Tauri GUI)
RUN RUSTFLAGS='--cfg reqwest_unstable' cargo build --release --bin ncrs_core -p ncrs_core

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    fuse3 libfuse3-3 iptables iproute2 curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/ncrs_core /usr/local/bin/ncrs
COPY tests/e2e/ncrs-test/run_tests.sh /tests/run_tests.sh
RUN chmod +x /tests/run_tests.sh

CMD ["/tests/run_tests.sh"]
