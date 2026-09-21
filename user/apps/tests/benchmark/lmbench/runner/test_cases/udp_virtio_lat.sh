#!/bin/sh
# Test: udp_virtio_lat
# Binary: lat_udp
# Description: UDP virtio latency test

set -e

# Load environment variables
SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

SERVER_PID=""

cleanup() {
    cleanup_rc=$?
    lmbench_cleanup_capture
    trap - 0 HUP INT TERM
    "${LMBENCH_BIN_DIR}/lat_udp" -S "$LMBENCH_NET_SERVER" 2>/dev/null || true
    if [ ! -z "$SERVER_PID" ]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    exit "$cleanup_rc"
}

trap cleanup 0
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

echo "=== Starting UDP server ==="
# The server forks a daemon; stop it via lat_udp -S during cleanup.
"${LMBENCH_BIN_DIR}/lat_udp" -s &
SERVER_PID=$!
sleep 2

echo "=== Running UDP latency test ==="
run_lmbench_measurement "${LMBENCH_BIN_DIR}/lat_udp" -P 1 "$LMBENCH_NET_SERVER"

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
