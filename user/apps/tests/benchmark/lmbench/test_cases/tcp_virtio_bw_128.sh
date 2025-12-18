#!/bin/bash
# Test: tcp_virtio_bw_128
# Binary: bw_tcp
# Description: TCP virtio bandwidth test with 128 byte messages

set -e

# 加载环境变量
SCTIPDIR=$(cd $(dirname ${BASH_SOURCE[0]}) > /dev/null && pwd)
ENV_PATH="${SCTIPDIR}/../env.sh"
source ${ENV_PATH}

echo "=== Running TCP virtio bandwidth test (128 bytes) ==="
echo "Note: This test requires a server running at 10.0.2.15"
${LMBENCH_BIN_DIR}/bw_tcp -s 10.0.2.15 -b 1

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
