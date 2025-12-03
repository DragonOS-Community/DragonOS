#!/bin/bash
# Test: unix_bw
# Binary: bw_unix
# Description: Unix domain socket bandwidth test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running Unix domain socket bandwidth test ==="
${LMBENCH_BIN}/bw_unix -P 1

echo "Test completed successfully"
