#!/bin/bash
# Test: vfs_fcntl_lat
# Binary: lat_fcntl
# Description: VFS fcntl latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running VFS fcntl latency test ==="
${LMBENCH_BIN}/lat_fcntl -P 1 -W 30 -N 200

echo "Test completed successfully"
