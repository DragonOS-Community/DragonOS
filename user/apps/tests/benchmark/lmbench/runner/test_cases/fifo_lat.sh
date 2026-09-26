#!/bin/sh
# Test: fifo_lat
# Binary: lat_fifo
# Description: FIFO latency test

set -e

# 加载环境变量
SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

echo "=== Running fifo_lat test ==="
if [ "${LMBENCH_FIFO_CLEANUP:-0}" = 1 ]; then
    run_lmbench_measurement "${LMBENCH_SH:-sh}" "$SCRIPT_DIR/../fifo_cleanup.sh" "$LMBENCH_BIN_DIR/lat_fifo"
else
    run_lmbench_measurement "$LMBENCH_BIN_DIR/lat_fifo" -P 1
fi

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
