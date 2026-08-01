#!/bin/sh
# Test: ramfs_create_delete_files_0k_ops
# Binary: lat_fs
# Description: Create and delete 0k files on ramfs

set -e

# 加载环境变量
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

echo "=== Running ramfs_create_delete_files_0k_ops test ==="
${LMBENCH_BIN_DIR}/lat_fs -s 0k -P 1 ${LMBENCH_TMP_DIR}

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
