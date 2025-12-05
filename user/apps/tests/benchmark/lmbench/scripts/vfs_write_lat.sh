#!/bin/bash
# Test: vfs_write_lat
# Binary: lat_syscall
# Description: VFS write syscall latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running VFS write latency test ==="
${LMBENCH_BIN}/lat_syscall -P 1 write

echo "Test completed successfully"
