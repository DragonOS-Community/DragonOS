#!/bin/sh
# Test: ext4_create_delete_files_10k_ops
# Binary: lat_fs
# Description: Create and delete 10k files on ext4 filesystem

set -e

# 加载环境变量
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

echo "=== Running ext4_create_delete_files_10k_ops test ==="
${LMBENCH_BIN_DIR}/lat_fs -s 10k -P 1 ${LMBENCH_EXT4_DIR}

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
