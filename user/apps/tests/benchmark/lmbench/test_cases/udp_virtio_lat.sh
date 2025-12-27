#!/bin/bash
# Test: udp_virtio_lat
# Binary: lat_udp
# Description: UDP virtio latency test

set -e

# 加载环境变量
SCTIPDIR=$(cd $(dirname ${BASH_SOURCE[0]}) > /dev/null && pwd)
ENV_PATH="${SCTIPDIR}/../env.sh"
source ${ENV_PATH}

echo "=== Running UDP virtio latency test ==="
echo "Note: This test requires a server running at 10.0.2.15"
${LMBENCH_BIN_DIR}/lat_udp -s 10.0.2.15

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
