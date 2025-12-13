#!/bin/bash
# Test: tcp_virtio_lat
# Binary: lat_tcp
# Description: TCP virtio latency test

set -e



echo "=== Running TCP virtio latency test ==="
echo "Note: This test requires a server running at 10.0.2.15"
${LMBENCH_BIN_DIR}/lat_tcp -s 10.0.2.15 -b 1

echo "Test completed successfully"
