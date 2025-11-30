#!/bin/bash
# Test: signal_prot_lat
# Binary: lat_sig
# Description: Signal protection latency test

set -e



echo "=== Running signal_prot_lat test ==="
${LMBENCH_BIN_DIR}/lat_sig -W 30 -N 300 prot ${LMBENCH_EXT4_DIR}/${LMBENCH_TEST_FILE}

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
