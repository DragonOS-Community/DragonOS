#!/bin/sh
# Test: signal_install_lat
# Binary: lat_sig
# Description: Signal install latency test

set -e

# 加载环境变量
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

echo "=== Running signal_install_lat test ==="
${LMBENCH_BIN_DIR}/lat_sig -P 1 install

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
