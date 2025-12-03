#!/bin/bash
# Test: semaphore_lat
# Binary: lat_sem
# Description: Semaphore latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running semaphore_lat test ==="
${LMBENCH_BIN}/lat_sem -P 1 -N 21

echo "Test completed successfully"
