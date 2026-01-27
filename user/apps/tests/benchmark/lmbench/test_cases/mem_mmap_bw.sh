#!/bin/bash
# Test: mem_mmap_bw
# Binary: bw_mmap_rd
# Description: Memory mmap bandwidth test

set -e

# 加载环境变量
SCTIPDIR=$(cd $(dirname ${BASH_SOURCE[0]}) > /dev/null && pwd)
ENV_PATH="${SCTIPDIR}/../env.sh"
source ${ENV_PATH}

echo "=== Running mem_mmap_bw test ==="
${LMBENCH_BIN_DIR}/bw_mmap_rd -W 30 -N 300 256m mmap_only ${LMBENCH_EXT4_DIR}/${LMBENCH_TEST_FILE}

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
