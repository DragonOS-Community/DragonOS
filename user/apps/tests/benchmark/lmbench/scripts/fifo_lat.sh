#!/bin/bash
# Test: fifo_lat
# Binary: lat_fifo
# Description: FIFO latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running fifo_lat test ==="
${LMBENCH_BIN}/lat_fifo -P 1

echo "Test completed successfully"
