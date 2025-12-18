#!/bin/bash
# Test: ramfs_create_delete_files_10k_ops
# Binary: lat_fs
# Description: Create and delete 10k files on ramfs

set -e

# 加载环境变量
SCTIPDIR=$(cd $(dirname ${BASH_SOURCE[0]}) > /dev/null && pwd)
ENV_PATH="${SCTIPDIR}/../env.sh"
source ${ENV_PATH}

echo "=== Running ramfs_create_delete_files_10k_ops test ==="
${LMBENCH_BIN_DIR}/lat_fs -s 10k -P 1 -W 30 -N 300

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
