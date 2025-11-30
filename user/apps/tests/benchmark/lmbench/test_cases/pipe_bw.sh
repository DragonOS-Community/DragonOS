#!/bin/bash
# Test: pipe_bw
# Binary: bw_pipe
# Description: Pipe bandwidth test

set -e



echo "=== Running pipe_bw test ==="
${LMBENCH_BIN_DIR}/bw_pipe -P 1

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
