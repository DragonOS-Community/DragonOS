#!/bin/sh
# Test: vfs_fcntl_lat
# Binary: lat_fcntl
# Description: VFS fcntl latency test

set -e

# 加载环境变量
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

echo "=== Running VFS fcntl latency test ==="
${LMBENCH_BIN_DIR}/lat_fcntl -P 1 -W 30 -N 200

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
