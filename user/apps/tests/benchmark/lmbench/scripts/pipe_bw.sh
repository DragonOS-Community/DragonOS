#!/bin/bash
# Test: pipe_bw
# Binary: bw_pipe
# Description: Pipe bandwidth test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running pipe_bw test ==="
${LMBENCH_BIN}/bw_pipe -P 1

echo "Test completed successfully"
