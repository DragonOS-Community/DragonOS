#!/bin/sh
# Test: tcp_virtio_connect_lat
# Binary: lat_connect
# Description: TCP virtio connection latency test

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
    "${LMBENCH_BIN_DIR}/lat_connect" -S "$LMBENCH_NET_SERVER" 2>/dev/null || true
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

echo "=== Starting TCP server ==="
# lmbench server mode (-s) double-forks a daemon and the launcher exits 0
# immediately; any arguments after -s are ignored. The daemon binds
# 0.0.0.0:31237 (fixed port) and must be stopped via `lat_connect -S`.
"${LMBENCH_BIN_DIR}/lat_connect" -s &
SERVER_PID=$!
sleep 2

echo "=== Running TCP connection latency test ==="
run_lmbench_measurement "${LMBENCH_BIN_DIR}/lat_connect" "$LMBENCH_NET_SERVER"

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
