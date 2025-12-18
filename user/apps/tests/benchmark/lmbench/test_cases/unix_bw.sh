#!/bin/bash
# Test: unix_bw
# Binary: bw_unix
# Description: Unix domain socket bandwidth test

set -e

# 加载环境变量
SCTIPDIR=$(cd $(dirname ${BASH_SOURCE[0]}) > /dev/null && pwd)
ENV_PATH="${SCTIPDIR}/../env.sh"
source ${ENV_PATH}

echo "=== Running Unix domain socket bandwidth test ==="
${LMBENCH_BIN_DIR}/bw_unix -P 1

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
