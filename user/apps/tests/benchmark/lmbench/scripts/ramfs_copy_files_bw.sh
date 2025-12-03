#!/bin/bash
# Test: ramfs_copy_files_bw
# Binary: lmdd
# Description: Copy files on ramfs bandwidth test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running ramfs_copy_files_bw test ==="
${LMBENCH_BIN}/lmdd if=${LMBENCH_TMP_DIR}/${LMBENCH_ZERO_FILE} of=${LMBENCH_TMP_DIR}/${LMBENCH_TEST_FILE}

echo "Test completed successfully"
