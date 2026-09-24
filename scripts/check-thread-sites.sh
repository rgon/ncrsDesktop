#!/usr/bin/env bash
# Fails if a bg:: pool or service thread is not documented in docs/threads.md,
# so the thread graph there stays the complete list of what the daemon runs.
set -euo pipefail
cd "$(dirname "$0")/.."
doc=docs/threads.md
src=ncrs_core/src
missing=0
pools=$(grep -ho 'pub static [A-Z_]*: Pool = Pool::new("[a-z-]*"' "$src/bg.rs" | sed 's/.*Pool::new("\([a-z-]*\)"/\1/' | sort -u)
services=$(grep -rho --include='*.rs' '\(start_service\|spawn_service\)("[a-z-]*"' "$src" | sed 's/.*("\([a-z-]*\)"/\1/' | sort -u)
for name in $pools $services; do
    if ! grep -q "\`$name\`" "$doc"; then
        echo "::error file=$doc::thread '$name' is created in $src but not documented in $doc"
        missing=1
    fi
done
[ "$missing" = 0 ] && echo "thread sites: $(echo $pools | wc -w) pools, $(echo $services | wc -w) services, all documented"
exit "$missing"
