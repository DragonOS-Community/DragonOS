#!/bin/bash
# Test: unix_connect_lat
# Binary: lat_unix_connect
# Description: Unix domain socket connection latency test

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

echo "=== Starting Unix socket server ==="
${LMBENCH_BIN}/lat_unix_connect -s &
SERVER_PID=$!
sleep 2

echo "=== Running Unix socket connection latency test ==="
${LMBENCH_BIN}/lat_unix_connect -P 1

echo "Test completed successfully"
