#!/bin/bash
# Test: tcp_virtio_lat
# Binary: lat_tcp
# Description: TCP virtio latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running TCP virtio latency test ==="
echo "Note: This test requires a server running at 10.0.2.15"
${LMBENCH_BIN}/lat_tcp -s 10.0.2.15 -b 1

echo "Test completed successfully"
