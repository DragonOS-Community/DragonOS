#!/bin/sh
# Test: tcp_virtio_bw_64k
# Binary: bw_tcp
# Description: TCP virtio bandwidth test with 64k messages

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
    "${LMBENCH_BIN_DIR}/bw_tcp" -S "$LMBENCH_NET_SERVER" 2>/dev/null || true
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
# 0.0.0.0:31236 (fixed port) and must be stopped via `bw_tcp -S`.
"${LMBENCH_BIN_DIR}/bw_tcp" -s &
SERVER_PID=$!
sleep 2

echo "=== Running TCP bandwidth test (64k) ==="
run_lmbench_measurement "${LMBENCH_BIN_DIR}/bw_tcp" -m 65536 -P 1 "$LMBENCH_NET_SERVER"

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
