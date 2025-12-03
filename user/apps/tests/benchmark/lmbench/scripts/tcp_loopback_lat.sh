#!/bin/bash
# Test: tcp_loopback_lat
# Binary: lat_tcp
# Description: TCP loopback latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

SERVER_PID=""

cleanup() {
    if [ ! -z "$SERVER_PID" ]; then
        kill $SERVER_PID 2>/dev/null || true
        wait $SERVER_PID 2>/dev/null || true
    fi
}

trap cleanup EXIT INT TERM

echo "=== Starting TCP server ==="
${LMBENCH_BIN}/lat_tcp -s 127.0.0.1 -b 1 &
SERVER_PID=$!
sleep 2

echo "=== Running TCP latency test ==="
${LMBENCH_BIN}/lat_tcp -P 1 127.0.0.1

echo "=== Shutting down server ==="
${LMBENCH_BIN}/lat_tcp -S 127.0.0.1

echo "Test completed successfully"
