#!/bin/sh
# Test: tcp_virtio_lat
# Binary: lat_tcp
# Description: TCP virtio latency test

set -e

# Load environment variables
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

SERVER_PID=""

cleanup() {
    ${LMBENCH_BIN_DIR}/lat_tcp -S 10.0.2.15 2>/dev/null || true
    if [ ! -z "$SERVER_PID" ]; then
        kill $SERVER_PID 2>/dev/null || true
        wait $SERVER_PID 2>/dev/null || true
    fi
}

trap cleanup EXIT INT TERM

echo "=== Starting TCP server ==="
# lmbench server mode (-s) double-forks a daemon and the launcher exits 0
# immediately; any arguments after -s are ignored. The daemon binds
# 0.0.0.0:31234 (fixed port) and must be stopped via `lat_tcp -S`.
${LMBENCH_BIN_DIR}/lat_tcp -s &
SERVER_PID=$!
sleep 2

echo "=== Running TCP latency test ==="
${LMBENCH_BIN_DIR}/lat_tcp -P 1 10.0.2.15

echo "=== Shutting down server ==="
${LMBENCH_BIN_DIR}/lat_tcp -S 10.0.2.15

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
