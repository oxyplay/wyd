#!/usr/bin/env bash
# Verify cgroup delegation the way a real machine provides it: through
# systemd, with no manual cgroup preparation.
#
# Runs a systemd container (PID 1), builds wyd for Linux, and inside the
# container:
#   1. runs `wyd capacity` in a transient unit with Delegate=yes and asserts
#      the hard limits are available;
#   2. runs a hard-limited command through the supervisor and asserts the
#      kernel OOM-killed it;
#   3. tries the same through a user manager (linger) and reports the result.
#
# Usage: scripts/systemd-delegation-e2e.sh   (needs Docker; run from the repo)
set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
IMAGE=wyd-systemd-test
CONTAINER=wyd-systemd-e2e
BINVOL=wyd-e2e-bin
fail=0

cleanup() {
    docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
    docker volume rm "$BINVOL" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "== build the Linux binary =="
docker volume create "$BINVOL" >/dev/null
docker run --rm -v "$REPO":/src:ro -v "$BINVOL":/out \
    -e CARGO_TARGET_DIR=/tmp/target -e CARGO_HOME=/tmp/cargo -w /src \
    rust:1-slim-bookworm bash -c \
    'cargo build --release 2>&1 | tail -1 && cp /tmp/target/release/wyd /out/wyd'

echo "== build the systemd image =="
docker build -q -f "$REPO/scripts/systemd-delegation.dockerfile" -t "$IMAGE" "$REPO" >/dev/null

echo "== start systemd =="
docker run -d --name "$CONTAINER" --privileged --cgroupns=private \
    --tmpfs /run --tmpfs /run/lock -v "$BINVOL":/usr/local/bin "$IMAGE" >/dev/null
for _ in $(seq 1 40); do
    state=$(docker exec "$CONTAINER" systemctl is-system-running 2>/dev/null || true)
    case "$state" in running|degraded) break ;; esac
    sleep 1
done
echo "systemd state: ${state:-unknown}"
[ "$state" = "running" ] || [ "$state" = "degraded" ] || { echo "FAIL: systemd did not come up"; exit 1; }

echo "== delegation through systemd (no manual cgroup setup) =="
docker cp "$REPO/scripts/systemd-delegation-inside.sh" "$CONTAINER:/inside.sh" >/dev/null
docker exec "$CONTAINER" bash /inside.sh
inside=$?
[ "$inside" -eq 0 ] || fail=1

echo
[ "$fail" -eq 0 ] && echo "PASS" || echo "FAIL"
exit "$fail"
