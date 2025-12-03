#!/bin/bash
# Test: vfs_read_lat
# Binary: lat_syscall
# Description: VFS read syscall latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running VFS read latency test ==="
${LMBENCH_BIN}/lat_syscall -P 1 read

echo "Test completed successfully"
