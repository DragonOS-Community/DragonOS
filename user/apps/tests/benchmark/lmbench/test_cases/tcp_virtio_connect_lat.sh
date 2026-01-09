#!/bin/bash
# Test: tcp_virtio_connect_lat
# Binary: lat_connect
# Description: TCP virtio connection latency test

set -e

# 加载环境变量
SCTIPDIR=$(cd $(dirname ${BASH_SOURCE[0]}) > /dev/null && pwd)
ENV_PATH="${SCTIPDIR}/../env.sh"
source ${ENV_PATH}

echo "=== Running TCP virtio connection latency test ==="
echo "Note: This test requires a server running at 10.0.2.15"
${LMBENCH_BIN_DIR}/lat_connect -s 10.0.2.15 -b 1000

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
