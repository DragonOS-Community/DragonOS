#!/bin/busybox sh

set -u

SCRIPT_DIR="$(cd -- "$(dirname "$0")" && pwd)"
if [ -x "$SCRIPT_DIR/dunitest-runner" ] && [ -d "$SCRIPT_DIR/bin" ]; then
    BASE_DIR="$SCRIPT_DIR"
else
    BASE_DIR="$(cd -- "$SCRIPT_DIR/.." && pwd)"
fi
RUNNER="$BASE_DIR/dunitest-runner"
BIN_DIR="$BASE_DIR/bin"
RESULTS="$BASE_DIR/results"

echo "[dunit] start running tests..."
"$RUNNER" --bin-dir "$BIN_DIR" --results-dir "$RESULTS"
status=$?
echo "[dunit] 测试完成, status=$status"
exit $status
