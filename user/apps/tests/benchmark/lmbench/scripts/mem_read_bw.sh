#!/bin/bash
# Test: mem_read_bw
# Binary: bw_mem
# Description: Memory read bandwidth test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running mem_read_bw test ==="
${LMBENCH_BIN}/bw_mem -P 1 -N 50 512m frd

echo "Test completed successfully"
