#!/bin/sh
# Regression test for tcp_loopback_lat result extraction.

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
RUNNER="$SCRIPT_DIR/../run_tests.sh"
META="$SCRIPT_DIR/tcp_loopback_lat.meta"
TMPDIR=${TMPDIR:-/tmp}
OUTPUT_FILE=$(mktemp "$TMPDIR/lmbench-tcp-parser.XXXXXX")

cleanup() {
    rm -f "$OUTPUT_FILE"
}
trap cleanup EXIT HUP INT TERM

cat > "$OUTPUT_FILE" <<'EOF'
timeout: warning: timer_create: Invalid argument
=== Starting TCP server ===
=== Running TCP latency test ===
TCP latency using 127.0.0.1: 4745.8345 microseconds
=== Shutting down server ===
Test completed successfully
EOF

LMBENCH_RUNNER_NO_MAIN=1
export LMBENCH_RUNNER_NO_MAIN
. "$RUNNER"

PAT=$(kv_get "$META" SEARCH_PATTERN)
IDX=$(kv_get "$META" RESULT_INDEX)
NTH=$(kv_get "$META" NTH_OCCURRENCE)
value=$(extract_value "$OUTPUT_FILE")

[ "$value" = "4745.8345" ] || {
    printf 'expected 4745.8345, got %s\n' "${value:-<empty>}" >&2
    exit 1
}

printf 'tcp_loopback_lat parser regression: PASS\n'
