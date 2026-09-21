#!/bin/sh
# Run the same POSIX-sh suite in private Linux namespaces, without host mounts.
set -eu
SUITE=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# Informational flags do not need binaries, namespaces, builds or result files.
skip_value=0
for arg do
    if [ "$skip_value" = 1 ]; then skip_value=0; continue; fi
    case "$arg" in
        --samples|--timeout|--warmup|--config|--whitelist|--only) skip_value=1 ;;
        --help|-h|--list) exec sh "$SUITE/runner/run.sh" "$@" ;;
    esac
done
REPO=$(git -C "$SUITE" rev-parse --show-toplevel)
LMBENCH_BIN_DIR=${LMBENCH_BIN_DIR:-$REPO/bin/sysroot/usr/lib/lmbench/bin/x86_64-linux-gnu}
LMBENCH_SH=${LMBENCH_SH:-/bin/sh}
LINUX_TIMEOUT=${LMBENCH_TIMEOUT:-/usr/bin/timeout}
[ -x "$LINUX_TIMEOUT" ] || { echo "Set LMBENCH_TIMEOUT to GNU timeout" >&2; exit 1; }
LMBENCH_LINUX_OUTPUT=${LMBENCH_LINUX_OUTPUT:-$REPO/harness/references/lmbench-linux/$(date -u +%Y%m%dT%H%M%SZ)-sh-validation}
for tool in bwrap timeout findmnt sha256sum ps awk find; do
    command -v "$tool" >/dev/null 2>&1 || { echo "Missing Linux dependency: $tool" >&2; exit 1; }
done
[ -x "$LMBENCH_BIN_DIR/lat_syscall" ] || { echo "Set LMBENCH_BIN_DIR to the lmbench binaries" >&2; exit 1; }
[ -x "$LMBENCH_SH" ] || { echo "Shell is not executable: $LMBENCH_SH" >&2; exit 1; }

is_positive_integer() {
    printf '%s\n' "$1" | awk '
        NR == 1 && /^[0-9]+$/ && $0 !~ /^0+$/ { valid = 1 }
        END { exit !(NR == 1 && valid) }
    '
}

is_nonnegative_decimal() {
    printf '%s\n' "$1" | awk '
        NR == 1 && /^[0-9]+([.][0-9]+)?$/ { valid = 1 }
        END { exit !(NR == 1 && valid) }
    '
}

calibrate() {
    calibration_tool=$1
    [ -x "$LMBENCH_BIN_DIR/$calibration_tool" ] || {
        echo "Calibration tool is not executable: $LMBENCH_BIN_DIR/$calibration_tool" >&2
        return 1
    }
    calibration_value=$(
        "$LINUX_TIMEOUT" -k 2s 60s "$LMBENCH_BIN_DIR/$calibration_tool"
    ) || {
        calibration_rc=$?
        echo "Calibration tool failed: $calibration_tool (exit $calibration_rc)" >&2
        return 1
    }
    printf '%s\n' "$calibration_value"
}

# Determine the complete calibration environment once, before entering the
# clearenv namespace. Explicit values are authoritative but still validated.
if [ "${ENOUGH+x}" = x ]; then
    is_positive_integer "$ENOUGH" || { echo "Invalid explicit ENOUGH: $ENOUGH" >&2; exit 1; }
    ENOUGH_SOURCE=explicit
else
    ENOUGH=$(calibrate enough) || exit 1
    is_positive_integer "$ENOUGH" || { echo "Invalid output from enough: $ENOUGH" >&2; exit 1; }
    ENOUGH_SOURCE=calibrated
fi
export ENOUGH
if [ "${TIMING_O+x}" = x ]; then
    is_nonnegative_decimal "$TIMING_O" || { echo "Invalid explicit TIMING_O: $TIMING_O" >&2; exit 1; }
    TIMING_O_SOURCE=explicit
else
    TIMING_O=$(calibrate timing_o) || exit 1
    is_nonnegative_decimal "$TIMING_O" || { echo "Invalid output from timing_o: $TIMING_O" >&2; exit 1; }
    TIMING_O_SOURCE=calibrated
fi
export TIMING_O
if [ "${LOOP_O+x}" = x ]; then
    is_nonnegative_decimal "$LOOP_O" || { echo "Invalid explicit LOOP_O: $LOOP_O" >&2; exit 1; }
    LOOP_O_SOURCE=explicit
else
    LOOP_O=$(calibrate loop_o) || exit 1
    is_nonnegative_decimal "$LOOP_O" || { echo "Invalid output from loop_o: $LOOP_O" >&2; exit 1; }
    LOOP_O_SOURCE=calibrated
fi
export ENOUGH TIMING_O LOOP_O
# Refuse to overwrite a previous (including partial) run.
mkdir -p "$(dirname -- "$LMBENCH_LINUX_OUTPUT")"
mkdir "$LMBENCH_LINUX_OUTPUT"
OUT=$(CDPATH='' cd -- "$LMBENCH_LINUX_OUTPUT" && pwd)
printf 'Linux lmbench output: %s\n' "$OUT"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/lmbench-linux.XXXXXX")
cleanup() { rm -rf "$OUT/fixtures" "$WORK"; }
trap cleanup 0
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir "$OUT/fixtures" "$OUT/raw" "$WORK/source"
# The ext4-labelled cases must actually use ext4; private /tmp is tmpfs.
fs_type=$(findmnt -n -o FSTYPE -T "$OUT/fixtures")
[ "$fs_type" = ext4 ] || { echo "Output directory must be on ext4 (found $fs_type)" >&2; exit 1; }
printf 'runner arguments:' > "$OUT/arguments.txt"
printf ' <%s>' "$@" >> "$OUT/arguments.txt"
printf '\n' >> "$OUT/arguments.txt"
# Resolve and archive host files before /tmp and the working directory change.
# Rotate only the original arguments, preserving spaces without eval or arrays.
remaining=$#
while [ "$remaining" -gt 0 ]; do
    arg=$1; shift
    remaining=$((remaining - 1))
    case "$arg" in
        --config|--whitelist)
            [ "$remaining" -gt 0 ] || { echo "Missing value for $arg" >&2; exit 2; }
            input_file=$1; shift
            remaining=$((remaining - 1))
            selected=selected-${arg#--}
            cp -- "$input_file" "$OUT/$selected"
            set -- "$@" "$arg" "/tmp/lmbench-output/$selected"
            ;;
        *) set -- "$@" "$arg" ;;
    esac
done
# Execute a snapshot so concurrent checkout edits cannot change a running suite.
cp -R "$SUITE/runner" "$SUITE/config" "$SUITE/whitelist.txt" \
    "$SUITE/run_linux.sh" "$SUITE/README.md" "$WORK/source/"
{
    date -u '+timestamp=%Y-%m-%dT%H:%M:%SZ'
    uname -a
    printf 'shell=%s\nbinary_dir=%s\nfifo_binary=%s\nfilesystem=%s\n' \
        "$LMBENCH_SH" "$LMBENCH_BIN_DIR" "$LMBENCH_BIN_DIR/lat_fifo" "$fs_type"
    printf 'filesystem_binary=%s\n' "$LMBENCH_BIN_DIR/lat_fs"
    printf 'network=private namespace; all clients use local addresses, not a virtio NIC\n'
    printf 'ENOUGH=%s\nENOUGH_SOURCE=%s\n' "$ENOUGH" "$ENOUGH_SOURCE"
    printf 'TIMING_O=%s\nTIMING_O_SOURCE=%s\n' "$TIMING_O" "$TIMING_O_SOURCE"
    printf 'LOOP_O=%s\nLOOP_O_SOURCE=%s\n' "$LOOP_O" "$LOOP_O_SOURCE"
    cat "$OUT/arguments.txt"
    git -C "$REPO" rev-parse HEAD
    getconf GNU_LIBC_VERSION
    findmnt -T "$OUT" -o TARGET,SOURCE,FSTYPE,OPTIONS
} > "$OUT/environment.txt"
# Hash the exact scripts and the selected, unmodified binary package.
{
    sha256sum "$WORK/source"/runner/*.sh "$WORK/source"/runner/test_cases/*.sh \
        "$WORK/source"/runner/test_cases/*.meta "$WORK/source/config" "$WORK/source/whitelist.txt" \
        "$WORK/source/run_linux.sh" "$LMBENCH_SH"
    for selected_file in "$OUT"/selected-*; do
        [ ! -f "$selected_file" ] || sha256sum "$selected_file"
    done
    for binary in "$LMBENCH_BIN_DIR"/*; do
        [ ! -f "$binary" ] || sha256sum "$binary"
    done
} > "$OUT/inputs.sha256"
cp "$WORK/source/whitelist.txt" "$OUT/whitelist.txt"
# Mount inputs below private /tmp; inputs originally under host /tmp also work.
# The PID namespace contains any daemon that outlives a broken wrapper.
rc=0
"$LINUX_TIMEOUT" -k 5s "${LMBENCH_SUITE_TIMEOUT:-7200}s" \
    bwrap --unshare-user --uid 0 --gid 0 --unshare-pid --unshare-net \
    --unshare-ipc --unshare-uts --die-with-parent \
    --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp --tmpfs /var/tmp --tmpfs /run \
    --ro-bind "$WORK/source" /tmp/lmbench-suite --ro-bind "$LMBENCH_BIN_DIR" /tmp/lmbench-bin \
    --ro-bind "$LINUX_TIMEOUT" /tmp/lmbench-timeout \
    --ro-bind "$LMBENCH_SH" /tmp/lmbench-shell/sh --bind "$OUT" /tmp/lmbench-output \
    --chdir /tmp --clearenv --setenv PATH /usr/bin:/bin --setenv LC_ALL C \
    --setenv ENOUGH "$ENOUGH" --setenv TIMING_O "$TIMING_O" --setenv LOOP_O "$LOOP_O" \
    --setenv LMBENCH_SH /tmp/lmbench-shell/sh \
    --setenv LMBENCH_TIMEOUT /tmp/lmbench-timeout \
    --setenv LMBENCH_BIN_DIR /tmp/lmbench-bin --setenv LMBENCH_FIFO_CLEANUP 1 \
    --setenv LMBENCH_EXT4_DIR /tmp/lmbench-output/fixtures --setenv LMBENCH_TMP_DIR /tmp \
    --setenv LMBENCH_RUN_TMP /tmp/lmbench-output/raw --setenv LMBENCH_MANAGE_EXT4 0 \
    --setenv LMBENCH_NET_SERVER 127.0.0.1 \
    /tmp/lmbench-shell/sh -c '
        rc=0
        "$LMBENCH_SH" /tmp/lmbench-suite/runner/run.sh "$@" || rc=$?
        ps -eo pid,ppid,stat,comm > /tmp/lmbench-processes-after.txt
        if ! awk '\''$3 !~ /^Z/ && $4 ~ /^(lat_|bw_|lmhttp|lmdd|enough|lmbench-fifo|lmbench-fs)/ { found=1 } END { exit found }'\'' \
            /tmp/lmbench-processes-after.txt; then
            echo "ERROR: benchmark processes survived cleanup" >&2
            rc=1
        fi
        find /tmp /var/tmp -type p > /tmp/lmbench-fifos-after.txt
        if [ -s /tmp/lmbench-fifos-after.txt ]; then
            echo "ERROR: FIFO files survived cleanup" >&2
            rc=1
        fi
        exit "$rc"
    ' sh "$@" > "$WORK/runner.log" 2>&1 || rc=$?
printf '%s\n' "$rc" > "$OUT/exit-status.txt"
date -u '+%Y-%m-%dT%H:%M:%SZ' > "$OUT/finished-at.txt"
grep '^LMBENCH_' "$WORK/runner.log" > "$OUT/results.jsonl" || true
grep '^LMBENCH_SUMMARY ' "$WORK/runner.log" || tail -n 20 "$WORK/runner.log"
printf 'Linux lmbench finished: exit=%s, output=%s\n' "$rc" "$OUT"
exit "$rc"
