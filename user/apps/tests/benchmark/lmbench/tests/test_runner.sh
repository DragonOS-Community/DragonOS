#!/bin/sh
# Exercise the actual runner with tiny benchmarks, without mounts or lmbench.
set -eu
SUITE=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
TEST_SHELL=${TEST_SHELL:-/bin/sh}
TEST_ROOT=$(mktemp -d)
trap 'rm -rf "$TEST_ROOT"' 0
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir -p "$TEST_ROOT/suite/test_cases" "$TEST_ROOT/work"
cp "$SUITE/runner/run.sh" "$SUITE/runner/env.sh" "$SUITE/runner/result.sh" "$TEST_ROOT/suite/"
export LMBENCH_RUN_TMP="$TEST_ROOT/work" ENOUGH=50000 LC_ALL=C
export LMBENCH_SH="$TEST_SHELL"
printf 'SAMPLES=2\nTIMEOUT_SEC=2\nWARMUP=0' > "$TEST_ROOT/suite/config"
printf 'first\nlast' > "$TEST_ROOT/suite/whitelist.txt"
for name in first last; do
    printf '#!/bin/sh\nprintf "value 12.5\\n"\n' > "$TEST_ROOT/suite/test_cases/$name.sh"
    printf 'SEARCH_PATTERN=^value\nRESULT_INDEX=2\nUNIT=us' > "$TEST_ROOT/suite/test_cases/$name.meta"
done
if ! "$TEST_SHELL" "$TEST_ROOT/suite/run.sh" > "$TEST_ROOT/run.log" 2>&1; then
    cat "$TEST_ROOT/run.log"
    exit 1
fi
grep -q '"total":2,"ok":2,"failed":0,"skipped":0' "$TEST_ROOT/run.log" || {
    cat "$TEST_ROOT/run.log"; echo 'FAIL: every whitelist entry must run'; exit 1;
}
grep -q '"unit":"us"' "$TEST_ROOT/run.log"
[ -f "$LMBENCH_RUN_TMP/first.1.out" ]
[ -f "$LMBENCH_RUN_TMP/first.2.out" ]

printf '#!/bin/sh\nprintf "timeout: warning: timer_create: Invalid argument\\nvalue 12.5\\n"\n' > "$TEST_ROOT/suite/test_cases/first.sh"
"$TEST_SHELL" "$TEST_ROOT/suite/run.sh" --only first --samples 1 > "$TEST_ROOT/timer-warning.log" 2>&1
grep -q '"status":"ok"' "$TEST_ROOT/timer-warning.log" || {
    cat "$TEST_ROOT/timer-warning.log"; echo 'FAIL: nonfatal timer warning rejected'; exit 1;
}

# A sample is successful only when its output contains one finite, positive
# metric and no fatal benchmark diagnostic, even if the wrapper exits zero.
assert_runner_rejects() {
    fixture=$1
    printf '#!/bin/sh\n%s\n' "$fixture" > "$TEST_ROOT/suite/test_cases/first.sh"
    if "$TEST_SHELL" "$TEST_ROOT/suite/run.sh" --only first --samples 1 > "$TEST_ROOT/rejected.log" 2>&1; then
        cat "$TEST_ROOT/rejected.log"
        echo "FAIL: runner accepted invalid output: $fixture"
        exit 1
    fi
    grep -q '"status":"failed"' "$TEST_ROOT/rejected.log"
    grep -q '"total":1,"ok":0,"failed":1,"skipped":0' "$TEST_ROOT/rejected.log"
}
assert_runner_rejects 'printf "assertion failed: clock went backwards\\nvalue 12.5\\n"'
assert_runner_rejects 'printf "error: benchmark child failed\\nvalue 12.5\\n"'
assert_runner_rejects 'printf "(r) error on semaphore: Invalid argument\\nvalue 12.5\\n"'
assert_runner_rejects 'printf "lat_udp client: send failed: Permission denied\\nvalue 12.5\\n"'
assert_runner_rejects 'printf "lat_udp client: recv failed: Invalid argument\\nvalue 12.5\\n"'
assert_runner_rejects 'printf "lmbench: internal timeout\\nvalue 12.5\\n"'
assert_runner_rejects 'printf "Function not implemented\\nvalue 12.5\\n"'
assert_runner_rejects 'printf "value 0\\n"'
assert_runner_rejects 'printf "value nan\\n"'
assert_runner_rejects 'printf "value 1e999\\n"'
assert_runner_rejects 'printf "benchmark produced no metric\\n"'

cat > "$TEST_ROOT/suite/test_cases/first.sh" <<'CASE'
#!/bin/sh
if [ -f "$LMBENCH_RUN_TMP/attempted" ]; then exit 1; fi
: > "$LMBENCH_RUN_TMP/attempted"
printf 'value 12.5\n'
CASE
if "$TEST_SHELL" "$TEST_ROOT/suite/run.sh" --only first > "$TEST_ROOT/partial.log" 2>&1; then
    cat "$TEST_ROOT/partial.log"; echo 'FAIL: partial samples must fail the run'; exit 1
fi
grep -q '"status":"failed"' "$TEST_ROOT/partial.log"
# Caller-selected binaries and fixtures must survive sourcing env.sh.
LMBENCH_BIN_DIR=/chosen/bin LMBENCH_TMP_DIR="$TEST_ROOT" "$TEST_SHELL" -c '
    . "$1"
    [ "$LMBENCH_BIN_DIR" = /chosen/bin ] && [ "$LMBENCH_TMP_DIR" = "$2" ]
' sh "$SUITE/runner/env.sh" "$TEST_ROOT"
# Help/invalid arguments must finish before initialization.
"$TEST_SHELL" "$TEST_ROOT/suite/run.sh" --help > "$TEST_ROOT/help.log" 2>&1
! grep -q LMBENCH_RUN_BEGIN "$TEST_ROOT/help.log"
if "$TEST_SHELL" "$TEST_ROOT/suite/run.sh" --samples 0 > /dev/null 2>&1; then
    echo 'FAIL: zero samples accepted'; exit 1
fi
for count in 00 01 08; do
    if "$TEST_SHELL" "$TEST_ROOT/suite/run.sh" --samples "$count" > /dev/null 2>&1; then
        echo "FAIL: noncanonical sample count accepted: $count"; exit 1
    fi
done
printf '\nSAMPLES=0\n' >> "$TEST_ROOT/suite/test_cases/first.meta"
if "$TEST_SHELL" "$TEST_ROOT/suite/run.sh" --only first > /dev/null 2>&1; then
    echo 'FAIL: metadata accepted zero samples'; exit 1
fi
printf '#!/bin/sh\nsleep 5\n' > "$TEST_ROOT/suite/test_cases/last.sh"
if "$TEST_SHELL" "$TEST_ROOT/suite/run.sh" --only last --samples 1 --timeout 1 > "$TEST_ROOT/timeout.log" 2>&1; then
    echo 'FAIL: timed-out sample accepted'; exit 1
fi
grep -q 'timeout' "$TEST_ROOT/timeout.log"
[ "$(cat "$LMBENCH_RUN_TMP/last.1.rc")" -eq 124 ]
if "$TEST_SHELL" "$TEST_ROOT/suite/run.sh" --config "$TEST_ROOT/missing" > /dev/null 2>&1; then
    echo 'FAIL: missing explicit config silently ignored'; exit 1
fi
# Completion must be emitted after cleanup, so the guest monitor may stop QEMU.
cat > "$TEST_ROOT/suite/clean_up.sh" <<'CLEANUP'
#!/bin/sh
echo CLEANUP_COMPLETE
CLEANUP
printf '#!/bin/sh\nprintf "value 12.5\\n"\n' > "$TEST_ROOT/suite/test_cases/last.sh"
unset ENOUGH
"$TEST_SHELL" "$TEST_ROOT/suite/run.sh" --only last --samples 1 > "$TEST_ROOT/order.log" 2>&1
awk '/CLEANUP_COMPLETE/ { cleaned=1 } /benchmark测试完成/ { if (!cleaned) exit 1 }' "$TEST_ROOT/order.log" || {
    cat "$TEST_ROOT/order.log"; echo 'FAIL: completion emitted before cleanup'; exit 1;
}
# Re-sourcing env.sh during default ENOUGH calibration must not replace the
# runner's EXIT cleanup trap.
cat > "$TEST_ROOT/suite/clean_up.sh" <<'CLEANUP'
#!/bin/sh
: > "$LMBENCH_RUN_TMP/resourced-env-cleaned"
CLEANUP
trap_rc=0
"$TEST_SHELL" -c '
    LMBENCH_RUNNER_NO_MAIN=1
    export LMBENCH_RUNNER_NO_MAIN
    . "$1/run.sh"
    SCRIPT_DIR=$1
    LMBENCH_SH=$2
    LMBENCH_RUN_TMP=$3
    export LMBENCH_RUN_TMP
    trap runner_cleanup 0
    . "$1/env.sh"
    exit 7
' sh "$TEST_ROOT/suite" "$TEST_SHELL" "$LMBENCH_RUN_TMP" || trap_rc=$?
[ "$trap_rc" -eq 7 ]
[ -f "$LMBENCH_RUN_TMP/resourced-env-cleaned" ] || {
    echo 'FAIL: re-sourcing env.sh replaced runner cleanup trap'; exit 1;
}
# Automatic calibration must preserve a valid value returned by lmbench's
# enough tool, including its 1000000 SHORT result.
mkdir -p "$TEST_ROOT/fake-bin"
cat > "$TEST_ROOT/fake-bin/enough" <<'ENOUGH'
#!/bin/sh
printf '1000000\n'
ENOUGH
chmod +x "$TEST_ROOT/fake-bin/enough"
cat > "$TEST_ROOT/suite/test_cases/last.sh" <<'CASE'
#!/bin/sh
printf '%s\n' "$ENOUGH" > "$LMBENCH_RUN_TMP/observed-enough"
printf 'value 12.5\n'
CASE
unset ENOUGH
LMBENCH_BIN_DIR="$TEST_ROOT/fake-bin" \
    "$TEST_SHELL" "$TEST_ROOT/suite/run.sh" --only last --samples 1 \
    > "$TEST_ROOT/enough-valid.log" 2>&1
[ "$(cat "$LMBENCH_RUN_TMP/observed-enough")" = 1000000 ] || {
    cat "$TEST_ROOT/enough-valid.log"
    echo 'FAIL: valid automatic ENOUGH value was not preserved'
    exit 1
}
grep -q 'ENOUGH=1000000 (calibrated by enough tool)' "$TEST_ROOT/enough-valid.log" || {
    cat "$TEST_ROOT/enough-valid.log"
    echo 'FAIL: successful ENOUGH calibration was not identified'
    exit 1
}
# Failed and invalid calibration results must use the bounded fallback and say
# why. These cases also guard against validating ENOUGH with shell arithmetic.
assert_enough_fallback() {
    label=$1
    reason=$2
    rm -f "$LMBENCH_RUN_TMP/observed-enough"
    unset ENOUGH
    LMBENCH_BIN_DIR="$TEST_ROOT/fake-bin" \
        "$TEST_SHELL" "$TEST_ROOT/suite/run.sh" --only last --samples 1 \
        > "$TEST_ROOT/enough-$label.log" 2>&1
    [ "$(cat "$LMBENCH_RUN_TMP/observed-enough")" = 50000 ] || {
        cat "$TEST_ROOT/enough-$label.log"
        echo "FAIL: $label calibration did not use ENOUGH=50000"
        exit 1
    }
    grep -q "ENOUGH=50000 (fallback: $reason)" "$TEST_ROOT/enough-$label.log" || {
        cat "$TEST_ROOT/enough-$label.log"
        echo "FAIL: $label fallback reason was not identified"
        exit 1
    }
}
cat > "$TEST_ROOT/fake-bin/enough" <<'ENOUGH'
#!/bin/sh
exit 1
ENOUGH
assert_enough_fallback failed 'enough tool failed'
cat > "$TEST_ROOT/fake-bin/enough" <<'ENOUGH'
#!/bin/sh
:
ENOUGH
assert_enough_fallback empty 'enough tool returned empty output'
cat > "$TEST_ROOT/fake-bin/enough" <<'ENOUGH'
#!/bin/sh
printf 'not-a-number\n'
ENOUGH
assert_enough_fallback nonnumeric 'enough tool returned non-decimal output'
cat > "$TEST_ROOT/fake-bin/enough" <<'ENOUGH'
#!/bin/sh
printf '0\n'
ENOUGH
assert_enough_fallback zero 'enough tool returned zero'

# A caller-provided ENOUGH value is authoritative and bypasses calibration.
cat > "$TEST_ROOT/fake-bin/enough" <<'ENOUGH'
#!/bin/sh
: > "$LMBENCH_RUN_TMP/enough-called"
printf '999\n'
ENOUGH
rm -f "$LMBENCH_RUN_TMP/enough-called" "$LMBENCH_RUN_TMP/observed-enough"
ENOUGH=765432 LMBENCH_BIN_DIR="$TEST_ROOT/fake-bin" \
    "$TEST_SHELL" "$TEST_ROOT/suite/run.sh" --only last --samples 1 \
    > "$TEST_ROOT/enough-explicit.log" 2>&1
[ "$(cat "$LMBENCH_RUN_TMP/observed-enough")" = 765432 ] || {
    cat "$TEST_ROOT/enough-explicit.log"
    echo 'FAIL: explicit ENOUGH value was not preserved'
    exit 1
}
[ ! -e "$LMBENCH_RUN_TMP/enough-called" ] || {
    cat "$TEST_ROOT/enough-explicit.log"
    echo 'FAIL: explicit ENOUGH unexpectedly ran calibration'
    exit 1
}
# Hangup must run cleanup and retain its conventional signal exit status.
cat > "$TEST_ROOT/suite/init.sh" <<'INIT'
#!/bin/sh
kill -HUP "$PPID"
INIT
cat > "$TEST_ROOT/suite/clean_up.sh" <<'CLEANUP'
#!/bin/sh
: > "$LMBENCH_RUN_TMP/hup-cleaned"
CLEANUP
signal_rc=0
"$TEST_SHELL" "$TEST_ROOT/suite/run.sh" --only last > "$TEST_ROOT/hup.log" 2>&1 || signal_rc=$?
[ "$signal_rc" -eq 129 ]
[ -f "$LMBENCH_RUN_TMP/hup-cleaned" ]
# DragonOS may provide BusyBox without standalone awk/sort applet links.
"$TEST_SHELL" -c '
    LMBENCH_RUNNER_NO_MAIN=1
    . "$1"
    PATH=/no-applet-links
    stats=$(compute_stats "1 2 3")
    case "$stats" in "3 2.000000 2.000000 "*) ;; *) exit 1 ;; esac
' sh "$SUITE/runner/run.sh"

# Published working-set descriptions must match the wrapper arguments.
grep -q ' 16m mmap_only ' "$SUITE/runner/test_cases/mem_mmap_bw.sh"
grep -q 'DESCRIPTION=.*(16MB)' "$SUITE/runner/test_cases/mem_mmap_bw.meta"
grep -q ' 64m io_only ' "$SUITE/runner/test_cases/vfs_read_pagecache_bw.sh"
grep -q 'DESCRIPTION=.*(64MB)' "$SUITE/runner/test_cases/vfs_read_pagecache_bw.meta"

# The pagefault wrapper owns a unique per-invocation fixture and removes it,
# while preserving an unrelated legacy path that may belong to the user.
PAGEFAULT_TMP="$TEST_ROOT/pagefault-tmp"
PAGEFAULT_RUN="$TEST_ROOT/pagefault-run"
PAGEFAULT_BIN="$TEST_ROOT/pagefault-bin"
mkdir -p "$PAGEFAULT_TMP" "$PAGEFAULT_RUN" "$PAGEFAULT_BIN"
printf 'keep me\n' > "$PAGEFAULT_TMP/pagefault_file"
cat > "$PAGEFAULT_BIN/lat_pagefault" <<'PAGEFAULT'
#!/bin/sh
for last do :; done
[ -f "$last" ] && [ "$(wc -c < "$last")" -eq 8388608 ] || exit 9
printf '%s\n' "$last" > "$LMBENCH_RUN_TMP/pagefault-observed-path"
printf 'Pagefaults on %s: 1.25 microseconds\n' "$last"
PAGEFAULT
chmod +x "$PAGEFAULT_BIN/lat_pagefault"
LMBENCH_BIN_DIR="$PAGEFAULT_BIN" LMBENCH_TMP_DIR="$PAGEFAULT_TMP" \
    LMBENCH_RUN_TMP="$PAGEFAULT_RUN" LMBENCH_TIMEOUT=/usr/bin/timeout \
    "$TEST_SHELL" "$SUITE/runner/test_cases/mem_pagefault_lat.sh" \
    > "$TEST_ROOT/pagefault.log" 2>&1 || {
        cat "$TEST_ROOT/pagefault.log"
        echo 'FAIL: pagefault wrapper invocation failed'
        exit 1
    }
[ "$(cat "$PAGEFAULT_TMP/pagefault_file")" = 'keep me' ] || {
    echo 'FAIL: pagefault wrapper overwrote a pre-existing file'; exit 1;
}
observed_pagefault_path=$(cat "$PAGEFAULT_RUN/pagefault-observed-path")
[ ! -e "$observed_pagefault_path" ] || {
    echo 'FAIL: pagefault wrapper left its owned fixture'; exit 1;
}
printf 'runner regression: PASS (%s)\n'  "$TEST_SHELL"
