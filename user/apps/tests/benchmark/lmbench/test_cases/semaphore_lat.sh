#!/bin/sh
# Test: semaphore_lat
# Binary: lat_sem
# Description: Semaphore latency test

set -e

# 加载环境变量
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

echo "=== Running semaphore_lat test ==="
${LMBENCH_BIN_DIR}/lat_sem -P 1 -N 21

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
