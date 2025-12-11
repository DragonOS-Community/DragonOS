#!/bin/bash
# Test: vfs_open_lat
# Binary: lat_syscall
# Description: VFS open syscall latency test

set -e



echo "=== Running VFS open latency test ==="
${LMBENCH_BIN_DIR}/lat_syscall -P 1 -W 1000 -N 1000 open testfile

echo "Test completed successfully"
