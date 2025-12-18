#!/bin/bash
# Test: vfs_select_lat
# Binary: lat_select
# Description: VFS select latency test

set -e

# 加载环境变量
SCTIPDIR=$(cd $(dirname ${BASH_SOURCE[0]}) > /dev/null && pwd)
ENV_PATH="${SCTIPDIR}/../env.sh"
source ${ENV_PATH}

echo "=== Running VFS select latency test ==="
${LMBENCH_BIN_DIR}/lat_select -P 1 file

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
