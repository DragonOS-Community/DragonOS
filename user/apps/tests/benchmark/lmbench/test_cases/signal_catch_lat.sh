#!/bin/bash
# Test: signal_catch_lat
# Binary: lat_sig
# Description: Signal catch latency test

set -e



echo "=== Running signal_catch_lat test ==="
${LMBENCH_BIN_DIR}/lat_sig -P 1 catch

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
