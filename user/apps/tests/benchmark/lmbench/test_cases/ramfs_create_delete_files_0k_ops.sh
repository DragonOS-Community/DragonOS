#!/bin/bash
# Test: ramfs_create_delete_files_0k_ops
# Binary: lat_fs
# Description: Create and delete 0k files on ramfs

set -e



echo "=== Running ramfs_create_delete_files_0k_ops test ==="
${LMBENCH_BIN_DIR}/lat_fs -s 0k -P 1 -W 30 -N 200

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
