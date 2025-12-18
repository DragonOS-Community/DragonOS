#!/bin/bash
# Test: udp_loopback_lat
# Binary: lat_udp
# Description: UDP loopback latency test

set -e

# 加载环境变量
SCTIPDIR=$(cd $(dirname ${BASH_SOURCE[0]}) > /dev/null && pwd)
ENV_PATH="${SCTIPDIR}/../env.sh"
source ${ENV_PATH}

SERVER_PID=""

cleanup() {
    if [ ! -z "$SERVER_PID" ]; then
        kill $SERVER_PID 2>/dev/null || true
        wait $SERVER_PID 2>/dev/null || true
    fi
}

trap cleanup EXIT INT TERM

echo "=== Starting UDP server ==="
${LMBENCH_BIN_DIR}/lat_udp -s 127.0.0.1 &
SERVER_PID=$!
sleep 2

echo "=== Running UDP latency test ==="
${LMBENCH_BIN_DIR}/lat_udp -P 1 127.0.0.1

echo "=== Shutting down server ==="
${LMBENCH_BIN_DIR}/lat_udp -S 127.0.0.1

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
