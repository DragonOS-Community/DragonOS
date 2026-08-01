#!/bin/sh
# Regression test for run.sh argument parsing.

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
RUNNER="$SCRIPT_DIR/../run.sh"

LMBENCH_RUNNER_NO_MAIN=1
export LMBENCH_RUNNER_NO_MAIN
. "$RUNNER"

reset_cli() {
    CLI_SAMPLES=""; CLI_TIMEOUT=""; CLI_WARMUP=""
    CLI_WHITELIST=""; CLI_CONFIG=""; ONLY_NAME=""; LIST_ONLY=""
}

# --samples / --only parsed into CLI_* / ONLY_NAME
reset_cli
parse_args --samples 7 --only foo
[ "$CLI_SAMPLES" = "7" ] || { echo "FAIL: CLI_SAMPLES=$CLI_SAMPLES" >&2; exit 1; }
[ "$ONLY_NAME" = "foo" ] || { echo "FAIL: ONLY_NAME=$ONLY_NAME" >&2; exit 1; }

# --timeout / --warmup / --whitelist / --config / --list
reset_cli
parse_args --timeout 30 --warmup 2 --whitelist /tmp/wl --config /tmp/cfg --list
[ "$CLI_TIMEOUT" = "30" ]        || { echo "FAIL: CLI_TIMEOUT" >&2; exit 1; }
[ "$CLI_WARMUP" = "2" ]          || { echo "FAIL: CLI_WARMUP" >&2; exit 1; }
[ "$CLI_WHITELIST" = "/tmp/wl" ] || { echo "FAIL: CLI_WHITELIST" >&2; exit 1; }
[ "$CLI_CONFIG" = "/tmp/cfg" ]   || { echo "FAIL: CLI_CONFIG" >&2; exit 1; }
[ "$LIST_ONLY" = "1" ]           || { echo "FAIL: LIST_ONLY" >&2; exit 1; }

# unknown arg returns 2 (does not exit the sourced shell)
reset_cli
set +e
parse_args --bogus 2>/dev/null
rc=$?
set -e
[ "$rc" = "2" ] || { echo "FAIL: unknown arg rc=$rc" >&2; exit 1; }

echo "args parser regression: PASS"