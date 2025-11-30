#!/bin/bash
# Test: mem_mmap_lat
# Binary: lat_mmap
# Description: Memory mmap latency test

set -e



echo "=== Running mem_mmap_lat test ==="
${LMBENCH_BIN_DIR}/lat_mmap 4m ${LMBENCH_EXT4_DIR}/${LMBENCH_TEST_FILE}

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
