#!/bin/sh
# Test: mem_copy_bw
# Binary: bw_mem
# Description: Memory copy bandwidth test

set -e

# 加载环境变量
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

echo "=== Running mem_copy_bw test ==="
${LMBENCH_BIN_DIR}/bw_mem -P 1 -N 3 64m fcp

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
