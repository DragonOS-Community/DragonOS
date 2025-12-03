#!/bin/bash
# Test: process_exec_lat
# Binary: lat_proc
# Description: Process exec latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running process_exec_lat test ==="
${LMBENCH_BIN}/lat_proc -P 1 exec

echo "Test completed successfully"
