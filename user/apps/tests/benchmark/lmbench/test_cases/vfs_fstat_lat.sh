#!/bin/bash
# Test: vfs_fstat_lat
# Binary: lat_syscall
# Description: VFS fstat syscall latency test

set -e

# 加载环境变量
SCTIPDIR=$(cd $(dirname ${BASH_SOURCE[0]}) > /dev/null && pwd)
ENV_PATH="${SCTIPDIR}/../env.sh"
source ${ENV_PATH}

echo "=== Running VFS fstat latency test ==="
touch test_file
${LMBENCH_BIN_DIR}/lat_syscall -P 1 fstat test_file
rm test_file

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
