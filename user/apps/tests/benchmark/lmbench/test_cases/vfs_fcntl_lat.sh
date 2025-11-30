#!/bin/bash
# Test: vfs_fcntl_lat
# Binary: lat_fcntl
# Description: VFS fcntl latency test

set -e



echo "=== Running VFS fcntl latency test ==="
${LMBENCH_BIN_DIR}/lat_fcntl -P 1 -W 30 -N 200

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
