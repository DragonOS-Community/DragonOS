#!/bin/bash
# Test: process_getppid_lat
# Binary: lat_syscall
# Description: Process getppid syscall latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running process_getppid_lat test ==="
${LMBENCH_BIN}/lat_syscall -P 1 null

echo "Test completed successfully"
