#!/bin/bash
# Test: tcp_loopback_select_lat
# Binary: lat_select
# Description: TCP loopback select latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running TCP select latency test ==="
${LMBENCH_BIN}/lat_select -P 1 tcp

echo "Test completed successfully"
