#!/bin/bash
# Test: signal_catch_lat
# Binary: lat_sig
# Description: Signal catch latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running signal_catch_lat test ==="
${LMBENCH_BIN}/lat_sig -P 1 catch

echo "Test completed successfully"
