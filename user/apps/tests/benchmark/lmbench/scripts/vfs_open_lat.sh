#!/bin/bash
# Test: vfs_open_lat
# Binary: lat_syscall
# Description: VFS open syscall latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running VFS open latency test ==="
${LMBENCH_BIN}/lat_syscall -P 1 -W 1000 -N 1000 open testfile

echo "Test completed successfully"
