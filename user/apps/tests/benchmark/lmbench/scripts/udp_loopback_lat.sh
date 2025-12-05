#!/bin/bash
# Test: udp_loopback_lat
# Binary: lat_udp
# Description: UDP loopback latency test

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

echo "=== Starting UDP server ==="
${LMBENCH_BIN}/lat_udp -s 127.0.0.1 &
SERVER_PID=$!
sleep 2

echo "=== Running UDP latency test ==="
${LMBENCH_BIN}/lat_udp -P 1 127.0.0.1

echo "=== Shutting down server ==="
${LMBENCH_BIN}/lat_udp -S 127.0.0.1

echo "Test completed successfully"
