#!/bin/bash
# Test: pipe_lat
# Binary: lat_pipe
# Description: Pipe latency test

set -e



echo "=== Running pipe_lat test ==="
${LMBENCH_BIN_DIR}/lat_pipe -P 1

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
