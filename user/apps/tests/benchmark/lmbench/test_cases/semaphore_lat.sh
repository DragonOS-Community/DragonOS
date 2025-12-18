#!/bin/bash
# Test: semaphore_lat
# Binary: lat_sem
# Description: Semaphore latency test

set -e

# 加载环境变量
SCTIPDIR=$(cd $(dirname ${BASH_SOURCE[0]}) > /dev/null && pwd)
ENV_PATH="${SCTIPDIR}/../env.sh"
source ${ENV_PATH}

echo "=== Running semaphore_lat test ==="
${LMBENCH_BIN_DIR}/lat_sem -P 1 -N 21

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
