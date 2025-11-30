#!/bin/bash
# Test: process_ctx_lat
# Binary: lat_ctx
# Description: Process context switch latency test

set -e



echo "=== Running process_ctx_lat test ==="
${LMBENCH_BIN_DIR}/lat_ctx -P 1 18

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
