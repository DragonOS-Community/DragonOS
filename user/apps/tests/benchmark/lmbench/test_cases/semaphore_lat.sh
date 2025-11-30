#!/bin/bash
# Test: semaphore_lat
# Binary: lat_sem
# Description: Semaphore latency test

set -e



echo "=== Running semaphore_lat test ==="
${LMBENCH_BIN_DIR}/lat_sem -P 1 -N 21

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
