#!/bin/bash
# Test: mem_mmap_bw
# Binary: bw_mmap_rd
# Description: Memory mmap bandwidth test

set -e



echo "=== Running mem_mmap_bw test ==="
${LMBENCH_BIN_DIR}/bw_mmap_rd -W 30 -N 300 256m mmap_only ${LMBENCH_EXT2_DIR}/${LMBENCH_TEST_FILE}

echo "Test completed successfully"
