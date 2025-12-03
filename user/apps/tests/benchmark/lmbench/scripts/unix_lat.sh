#!/bin/bash
# Test: unix_lat
# Binary: lat_unix
# Description: Unix domain socket latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running Unix domain socket latency test ==="
${LMBENCH_BIN}/lat_unix -P 1

echo "Test completed successfully"
