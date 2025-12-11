#!/bin/bash
# Test: signal_install_lat
# Binary: lat_sig
# Description: Signal install latency test

set -e



echo "=== Running signal_install_lat test ==="
${LMBENCH_BIN_DIR}/lat_sig -P 1 install

echo "Test completed successfully"
