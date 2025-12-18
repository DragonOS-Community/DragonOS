#!/bin/bash
# Test: tcp_loopback_http_bw
# Binary: lmhttp, lat_http
# Description: TCP loopback HTTP bandwidth test

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

echo "=== Starting HTTP server ==="
${LMBENCH_BIN_DIR}/lmhttp &
SERVER_PID=$!
sleep 2

echo "=== Running HTTP bandwidth test ==="
${LMBENCH_BIN_DIR}/lat_http 127.0.0.1 < file_list

echo "=== Shutting down server ==="
${LMBENCH_BIN_DIR}/lat_http -S 127.0.0.1

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
