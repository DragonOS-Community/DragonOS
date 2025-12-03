#!/bin/bash
# Test: ext2_copy_files_bw
# Binary: lmdd
# Description: Copy files on ext2 filesystem bandwidth test

set -e

# 检查环境变量
if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running ext2_copy_files_bw test ==="
${LMBENCH_BIN}/lmdd if=${LMBENCH_EXT2_DIR}/${LMBENCH_ZERO_FILE} of=${LMBENCH_EXT2_DIR}/${LMBENCH_TEST_FILE}

echo "Test completed successfully"
