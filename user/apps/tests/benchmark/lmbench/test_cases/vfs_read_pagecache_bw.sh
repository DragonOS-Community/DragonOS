#!/bin/bash
# Test: vfs_read_pagecache_bw
# Binary: bw_file_rd
# Description: VFS read page cache bandwidth test

set -e



echo "=== Running VFS read page cache bandwidth test ==="
${LMBENCH_BIN_DIR}/bw_file_rd -P 1 -W 30 -N 300 512m io_only ${LMBENCH_EXT4_DIR}/${LMBENCH_TEST_FILE}

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
