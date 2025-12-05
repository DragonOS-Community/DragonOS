#!/bin/bash
# Test: tcp_virtio_bw_64k
# Binary: bw_tcp
# Description: TCP virtio bandwidth test with 64k messages

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running TCP virtio bandwidth test (64k) ==="
echo "Note: This test requires a server running at 10.0.2.15"
${LMBENCH_BIN}/bw_tcp -s 10.0.2.15 -b 1

echo "Test completed successfully"
