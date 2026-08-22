#!/bin/sh
# Test: mem_mmap_lat
# Binary: lat_mmap
# Description: Memory mmap latency test

set -e

# 加载环境变量
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

echo "=== Running mem_mmap_lat test ==="
${LMBENCH_BIN_DIR}/lat_mmap 4m ${LMBENCH_EXT4_DIR}/${LMBENCH_TEST_FILE}

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
