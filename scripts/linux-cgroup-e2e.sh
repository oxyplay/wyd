#!/usr/bin/env bash
# Linux cgroup v2 end-to-end check for managed runs.
#
# Needs a delegated subtree. Run it in a privileged container that prepares
# one the way systemd's user manager would:
#
#   docker run --rm --privileged --cgroupns=private \
#     -v "$PWD":/src:ro -v "$PWD/scripts/linux-cgroup-e2e.sh":/e2e.sh:ro \
#     -e CARGO_TARGET_DIR=/tmp/target -e CARGO_HOME=/tmp/cargo -w /src \
#     rust:1-slim-bookworm bash -c \
#     'apt-get update -qq && apt-get install -y -qq python3 procps && \
#      cargo build --release && bash /e2e.sh'
#
# Checks: hard limits are unavailable without delegation and refused; available
# with it; a hard memory limit produces a real OOM kill with the kernel's
# evidence; pids.max is applied; a setsid escapee is still killed and the run
# reports complete cleanup; no cgroup directory is left behind.
set -u

B=${WYD_BIN:-/tmp/target/release/wyd}
export HOME=${WYD_HOME:-/tmp/wydhome}
mkdir -p "$HOME"
fail=0

check() { # check <description> <actual> <expected>
    if [ "$2" = "$3" ]; then
        echo "  ok   $1"
    else
        echo "  FAIL $1: got [$2] want [$3]"
        fail=1
    fi
}

field() { # field <python expression over r> <json>
    python3 -c "import json,sys;r=json.load(sys.stdin);print($1)" <<<"$2"
}

# Capability serialises as "available" or as {"monitored"|"unavailable": ...}.
cap_state() { # cap_state <json> <name>
    python3 -c "
import json,sys
v = json.load(sys.stdin)['capabilities']['$2']
print(v if isinstance(v, str) else list(v)[0])
" <<<"$1"
}

echo "== A. no delegated subtree =="
check "root subtree_control empty" "$(cat /sys/fs/cgroup/cgroup.subtree_control)" ""
cap=$("$B" capacity --json)
check "memory capability" "$(cap_state "$cap" aggregate_memory_limit)" "unavailable"
code=0
out=$("$B" run --enforce hard --memory 32 -- true 2>&1) || code=$?
check "hard request refused" "$code" "1"
grep -q "not available on this backend" <<<"$out" || { echo "  FAIL refusal message: $out"; fail=1; }

echo "== B. delegated subtree =="
# Prepare what systemd's user manager would: a process-free cgroup whose
# subtree_control enables the controllers our runs need.
mkdir -p /sys/fs/cgroup/payload
for p in $(cat /sys/fs/cgroup/cgroup.procs); do
    echo "$p" > /sys/fs/cgroup/payload/cgroup.procs 2>/dev/null || true
done
echo "+memory +pids +cpu" > /sys/fs/cgroup/cgroup.subtree_control
mkdir -p /sys/fs/cgroup/wyd
echo "+memory +pids +cpu" > /sys/fs/cgroup/wyd/cgroup.subtree_control
export WYD_CGROUP_ROOT=/sys/fs/cgroup/wyd

# Capabilities are fixed when the supervisor starts, so start a fresh one.
pkill -f "wyd serve" 2>/dev/null || true
sleep 0.5

cap=$("$B" capacity --json)
for c in aggregate_memory_limit cpu_quota process_limit; do
    check "$c available" "$(cap_state "$cap" "$c")" "available"
done

echo "-- hard memory limit must OOM-kill --"
"$B" run --timeout 20s --enforce hard --memory 16 -- \
    dd if=/dev/zero of=/dev/shm/blob bs=1M count=64 >/dev/null 2>&1
code=$?
check "exit code" "$code" "137"
run=$("$B" runs --json | python3 -c 'import json,sys;print(json.dumps(json.load(sys.stdin)[0]))')
check "outcome" "$(field "r['outcome']" "$run")" "resource_limit"
check "cleanup" "$(field "r['cleanup']" "$run")" "complete"
check "backend" "$(field "r['effective']['backend']" "$run")" "cgroup_v2"
check "memory limit applied" "$(field "r['effective']['memory_bytes']" "$run")" "16777216"
grep -q "OOM-killed" <<<"$(field "r.get('limit_event') or ''" "$run")" ||
    { echo "  FAIL OOM evidence missing"; fail=1; }

echo "-- pids.max must be applied --"
"$B" run --timeout 20s --enforce hard --processes 8 -- \
    sh -c 'for i in $(seq 1 40); do sleep 30 & done; wait' >/dev/null 2>&1 || true
run=$("$B" runs --json | python3 -c 'import json,sys;print(json.dumps(json.load(sys.stdin)[0]))')
check "pids limit applied" "$(field "r['effective']['processes']" "$run")" "8"
check "pids backend" "$(field "r['effective']['backend']" "$run")" "cgroup_v2"

echo "-- a setsid escapee must still die with the run --"
"$B" run --timeout 10s -- sh -c 'setsid sleep 30 >/dev/null 2>&1 & echo $!; exit 0' >/dev/null 2>&1 || true
sleep 0.5
live=$(pgrep -c 'sleep 30' 2>/dev/null || true)
check "no live escapee" "${live:-0}" "0"
run=$("$B" runs --json | python3 -c 'import json,sys;print(json.dumps(json.load(sys.stdin)[0]))')
check "escapee cleanup" "$(field "r['cleanup']" "$run")" "complete"
left=$(find /sys/fs/cgroup/wyd -maxdepth 1 -name 'run-*' | wc -l | tr -d ' ')
check "no cgroup left behind" "$left" "0"

echo
if [ "$fail" -eq 0 ]; then
    echo "PASS"
else
    echo "FAIL"
fi
exit "$fail"
