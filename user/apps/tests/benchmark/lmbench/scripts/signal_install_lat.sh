#!/bin/bash
# Test: signal_install_lat
# Binary: lat_sig
# Description: Signal install latency test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running signal_install_lat test ==="
${LMBENCH_BIN}/lat_sig -P 1 install

echo "Test completed successfully"
