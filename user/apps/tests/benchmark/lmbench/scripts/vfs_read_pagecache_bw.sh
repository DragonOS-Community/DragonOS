#!/bin/bash
# Test: vfs_read_pagecache_bw
# Binary: bw_file_rd
# Description: VFS read page cache bandwidth test

set -e

if [ -z "$LMBENCH_BIN" ]; then
    echo "Error: Please source env.sh first"
    exit 1
fi

echo "=== Running VFS read page cache bandwidth test ==="
${LMBENCH_BIN}/bw_file_rd -P 1 -W 30 -N 300 512m io_only ${LMBENCH_EXT2_DIR}/${LMBENCH_TEST_FILE}

echo "Test completed successfully"
