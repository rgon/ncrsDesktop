# Runtime-only image for the walker harness: NO Rust build. The ncrs binary is
# bind-mounted from the host (NCRS_BIN), so the base must carry a glibc at least
# as new as the host that built it (Ubuntu 24.04 = glibc 2.39). The same image
# runs the fault proxy and the volume seeder.
FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends \
        fuse3 curl ca-certificates python3 procps coreutils findutils \
    && rm -rf /var/lib/apt/lists/* \
    && echo user_allow_other >> /etc/fuse.conf
COPY faultproxy.py sampler.py nofreeze.py uploadfreeze.py run_walker.sh seed_tree.sh /walker/
RUN chmod +x /walker/*.sh /walker/*.py
