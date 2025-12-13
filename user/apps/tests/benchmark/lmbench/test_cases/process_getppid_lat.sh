#!/bin/bash
# Test: process_getppid_lat
# Binary: lat_syscall
# Description: Process getppid syscall latency test

set -e



echo "=== Running process_getppid_lat test ==="
${LMBENCH_BIN_DIR}/lat_syscall -P 1 null

echo "Test completed successfully"
