#!/bin/bash
# Test: process_fork_lat
# Binary: lat_proc
# Description: Process fork latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running process_fork_lat test ==="
${LMBENCH_BIN}/lat_proc -P 1 fork

echo "Test completed successfully"
