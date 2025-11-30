#!/bin/bash
# Test: ext4_create_delete_files_0k_ops
# Binary: lat_fs
# Description: Create and delete 0k files on ext4 filesystem

set -e



echo "=== Running ext4_create_delete_files_0k_ops test ==="
${LMBENCH_BIN_DIR}/lat_fs -s 0k -P 1 ${LMBENCH_EXT4_DIR}

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
