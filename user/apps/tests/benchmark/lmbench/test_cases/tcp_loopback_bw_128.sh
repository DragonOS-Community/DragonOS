#!/bin/bash
# Test: tcp_loopback_bw_128
# Binary: bw_tcp
# Description: TCP loopback bandwidth test with 128 byte messages

set -e



SERVER_PID=""

cleanup() {
    if [ ! -z "$SERVER_PID" ]; then
        kill $SERVER_PID 2>/dev/null || true
        wait $SERVER_PID 2>/dev/null || true
    fi
}

trap cleanup EXIT INT TERM

echo "=== Starting TCP server ==="
${LMBENCH_BIN_DIR}/bw_tcp -s 127.0.0.1 -b 1 &
SERVER_PID=$!
sleep 2

echo "=== Running TCP bandwidth test (128 bytes) ==="
${LMBENCH_BIN_DIR}/bw_tcp -m 128 -P 1 127.0.0.1

echo "=== Shutting down server ==="
${LMBENCH_BIN_DIR}/bw_tcp -S 127.0.0.1

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
