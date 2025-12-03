#!/bin/bash
# Test: vfs_stat_lat
# Binary: lat_syscall
# Description: VFS stat syscall latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running VFS stat latency test ==="
${LMBENCH_BIN}/lat_syscall -P 1 -W 1000 -N 1000 stat testfile

echo "Test completed successfully"
