#!/bin/sh
# Test: vfs_select_lat
# Binary: lat_select
# Description: VFS select latency test

set -e

# 加载环境变量
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

echo "=== Running VFS select latency test ==="
${LMBENCH_BIN_DIR}/lat_select -P 1 file

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
