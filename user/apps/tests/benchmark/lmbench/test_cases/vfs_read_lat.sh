#!/bin/bash
# Test: vfs_read_lat
# Binary: lat_syscall
# Description: VFS read syscall latency test

set -e



echo "=== Running VFS read latency test ==="
${LMBENCH_BIN_DIR}/lat_syscall -P 1 read

echo "Test completed successfully"
