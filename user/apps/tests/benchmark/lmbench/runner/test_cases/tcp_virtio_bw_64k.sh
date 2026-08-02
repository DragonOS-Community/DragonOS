#!/bin/sh
# Test: tcp_virtio_bw_64k
# Binary: bw_tcp
# Description: TCP virtio bandwidth test with 64k messages

set -e

# Load environment variables
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

SERVER_PID=""

cleanup() {
    if [ ! -z "$SERVER_PID" ]; then
        kill $SERVER_PID 2>/dev/null || true
        wait $SERVER_PID 2>/dev/null || true
    fi
}

trap cleanup EXIT INT TERM

echo "=== Starting TCP server ==="
# lmbench server mode (-s) double-forks a daemon and the launcher exits 0
# immediately; any arguments after -s are ignored. The daemon binds
# 0.0.0.0:31236 (fixed port) and must be stopped via `bw_tcp -S`.
${LMBENCH_BIN_DIR}/bw_tcp -s &
SERVER_PID=$!
sleep 2

echo "=== Running TCP bandwidth test (64k) ==="
${LMBENCH_BIN_DIR}/bw_tcp -m 65536 -P 1 10.0.2.15

echo "=== Shutting down server ==="
${LMBENCH_BIN_DIR}/bw_tcp -S 10.0.2.15

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
