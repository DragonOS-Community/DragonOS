#!/bin/bash
# Test: ext2_create_delete_files_0k_ops
# Binary: lat_fs
# Description: Create and delete 0k files on ext2 filesystem

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running ext2_create_delete_files_0k_ops test ==="
${LMBENCH_BIN}/lat_fs -s 0k -P 1 ${LMBENCH_EXT2_DIR}

echo "Test completed successfully"
