#!/bin/bash
# Test: mem_copy_bw
# Binary: bw_mem
# Description: Memory copy bandwidth test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running mem_copy_bw test ==="
${LMBENCH_BIN}/bw_mem -P 1 -N 50 512m fcp

echo "Test completed successfully"
