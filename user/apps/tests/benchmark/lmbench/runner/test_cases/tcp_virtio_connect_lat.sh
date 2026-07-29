#!/bin/sh
# Test: tcp_virtio_connect_lat
# Binary: lat_connect
# Description: TCP virtio connection latency test

set -e

# 加载环境变量
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

echo "=== Running TCP virtio connection latency test ==="
echo "Note: This test requires a server running at 10.0.2.15"
${LMBENCH_BIN_DIR}/lat_connect -s 10.0.2.15 -b 1000

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
