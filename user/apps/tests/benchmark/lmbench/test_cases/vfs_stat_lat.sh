#!/bin/bash
# Test: vfs_stat_lat
# Binary: lat_syscall
# Description: VFS stat syscall latency test

set -e



echo "=== Running VFS stat latency test ==="
${LMBENCH_BIN_DIR}/lat_syscall -P 1 -W 1000 -N 1000 stat testfile

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
