#!/usr/bin/env bash
# Run the end-to-end suite: build the WebDAV server + ncrs CLI daemon images,
# mount over FUSE, and run the data-loss scenarios. Exits non-zero if any
# scenario fails. Requires docker (with compose) and /dev/fuse.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
COMPOSE_FILE="$REPO_ROOT/docker/e2e/docker-compose.yml"

cleanup() {
    docker compose -f "$COMPOSE_FILE" down -v --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "→ building e2e images"
docker compose -f "$COMPOSE_FILE" build

echo "→ running e2e scenarios"
docker compose -f "$COMPOSE_FILE" up \
    --abort-on-container-exit \
    --exit-code-from ncrs
