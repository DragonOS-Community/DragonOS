#!/bin/bash
# Test: udp_virtio_lat
# Binary: lat_udp
# Description: UDP virtio latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running UDP virtio latency test ==="
echo "Note: This test requires a server running at 10.0.2.15"
${LMBENCH_BIN}/lat_udp -s 10.0.2.15

echo "Test completed successfully"
