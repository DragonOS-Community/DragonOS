#!/bin/sh
# Test: tcp_loopback_http_bw
# Binary: lmhttp, lat_http
# Description: TCP loopback HTTP bandwidth test

set -e

# 加载环境变量
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

SERVER_PID=""
WEB_DIR="${LMBENCH_TMP_DIR:-/tmp}/lmbench_http"
PORT=8080

cleanup() {
    ${LMBENCH_BIN_DIR}/lat_http -S 127.0.0.1 $PORT 2>/dev/null || true
    if [ ! -z "$SERVER_PID" ]; then
        kill $SERVER_PID 2>/dev/null || true
        wait $SERVER_PID 2>/dev/null || true
    fi
}

trap cleanup EXIT INT TERM

# Rebuild a small web root. lmhttp serves files relative to its CWD (or
# $DOCROOT); lat_http reads one relative file name per line from stdin.
rm -rf "$WEB_DIR"
mkdir -p "$WEB_DIR"
dd if=/dev/zero of="$WEB_DIR/file1k"  bs=1024 count=1  2>/dev/null
dd if=/dev/zero of="$WEB_DIR/file4k"  bs=1024 count=4  2>/dev/null
dd if=/dev/zero of="$WEB_DIR/file16k" bs=1024 count=16 2>/dev/null
printf 'file1k\nfile4k\nfile16k\n' > "$WEB_DIR/file_list"

echo "=== Starting HTTP server ==="
DOCROOT="$WEB_DIR" ${LMBENCH_BIN_DIR}/lmhttp $PORT &
SERVER_PID=$!
sleep 2

echo "=== Running HTTP bandwidth test ==="
${LMBENCH_BIN_DIR}/lat_http 127.0.0.1 $PORT < "$WEB_DIR/file_list"

echo "=== Shutting down server ==="
${LMBENCH_BIN_DIR}/lat_http -S 127.0.0.1 $PORT

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
