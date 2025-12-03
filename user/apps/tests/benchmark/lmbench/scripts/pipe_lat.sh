#!/bin/bash
# Test: pipe_lat
# Binary: lat_pipe
# Description: Pipe latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running pipe_lat test ==="
${LMBENCH_BIN}/lat_pipe -P 1

echo "Test completed successfully"
