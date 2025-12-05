#!/bin/bash
# Test: mem_write_bw
# Binary: bw_mem
# Description: Memory write bandwidth test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running mem_write_bw test ==="
${LMBENCH_BIN}/bw_mem -P 1 -N 50 512m fwr

echo "Test completed successfully"
