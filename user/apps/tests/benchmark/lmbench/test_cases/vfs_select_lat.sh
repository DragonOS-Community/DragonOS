#!/bin/bash
# Test: vfs_select_lat
# Binary: lat_select
# Description: VFS select latency test

set -e



echo "=== Running VFS select latency test ==="
${LMBENCH_BIN_DIR}/lat_select -P 1 file

echo "Test completed successfully"
