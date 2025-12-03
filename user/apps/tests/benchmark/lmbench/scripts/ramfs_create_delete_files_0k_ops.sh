#!/bin/bash
# Test: ramfs_create_delete_files_0k_ops
# Binary: lat_fs
# Description: Create and delete 0k files on ramfs

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running ramfs_create_delete_files_0k_ops test ==="
${LMBENCH_BIN}/lat_fs -s 0k -P 1 -W 30 -N 200

echo "Test completed successfully"
