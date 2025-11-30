#!/bin/bash
# Test: ramfs_copy_files_bw
# Binary: lmdd
# Description: Copy files on ramfs bandwidth test

set -e



echo "=== Running ramfs_copy_files_bw test ==="
${LMBENCH_BIN_DIR}/lmdd if=${LMBENCH_TMP_DIR}/${LMBENCH_ZERO_FILE} of=${LMBENCH_TMP_DIR}/${LMBENCH_TEST_FILE}

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
