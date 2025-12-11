#!/bin/bash
# Test: mem_copy_bw
# Binary: bw_mem
# Description: Memory copy bandwidth test

set -e



echo "=== Running mem_copy_bw test ==="
${LMBENCH_BIN_DIR}/bw_mem -P 1 -N 50 512m fcp

echo "Test completed successfully"
