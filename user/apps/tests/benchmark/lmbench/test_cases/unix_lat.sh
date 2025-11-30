#!/bin/bash
# Test: unix_lat
# Binary: lat_unix
# Description: Unix domain socket latency test

set -e



echo "=== Running Unix domain socket latency test ==="
${LMBENCH_BIN_DIR}/lat_unix -P 1

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
