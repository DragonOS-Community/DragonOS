#!/bin/bash
# Test: ext2_create_delete_files_0k_ops
# Binary: lat_fs
# Description: Create and delete 0k files on ext2 filesystem

set -e



echo "=== Running ext2_create_delete_files_0k_ops test ==="
${LMBENCH_BIN_DIR}/lat_fs -s 0k -P 1 ${LMBENCH_EXT2_DIR}

echo "Test completed successfully"
