#!/bin/bash
# Test: vfs_write_lat
# Binary: lat_syscall
# Description: VFS write syscall latency test

set -e



echo "=== Running VFS write latency test ==="
${LMBENCH_BIN_DIR}/lat_syscall -P 1 write

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
