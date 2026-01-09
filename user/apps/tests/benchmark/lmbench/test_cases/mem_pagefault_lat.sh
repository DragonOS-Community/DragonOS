#!/bin/bash
# Test: mem_pagefault_lat
# Binary: lat_pagefault
# Description: Memory page fault latency test

set -e

# 加载环境变量
SCTIPDIR=$(cd $(dirname ${BASH_SOURCE[0]}) > /dev/null && pwd)
ENV_PATH="${SCTIPDIR}/../env.sh"
source ${ENV_PATH}

echo "=== Running mem_pagefault_lat test ==="
${LMBENCH_BIN_DIR}/lat_pagefault -P 1 ${LMBENCH_EXT4_DIR}/${LMBENCH_TEST_FILE}

if [ $? -eq 0 ]; then
    echo "Test completed successfully"
else
    echo "Test failed"
    exit 1
fi
