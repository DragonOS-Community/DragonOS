#!/bin/bash
# Test: ext4_copy_files_bw
# Binary: lmdd
# Description: Copy files on ext4 filesystem bandwidth test


set -e




echo "=== Running ext4_copy_files_bw test ==="
sudo ${LMBENCH_BIN_DIR}/lmdd if=${LMBENCH_EXT4_DIR}/${LMBENCH_ZERO_FILE} of=${LMBENCH_EXT4_DIR}/${LMBENCH_TEST_FILE}

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
