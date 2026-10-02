#!/bin/sh
# Linux entry-point functionality only: benchmark executables below are stubs.
# This verifies CLI/path mapping and exit status, not performance or lmbench.
set -eu
SUITE=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
TEST_SHELL=${TEST_SHELL:-/bin/sh}
TEST_ROOT=$(mktemp -d /tmp/lmbench-linux-entry.XXXXXX)
trap 'rm -rf "$TEST_ROOT"' 0
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
unset ENOUGH TIMING_O LOOP_O
# Running the selected binary package must never download or compile sources.
export LMBENCH_SOURCE_ARCHIVE=/nonexistent/lmbench-source-must-not-be-needed
mkdir -p "$TEST_ROOT/bin" "$TEST_ROOT/inputs with spaces"
cat > "$TEST_ROOT/bin/lat_syscall" <<'STUB'
#!/bin/sh
printf 'called\n' >> "$LMBENCH_RUN_TMP/stub-calls"
printf 'ENOUGH=%s\nTIMING_O=%s\nLOOP_O=%s\n' "$ENOUGH" "$TIMING_O" "$LOOP_O" \
    > "$LMBENCH_RUN_TMP/stub-calibration"
printf 'Simple syscall: 1.2500 microseconds\n'
STUB
cat > "$TEST_ROOT/bin/enough" <<'STUB'
#!/bin/sh
[ "${CALIBRATION_TIMEOUT_ARGS:-}" = '-k 2s 60s' ] || exit 80
printf 'enough\n' >> "$CALIBRATION_CALLS"
printf '1000000\n'
STUB
cat > "$TEST_ROOT/bin/timing_o" <<'STUB'
#!/bin/sh
[ "${ENOUGH:-}" = 1000000 ] || exit 81
printf 'timing_o\n' >> "$CALIBRATION_CALLS"
printf '0\n'
STUB
cat > "$TEST_ROOT/bin/loop_o" <<'STUB'
#!/bin/sh
[ "${ENOUGH:-}" = 1000000 ] || exit 82
[ "${TIMING_O:-}" = 0 ] || exit 83
printf 'loop_o\n' >> "$CALIBRATION_CALLS"
printf '0.00000000\n'
STUB
cat > "$TEST_ROOT/timeout-stub" <<'STUB'
#!/bin/sh
CALIBRATION_TIMEOUT_ARGS="$1 $2 $3"
export CALIBRATION_TIMEOUT_ARGS
exec /usr/bin/timeout "$@"
STUB
printf '#!/bin/sh\nexit 0\n' > "$TEST_ROOT/bin/hello"
printf '#!/bin/sh\nexit 0\n' > "$TEST_ROOT/bin/lat_fifo"
cat > "$TEST_ROOT/bin/lat_fs" <<'STUB'
#!/bin/sh
set -eu
for last do :; done
[ "$TMPDIR" = "$LMBENCH_EXT4_DIR" ] && [ "$last" = "$LMBENCH_EXT4_DIR" ] || exit 9
printf 'called\n' >> "$LMBENCH_RUN_TMP/fs-stub-calls"
printf '0k\t1\t2\t3\n'
STUB
chmod +x "$TEST_ROOT/bin/lat_fs"
chmod +x "$TEST_ROOT/bin/lat_syscall" "$TEST_ROOT/bin/hello" "$TEST_ROOT/bin/lat_fifo" \
    "$TEST_ROOT/bin/enough" "$TEST_ROOT/bin/timing_o" "$TEST_ROOT/bin/loop_o" \
    "$TEST_ROOT/timeout-stub"
printf 'SAMPLES=2\nTIMEOUT_SEC=3\nWARMUP=0\n' > "$TEST_ROOT/inputs with spaces/config"
printf 'process_getppid_lat\n' > "$TEST_ROOT/inputs with spaces/whitelist"
FAILURES=0
fail() { printf 'FAIL: %s\n' "$*" >&2; FAILURES=$((FAILURES + 1)); }

run_entry() {
    ENTRY_NAME=$1; shift
    ENTRY_OUT="$TEST_ROOT/$ENTRY_NAME"
    ENTRY_RC=0
    if [ "${ENTRY_EXPLICIT_CALIBRATION:-0}" -eq 1 ]; then
        ENOUGH=$ENTRY_ENOUGH TIMING_O=$ENTRY_TIMING_O LOOP_O=$ENTRY_LOOP_O \
            CALIBRATION_CALLS="$TEST_ROOT/calibration-calls" \
            LMBENCH_BIN_DIR="$TEST_ROOT/bin" \
            LMBENCH_TIMEOUT="$TEST_ROOT/timeout-stub" \
            LMBENCH_SH="$TEST_SHELL" LMBENCH_LINUX_OUTPUT="$ENTRY_OUT" \
            LMBENCH_SUITE_TIMEOUT=30 \
            "$TEST_SHELL" "$SUITE/run_linux.sh" "$@" > "$TEST_ROOT/$ENTRY_NAME.log" 2>&1 || ENTRY_RC=$?
    else
        CALIBRATION_CALLS="$TEST_ROOT/calibration-calls" \
            LMBENCH_BIN_DIR="$TEST_ROOT/bin" \
            LMBENCH_TIMEOUT="$TEST_ROOT/timeout-stub" \
            LMBENCH_SH="$TEST_SHELL" LMBENCH_LINUX_OUTPUT="$ENTRY_OUT" \
            LMBENCH_SUITE_TIMEOUT=30 \
            "$TEST_SHELL" "$SUITE/run_linux.sh" "$@" > "$TEST_ROOT/$ENTRY_NAME.log" 2>&1 || ENTRY_RC=$?
    fi
}

assert_configured_run() {
    if [ "$ENTRY_RC" -ne 0 ]; then
        fail "$ENTRY_NAME: expected successful mapped run, got exit $ENTRY_RC"
        return
    fi
    grep -q '"total":1,"ok":1,"failed":0,"skipped":0' "$ENTRY_OUT/results.jsonl" || \
        fail "$ENTRY_NAME: selected whitelist did not produce exactly one successful case"
    grep -q '"samples":2,"timeout_sec":3,"warmup":0' "$ENTRY_OUT/results.jsonl" || \
        fail "$ENTRY_NAME: host config was not applied"
    if [ ! -f "$ENTRY_OUT/raw/stub-calls" ]; then
        fail "$ENTRY_NAME: benchmark stub was not invoked"
    elif [ "$(wc -l < "$ENTRY_OUT/raw/stub-calls")" -ne 2 ]; then
        fail "$ENTRY_NAME: expected exactly two samples from host config"
    fi
    [ ! -d "$ENTRY_OUT/fixtures" ] || fail "$ENTRY_NAME: fixture directory was not cleaned"
}

assert_no_benchmark() {
    [ "$ENTRY_RC" -eq 0 ] || fail "$ENTRY_NAME: informational invocation exited $ENTRY_RC"
    [ ! -f "$ENTRY_OUT/raw/stub-calls" ] || fail "$ENTRY_NAME: started a benchmark"
    for log_file in "$TEST_ROOT/$ENTRY_NAME.log" "$ENTRY_OUT/runner.log"; do
        if [ -f "$log_file" ] && grep -q '^===LMBENCH_RUN_BEGIN===' "$log_file"; then
            fail "$ENTRY_NAME: initialized the benchmark runner"
        fi
    done
}

# Both absolute files live below the host /tmp, which the sandbox replaces.
run_entry absolute --config "$TEST_ROOT/inputs with spaces/config" \
    --whitelist "$TEST_ROOT/inputs with spaces/whitelist"
assert_configured_run
grep -q '^ENOUGH=1000000$' "$ENTRY_OUT/raw/stub-calibration" || fail 'calibrated ENOUGH was not injected'
grep -q '^TIMING_O=0$' "$ENTRY_OUT/raw/stub-calibration" || fail 'calibrated TIMING_O=0 was not injected'
grep -q '^LOOP_O=0.00000000$' "$ENTRY_OUT/raw/stub-calibration" || fail 'calibrated LOOP_O=0.00000000 was not injected'
[ "$(grep -c '^enough$' "$TEST_ROOT/calibration-calls")" -eq 1 ] || fail 'enough was calibrated more than once'
[ "$(grep -c '^timing_o$' "$TEST_ROOT/calibration-calls")" -eq 1 ] || fail 'timing_o was calibrated more than once'
[ "$(grep -c '^loop_o$' "$TEST_ROOT/calibration-calls")" -eq 1 ] || fail 'loop_o was calibrated more than once'
for expected in \
    'ENOUGH=1000000' 'ENOUGH_SOURCE=calibrated' \
    'TIMING_O=0' 'TIMING_O_SOURCE=calibrated' \
    'LOOP_O=0.00000000' 'LOOP_O_SOURCE=calibrated'; do
    grep -q "^$expected$" "$ENTRY_OUT/environment.txt" || fail "missing provenance: $expected"
done

# A binary under host /tmp must remain available after the namespace hides /tmp.
run_entry filesystem --only ext4_create_delete_files_0k_ops --samples 1 --timeout 3
[ "$ENTRY_RC" -eq 0 ] || fail 'filesystem binary mapping failed'
[ -f "$ENTRY_OUT/raw/fs-stub-calls" ] || fail 'selected filesystem binary was not invoked'
grep -q '^filesystem_binary=' "$ENTRY_OUT/environment.txt" || fail 'filesystem provenance missing'
fs_hash=$(sha256sum "$TEST_ROOT/bin/lat_fs" | awk '{print $1}')
grep -q "^$fs_hash  $TEST_ROOT/bin/lat_fs\$" "$ENTRY_OUT/inputs.sha256" || \
    fail 'selected filesystem binary hash missing'

# Explicit valid values, including zero overheads, are preserved and bypass tools.
: > "$TEST_ROOT/calibration-calls"
ENTRY_EXPLICIT_CALIBRATION=1 ENTRY_ENOUGH=765432 ENTRY_TIMING_O=0 ENTRY_LOOP_O=0.00000000
export ENTRY_EXPLICIT_CALIBRATION ENTRY_ENOUGH ENTRY_TIMING_O ENTRY_LOOP_O
run_entry explicit --only process_getppid_lat --samples 1
[ "$ENTRY_RC" -eq 0 ] || fail 'explicit calibration run failed'
[ ! -s "$TEST_ROOT/calibration-calls" ] || fail 'explicit values unexpectedly ran calibration tools'
grep -q '^ENOUGH=765432$' "$ENTRY_OUT/raw/stub-calibration" || fail 'explicit ENOUGH was not injected'
grep -q '^TIMING_O=0$' "$ENTRY_OUT/raw/stub-calibration" || fail 'explicit TIMING_O=0 was not injected'
grep -q '^LOOP_O=0.00000000$' "$ENTRY_OUT/raw/stub-calibration" || fail 'explicit LOOP_O=0 was not injected'
grep -q '^ENOUGH_SOURCE=explicit$' "$ENTRY_OUT/environment.txt" || fail 'explicit ENOUGH source missing'
grep -q '^TIMING_O_SOURCE=explicit$' "$ENTRY_OUT/environment.txt" || fail 'explicit TIMING_O source missing'
grep -q '^LOOP_O_SOURCE=explicit$' "$ENTRY_OUT/environment.txt" || fail 'explicit LOOP_O source missing'
unset ENTRY_EXPLICIT_CALIBRATION ENTRY_ENOUGH ENTRY_TIMING_O ENTRY_LOOP_O

# Calibration failures are fatal and never claim calibrated provenance.
cat > "$TEST_ROOT/bin/timing_o" <<'STUB'
#!/bin/sh
exit 7
STUB
run_entry calibration_failed --only process_getppid_lat --samples 1
[ "$ENTRY_RC" -ne 0 ] || fail 'failed timing_o calibration was accepted'
grep -q 'Calibration tool failed: timing_o (exit 7)' "$TEST_ROOT/calibration_failed.log" || \
    fail 'timing_o calibration failure was not explained'
[ ! -e "$ENTRY_OUT/environment.txt" ] || fail 'failed calibration wrote provenance as if usable'
cat > "$TEST_ROOT/bin/timing_o" <<'STUB'
#!/bin/sh
[ "${ENOUGH:-}" = 1000000 ] || exit 81
printf 'timing_o\n' >> "$CALIBRATION_CALLS"
printf '0\n'
STUB
chmod +x "$TEST_ROOT/bin/timing_o"

# Every explicit value is validated before any result directory is created.
for invalid_case in enough timing loop; do
    ENTRY_EXPLICIT_CALIBRATION=1 ENTRY_ENOUGH=765432 ENTRY_TIMING_O=0 ENTRY_LOOP_O=0.00000000
    case "$invalid_case" in
        enough) ENTRY_ENOUGH=0 ;;
        timing) ENTRY_TIMING_O=invalid ;;
        loop) ENTRY_LOOP_O=-1 ;;
    esac
    export ENTRY_EXPLICIT_CALIBRATION ENTRY_ENOUGH ENTRY_TIMING_O ENTRY_LOOP_O
    run_entry "invalid_$invalid_case" --only process_getppid_lat --samples 1
    [ "$ENTRY_RC" -ne 0 ] || fail "invalid explicit $invalid_case calibration was accepted"
    [ ! -e "$ENTRY_OUT/environment.txt" ] || fail "invalid explicit $invalid_case wrote provenance"
done
unset ENTRY_EXPLICIT_CALIBRATION ENTRY_ENOUGH ENTRY_TIMING_O ENTRY_LOOP_O

# A missing config must not silently fall back to the suite's default samples.
run_entry config_only --config "$TEST_ROOT/inputs with spaces/config" --only process_getppid_lat
assert_configured_run

# Resolve relative paths against the caller's CWD before entering namespaces.
cd "$TEST_ROOT"
run_entry relative --config 'inputs with spaces/config' --whitelist 'inputs with spaces/whitelist'
assert_configured_run

run_entry help --help
assert_no_benchmark
run_entry list --list
assert_no_benchmark

mkdir "$TEST_ROOT/existing"
printf 'keep this data\n' > "$TEST_ROOT/existing/keep"
run_entry existing --only process_getppid_lat --samples 1
[ "$ENTRY_RC" -ne 0 ] || fail 'existing output directory must be rejected'
[ "$(cat "$TEST_ROOT/existing/keep")" = 'keep this data' ] || fail 'existing output data was changed'
[ "$(ls -A "$TEST_ROOT/existing")" = keep ] || fail 'existing output directory was populated'

# A failed benchmark must propagate failure through runner, bwrap and entry.
printf '#!/bin/sh\nexit 7\n' > "$TEST_ROOT/bin/lat_syscall"
run_entry failed --only process_getppid_lat --samples 1 --timeout 3
[ "$ENTRY_RC" -ne 0 ] || fail 'failed benchmark was reported as a successful Linux run'
if [ -f "$ENTRY_OUT/results.jsonl" ]; then
    grep -q '"total":1,"ok":0,"failed":1,"skipped":0' "$ENTRY_OUT/results.jsonl" || \
        fail 'failed benchmark did not retain its failed summary'
else
    fail 'failed benchmark did not produce results.jsonl'
fi
[ ! -d "$ENTRY_OUT/fixtures" ] || fail 'failed benchmark left its fixture directory'

# Even a printed latency cannot make a hung FIFO controller a success.
cat > "$TEST_ROOT/bin/lat_fifo" <<'STUB'
#!/bin/sh
printf 'Fifo latency: 1.0 microseconds\n'
sleep 60
STUB
run_entry fifo_timeout --only fifo_lat --samples 1 --timeout 1
[ "$ENTRY_RC" -ne 0 ] || fail 'FIFO timeout was accepted as a valid measurement'
grep -q '"total":1,"ok":0,"failed":1,"skipped":0' "$ENTRY_OUT/results.jsonl" || \
    fail 'FIFO timeout did not retain a failed summary'
[ "$(cat "$ENTRY_OUT/raw/fifo_lat.1.rc")" = 124 ] || fail 'FIFO timeout status was lost'
[ "$(find "$ENTRY_OUT" -type p | wc -l)" -eq 0 ] || fail 'FIFO scratch pipe persisted in results'
[ "$(find "$ENTRY_OUT/raw" -type d | wc -l)" -eq 1 ] || fail 'FIFO scratch directory persisted in results'

[ "$FAILURES" -eq 0 ] || exit 1
printf 'Linux entry functionality regression: PASS (%s; no performance measurements)\n' "$TEST_SHELL"
