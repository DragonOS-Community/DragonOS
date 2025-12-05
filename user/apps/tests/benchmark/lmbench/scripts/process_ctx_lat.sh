#!/bin/bash
# Test: process_ctx_lat
# Binary: lat_ctx
# Description: Process context switch latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running process_ctx_lat test ==="
${LMBENCH_BIN}/lat_ctx -P 1 18

echo "Test completed successfully"
