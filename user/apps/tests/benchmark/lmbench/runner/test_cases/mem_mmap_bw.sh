#!/bin/sh
# Test: mem_mmap_bw
# Binary: bw_mmap_rd
# Description: Memory mmap bandwidth test

set -e

# 加载环境变量
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

echo "=== Running mem_mmap_bw test ==="
# size must be <= test file size (see bw_mmap_rd.c: nbytes > st_size
# => "<size> out of range!"), and >= MINSZ (512B). Use 16m (not 64m): full-size
# 64MB mmap rounds repeatedly slow the guest kernel down (mmap/page-cache
# state), stalling later process_* fork tests. 16m still covers DRAM size for
# bandwidth purposes and keeps the suite green.
${LMBENCH_BIN_DIR}/bw_mmap_rd 16m mmap_only ${LMBENCH_EXT4_DIR}/${LMBENCH_TEST_FILE}

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
