#!/bin/bash
# Test: process_shell_lat
# Binary: lat_proc
# Description: Process shell latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running process_shell_lat test ==="
${LMBENCH_BIN}/lat_proc -P 1 shell

echo "Test completed successfully"
