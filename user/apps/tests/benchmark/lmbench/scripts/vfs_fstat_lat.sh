#!/bin/bash
# Test: vfs_fstat_lat
# Binary: lat_syscall
# Description: VFS fstat syscall latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running VFS fstat latency test ==="
${LMBENCH_BIN}/lat_syscall -P 1 fstat test_file

echo "Test completed successfully"
