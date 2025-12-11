#!/bin/bash
# Test: ext2_copy_files_bw
# Binary: lmdd
# Description: Copy files on ext2 filesystem bandwidth test

set -e

# 检查环境变量


echo "=== Running ext2_copy_files_bw test ==="
${LMBENCH_BIN_DIR}/lmdd if=${LMBENCH_EXT2_DIR}/${LMBENCH_ZERO_FILE} of=${LMBENCH_EXT2_DIR}/${LMBENCH_TEST_FILE}

echo "Test completed successfully"
