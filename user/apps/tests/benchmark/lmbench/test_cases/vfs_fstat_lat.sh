#!/bin/bash
# Test: vfs_fstat_lat
# Binary: lat_syscall
# Description: VFS fstat syscall latency test

set -e



echo "=== Running VFS fstat latency test ==="
${LMBENCH_BIN_DIR}/lat_syscall -P 1 fstat test_file

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
