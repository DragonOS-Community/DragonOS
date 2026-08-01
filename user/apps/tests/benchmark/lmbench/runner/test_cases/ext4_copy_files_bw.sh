#!/bin/sh
# Test: ext4_copy_files_bw
# Binary: lmdd
# Description: Copy files on ext4 filesystem bandwidth test


set -e

# 加载环境变量
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"


echo "=== Running ext4_copy_files_bw test ==="
${LMBENCH_BIN_DIR}/lmdd if=${LMBENCH_EXT4_DIR}/${LMBENCH_ZERO_FILE} of=${LMBENCH_EXT4_DIR}/${LMBENCH_TEST_FILE}

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
