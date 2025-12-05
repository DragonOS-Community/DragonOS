#!/bin/bash
# Test: vfs_select_lat
# Binary: lat_select
# Description: VFS select latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running VFS select latency test ==="
${LMBENCH_BIN}/lat_select -P 1 file

echo "Test completed successfully"
