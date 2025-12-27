#!/bin/bash
# Test: pipe_bw
# Binary: bw_pipe
# Description: Pipe bandwidth test

set -e

# 加载环境变量
SCTIPDIR=$(cd $(dirname ${BASH_SOURCE[0]}) > /dev/null && pwd)
ENV_PATH="${SCTIPDIR}/../env.sh"
source ${ENV_PATH}

echo "=== Running pipe_bw test ==="
${LMBENCH_BIN_DIR}/bw_pipe -P 1

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
