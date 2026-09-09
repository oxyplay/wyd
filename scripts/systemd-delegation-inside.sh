#!/usr/bin/env bash
# Runs inside the systemd container (see systemd-delegation-e2e.sh).
# Everything here uses delegation that systemd set up; nothing is prepared by
# hand, which is the point of the check.
set -u

fail=0
check() {
    if [ "$2" = "$3" ]; then
        echo "  ok   $1"
    else
        echo "  FAIL $1: got [$2] want [$3]"
        fail=1
    fi
}

# Capability serialises as "available" or as {"monitored"|"unavailable": ...}.
cap_state() { # cap_state <json> <name>
    python3 -c "
import json, sys
v = json.load(sys.stdin)['capabilities']['$2']
print(v if isinstance(v, str) else list(v)[0])
" <<<"$1"
}

field() { # field <json> <expression over d>
    python3 -c "
import json, sys
d = json.load(sys.stdin)
print($2)
" <<<"$1"
}

echo "-- system manager: transient unit with Delegate=yes --"
cap=$(systemd-run --quiet --unit=wyd-cap --property=Delegate=yes --wait --collect --pipe \
    /usr/local/bin/wyd capacity --json)
check "hard memory available" "$(cap_state "$cap" aggregate_memory_limit)" "available"
check "cpu quota available" "$(cap_state "$cap" cpu_quota)" "available"
check "pids limit available" "$(cap_state "$cap" process_limit)" "available"

# A live supervisor inside the delegated unit reports the aggregate kernel cap.
cap=$(systemd-run --quiet --unit=wyd-cap2 --property=Delegate=yes --wait --collect --pipe \
    /usr/local/bin/wyd capacity --set memory_budget_mb=1024 --json)
check "aggregate cap present" "$(field "$cap" "d.get('aggregate_memory_max_bytes')")" "1073741824"

echo "-- the hard limit must actually be enforced --"
systemd-run --quiet --unit=wyd-oom --property=Delegate=yes --wait --collect --pipe \
    /usr/local/bin/wyd run --timeout 20s --enforce hard --memory 16 -- \
    dd if=/dev/zero of=/dev/shm/blob bs=1M count=64 >/dev/null 2>&1
check "OOM under the system manager" "$?" "137"

echo "-- user manager with linger --"
useradd -m tester 2>/dev/null || true
loginctl enable-linger tester >/dev/null 2>&1 || true
sleep 3
out=$(su - tester -c 'export XDG_RUNTIME_DIR=/run/user/$(id -u); \
    systemd-run --user --quiet --property=Delegate=yes --wait --collect --pipe \
    /usr/local/bin/wyd capacity --json' 2>&1) || true
check "user-manager delegation available" "$(cap_state "$out" aggregate_memory_limit)" "available"

oom=$(su - tester -c 'export XDG_RUNTIME_DIR=/run/user/$(id -u); \
    systemd-run --user --quiet --property=Delegate=yes --wait --collect --pipe \
    /usr/local/bin/wyd run --timeout 20s --enforce hard --memory 16 -- \
    dd if=/dev/zero of=/dev/shm/blob-user bs=1M count=64' 2>&1)
code=$?
check "OOM under the user manager" "$code" "137"
if [ "$code" != "137" ]; then
    echo "  output: $(echo "$oom" | tail -2 | tr '\n' ' ')"
fi

exit "$fail"
