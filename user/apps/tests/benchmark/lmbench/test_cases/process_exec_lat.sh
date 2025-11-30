#!/bin/bash
# Test: process_exec_lat
# Binary: lat_proc
# Description: Process exec latency test

set -e



echo "=== Running process_exec_lat test ==="
${LMBENCH_BIN_DIR}/lat_proc -P 1 exec

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
