#!/bin/bash
# Test: tcp_loopback_select_lat
# Binary: lat_select
# Description: TCP loopback select latency test

set -e



echo "=== Running TCP select latency test ==="
${LMBENCH_BIN_DIR}/lat_select -P 1 tcp

echo "Test completed successfully"
