#!/bin/bash
# Test: mem_pagefault_lat
# Binary: lat_pagefault
# Description: Memory page fault latency test

set -e



echo "=== Running mem_pagefault_lat test ==="
${LMBENCH_BIN_DIR}/lat_pagefault -P 1 ${LMBENCH_EXT2_DIR}/${LMBENCH_TEST_FILE}

echo "Test completed successfully"
