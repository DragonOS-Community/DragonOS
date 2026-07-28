#!/bin/sh
# Test: vfs_read_lat
# Binary: lat_syscall
# Description: VFS read syscall latency test

set -e

# 加载环境变量
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

echo "=== Running VFS read latency test ==="
${LMBENCH_BIN_DIR}/lat_syscall -P 1 read

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
