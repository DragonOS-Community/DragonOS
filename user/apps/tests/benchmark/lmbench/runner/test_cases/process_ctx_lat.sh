#!/bin/sh
# Test: process_ctx_lat
# Binary: lat_ctx
# Description: Process context switch latency test

set -e

# 加载环境变量
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

echo "=== Running process_ctx_lat test ==="
${LMBENCH_BIN_DIR}/lat_ctx -P 1 18

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
