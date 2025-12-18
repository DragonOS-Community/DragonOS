#!/bin/bash
# Test: mem_copy_bw
# Binary: bw_mem
# Description: Memory copy bandwidth test

set -e

# 加载环境变量
SCTIPDIR=$(cd $(dirname ${BASH_SOURCE[0]}) > /dev/null && pwd)
ENV_PATH="${SCTIPDIR}/../env.sh"
source ${ENV_PATH}

echo "=== Running mem_copy_bw test ==="
${LMBENCH_BIN_DIR}/bw_mem -P 1 -N 50 512m fcp

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
