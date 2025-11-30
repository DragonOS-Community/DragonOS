#!/bin/bash
# Test: vfs_read_lat
# Binary: lat_syscall
# Description: VFS read syscall latency test

set -e



echo "=== Running VFS read latency test ==="
${LMBENCH_BIN_DIR}/lat_syscall -P 1 read

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
