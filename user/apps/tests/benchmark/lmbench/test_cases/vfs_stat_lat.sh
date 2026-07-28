#!/bin/sh
# Test: vfs_stat_lat
# Binary: lat_syscall
# Description: VFS stat syscall latency test

set -e

# 加载环境变量
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

echo "=== Running VFS stat latency test ==="
touch testfile
${LMBENCH_BIN_DIR}/lat_syscall -P 1 -W 1000 -N 1000 stat testfile
rm testfile

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
