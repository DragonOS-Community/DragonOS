#!/bin/sh
# Test: vfs_read_pagecache_bw
# Binary: bw_file_rd
# Description: VFS read page cache bandwidth test

set -e

# 加载环境变量
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

echo "=== Running VFS read page cache bandwidth test ==="
# size must be <= test file size (64MB fixture recreated by init.sh);
# bw_file_rd validates against the file and fails with perror("x") otherwise.
${LMBENCH_BIN_DIR}/bw_file_rd -P 1 -W 30 -N 300 64m io_only ${LMBENCH_EXT4_DIR}/${LMBENCH_TEST_FILE}

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
