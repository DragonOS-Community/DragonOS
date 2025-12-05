#!/bin/bash
# Test: tcp_virtio_connect_lat
# Binary: lat_connect
# Description: TCP virtio connection latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running TCP virtio connection latency test ==="
echo "Note: This test requires a server running at 10.0.2.15"
${LMBENCH_BIN}/lat_connect -s 10.0.2.15 -b 1000

echo "Test completed successfully"
