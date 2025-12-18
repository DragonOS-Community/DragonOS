#!/bin/bash
# Test: process_getppid_lat
# Binary: lat_syscall
# Description: Process getppid syscall latency test

set -e

# 加载环境变量
SCTIPDIR=$(cd $(dirname ${BASH_SOURCE[0]}) > /dev/null && pwd)
ENV_PATH="${SCTIPDIR}/../env.sh"
source ${ENV_PATH}

echo "=== Running process_getppid_lat test ==="
${LMBENCH_BIN_DIR}/lat_syscall -P 1 null

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
