#!/bin/sh
# Test: mem_pagefault_lat
# Binary: lat_pagefault
# Description: Memory page fault latency test

set -e

# 加载环境变量
SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
ENV_PATH="$SCRIPT_DIR/../env.sh"
. "$ENV_PATH"

echo "=== Running mem_pagefault_lat test ==="
# Pagefault the whole passed file; the 64MB fixture would touch every page of
# the full ext4 image-backed file each sample and stall later fork tests.
# Use a small runner-owned file instead (lat_pagefault requires >= 1MB).
# A unique directory avoids overwriting or deleting an existing user file.
PFAULT_PARENT=${LMBENCH_RUN_TMP:-${LMBENCH_TMP_DIR:-/tmp}}
PFAULT_DIR=$PFAULT_PARENT/pagefault.$$
mkdir "$PFAULT_DIR"
PFAULT_FILE=$PFAULT_DIR/input
cleanup_pagefault_fixture() {
    rm -f "$PFAULT_FILE"
    rmdir "$PFAULT_DIR" 2>/dev/null || true
}
trap cleanup_pagefault_fixture 0
dd if=/dev/zero of="$PFAULT_FILE" bs=1M count=8 2>/dev/null
run_lmbench_measurement "${LMBENCH_BIN_DIR}/lat_pagefault" -P 1 "$PFAULT_FILE"

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
