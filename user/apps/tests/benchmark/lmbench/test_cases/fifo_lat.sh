#!/bin/bash
# Test: fifo_lat
# Binary: lat_fifo
# Description: FIFO latency test

set -e



echo "=== Running fifo_lat test ==="
${LMBENCH_BIN_DIR}/lat_fifo -P 1

echo "Test completed successfully"
