#!/bin/sh
# Test: process_exec_lat
# Binary: lat_proc
# Description: Process exec latency test

set -e

# 加载环境变量
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

echo "=== Running process_exec_lat test ==="
${LMBENCH_BIN_DIR}/lat_proc -P 1 exec

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
