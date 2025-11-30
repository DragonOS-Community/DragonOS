#!/bin/bash
# Test: mem_read_bw
# Binary: bw_mem
# Description: Memory read bandwidth test

set -e



echo "=== Running mem_read_bw test ==="
${LMBENCH_BIN_DIR}/bw_mem -P 1 -N 50 512m frd

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
