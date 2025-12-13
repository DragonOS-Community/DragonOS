#!/bin/bash
# Test: signal_prot_lat
# Binary: lat_sig
# Description: Signal protection latency test

set -e



echo "=== Running signal_prot_lat test ==="
${LMBENCH_BIN_DIR}/lat_sig -W 30 -N 300 prot ${LMBENCH_EXT2_DIR}/${LMBENCH_TEST_FILE}

echo "Test completed successfully"
