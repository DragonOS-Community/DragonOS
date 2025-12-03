#!/bin/bash
# Test: mem_mmap_lat
# Binary: lat_mmap
# Description: Memory mmap latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running mem_mmap_lat test ==="
${LMBENCH_BIN}/lat_mmap 4m ${LMBENCH_EXT2_DIR}/${LMBENCH_TEST_FILE}

echo "Test completed successfully"
