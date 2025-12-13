#!/bin/bash
# Test: process_shell_lat
# Binary: lat_proc
# Description: Process shell latency test

set -e



echo "=== Running process_shell_lat test ==="
${LMBENCH_BIN_DIR}/lat_proc -P 1 shell

echo "Test completed successfully"
