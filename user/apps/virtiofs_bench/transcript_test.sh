#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
TMPDIR_ROOT=${TMPDIR:-/tmp}
WORK_DIR=$(mktemp -d "${TMPDIR_ROOT}/virtiofs-bench-transcript.XXXXXX")
trap 'rm -rf "$WORK_DIR"' EXIT HUP INT TERM

BIN="$WORK_DIR/virtiofs_bench"
"${CXX:-c++}" -Wall -Wextra -Werror -O2 -std=c++17 -pthread \
    "$SCRIPT_DIR/virtiofs_bench.cc" -o "$BIN"

check_transcript() {
    awk '
        function value(prefix,    i) {
            for (i = 1; i <= NF; ++i) {
                if (index($i, prefix "=") == 1) {
                    return substr($i, length(prefix) + 2)
                }
            }
            return "__missing__"
        }
        function decimal(name, v) {
            v = value(name)
            return v != "__missing__" && v ~ /^[0-9]+$/
        }
        $1 == "phase" {
            phases++
            if (value("workload") == "__missing__" ||
                value("dataset") == "__missing__" ||
                value("phase") == "__missing__" ||
                (value("event") != "begin" && value("event") != "end") ||
                !decimal("pid") || !decimal("monotonic_us") || !decimal("elapsed_us") ||
                !decimal("offset") || !decimal("requested") ||
                value("returned") !~ /^-?[0-9]+$/ || !decimal("errno") ||
                value("run_id") == "__missing__") {
                bad = 1
            }
            next
        }
        $1 == "result" {
            results++
            checksum = value("checksum")
            if (value("workload") == "__missing__" ||
                (value("status") != "ok" && value("status") != "fail") ||
                !decimal("errno") || !decimal("elapsed_us") || !decimal("bytes") ||
                !decimal("ops") || !decimal("syscalls") || !decimal("short_io") ||
                !decimal("eintr") || checksum !~ /^[0-9a-f]+$/ || length(checksum) != 16) {
                bad = 1
            }
        }
        END { exit bad || phases == 0 || results != 1 }
    '
}

MOUNT="$WORK_DIR/mount"
mkdir "$MOUNT"

VIRTIOFS_BENCH_RUN_ID=host_prepare VIRTIOFS_BENCH_CACHE_MODE=warm \
    "$BIN" --mount "$MOUNT" --workload prepare --path schema_test \
    --file-size 16384 --block-size 4096 >"$WORK_DIR/prepare.log" 2>&1
check_transcript <"$WORK_DIR/prepare.log"

VIRTIOFS_BENCH_RUN_ID=host_read VIRTIOFS_BENCH_CACHE_MODE=warm \
    "$BIN" --mount "$MOUNT" --workload sequential_read --path schema_test \
    --file-size 16384 --block-size 4096 >"$WORK_DIR/read.log" 2>&1
check_transcript <"$WORK_DIR/read.log"

# A missing mandatory result field must be rejected by the host parser.
sed 's/ syscalls=[^ ]*//' "$WORK_DIR/read.log" >"$WORK_DIR/malformed.log"
if check_transcript <"$WORK_DIR/malformed.log"; then
    echo "malformed transcript unexpectedly accepted" >&2
    exit 1
fi

# Argument parsing must reject path traversal and unknown dispatch names before
# any workload is run.
set +e
"$BIN" --mount "$MOUNT" --workload prepare --path ../escape \
    --file-size 4096 --block-size 4096 >"$WORK_DIR/bad-path.log" 2>&1
bad_path_rc=$?
"$BIN" --mount "$MOUNT" --workload does_not_exist \
    >"$WORK_DIR/bad-workload.log" 2>&1
bad_workload_rc=$?
set -e
if [ "$bad_path_rc" -ne 2 ]; then
    echo "unsafe dataset path returned $bad_path_rc, expected parse failure 2" >&2
    exit 1
fi
if [ "$bad_workload_rc" -ne 2 ]; then
    echo "unknown workload returned $bad_workload_rc, expected parse failure 2" >&2
    exit 1
fi
if find "$WORK_DIR" -name '*escape*' -print | grep -q .; then
    echo "unsafe dataset path changed the filesystem" >&2
    exit 1
fi

expect_parse_failure() {
    name=$1
    shift
    set +e
    "$BIN" --mount "$MOUNT" --workload prepare --path "parse_${name}" "$@" \
        >"$WORK_DIR/parse-${name}.log" 2>&1
    rc=$?
    set -e
    if [ "$rc" -ne 2 ]; then
        echo "$name returned $rc, expected parse failure 2" >&2
        exit 1
    fi
}

# strtoull accepts signs and can turn -1 into SIZE_MAX. Resource-sized
# arguments must reject both forms, as well as values beyond their documented
# workload limits, before allocating or creating a dataset.
expect_parse_failure negative --file-size -1
expect_parse_failure plus --block-size +4096
expect_parse_failure size_max --file-size 18446744073709551615
expect_parse_failure file_too_large --file-size 1073741825
expect_parse_failure block_too_large --block-size 16777217
expect_parse_failure too_many_files --files 1000001
expect_parse_failure too_many_workers --workers 1025
expect_parse_failure too_many_iterations --iterations 100000001

DATASET_DIR="$MOUNT/.virtiofs_bench_schema_test"
cp "$DATASET_DIR/seq.dat" "$WORK_DIR/old-seq.dat"
cp "$DATASET_DIR/manifest.v1" "$WORK_DIR/old-manifest.v1"
set +e
VIRTIOFS_BENCH_TEST_FAULT=before_manifest_publish \
    "$BIN" --mount "$MOUNT" --workload prepare --path schema_test --seed 99 \
    --file-size 16384 --block-size 4096 >"$WORK_DIR/rollback.log" 2>&1
rollback_rc=$?
set -e
if [ "$rollback_rc" -eq 0 ] || ! cmp -s "$DATASET_DIR/seq.dat" "$WORK_DIR/old-seq.dat" || \
    ! cmp -s "$DATASET_DIR/manifest.v1" "$WORK_DIR/old-manifest.v1"; then
    echo "failed prepare did not restore the old valid dataset" >&2
    exit 1
fi
VIRTIOFS_BENCH_RUN_ID=rollback_verify \
    "$BIN" --mount "$MOUNT" --workload sequential_read --path schema_test \
    --file-size 16384 --block-size 4096 >"$WORK_DIR/rollback-verify.log" 2>&1
check_transcript <"$WORK_DIR/rollback-verify.log"

# Force one prepare to pause after publishing seq.dat but before publishing its
# manifest. A second prepare for the same fixed dataset must wait on the stable
# per-dataset lock and then publish a complete pair of its own.
for seed in 101 202; do
    VIRTIOFS_BENCH_RUN_ID="expected_${seed}" \
        "$BIN" --mount "$MOUNT" --workload prepare --path "expected_${seed}" --seed "$seed" \
        --file-size 16384 --block-size 4096 >"$WORK_DIR/expected-${seed}.log" 2>&1
done
VIRTIOFS_BENCH_RUN_ID=concurrent_a VIRTIOFS_BENCH_TEST_FAULT=delay_before_manifest_publish \
    "$BIN" --mount "$MOUNT" --workload prepare --path concurrent_prepare --seed 101 \
    --file-size 16384 --block-size 4096 >"$WORK_DIR/concurrent-a.log" 2>&1 &
prepare_a_pid=$!
concurrent_dir="$MOUNT/.virtiofs_bench_concurrent_prepare"
published_a=0
for unused in $(seq 1 200); do
    if [ -f "$concurrent_dir/seq.dat" ] && \
        cmp -s "$concurrent_dir/seq.dat" "$MOUNT/.virtiofs_bench_expected_101/seq.dat"; then
        published_a=1
        break
    fi
    sleep 0.01
done
if [ "$published_a" -ne 1 ]; then
    kill "$prepare_a_pid" 2>/dev/null || true
    wait "$prepare_a_pid" 2>/dev/null || true
    echo "first concurrent prepare did not enter the publish window" >&2
    exit 1
fi
VIRTIOFS_BENCH_RUN_ID=concurrent_b \
    "$BIN" --mount "$MOUNT" --workload prepare --path concurrent_prepare --seed 202 \
    --file-size 16384 --block-size 4096 >"$WORK_DIR/concurrent-b.log" 2>&1 &
prepare_b_pid=$!
wait "$prepare_a_pid"
wait "$prepare_b_pid"
check_transcript <"$WORK_DIR/concurrent-a.log"
check_transcript <"$WORK_DIR/concurrent-b.log"
if ! cmp -s "$concurrent_dir/seq.dat" "$MOUNT/.virtiofs_bench_expected_202/seq.dat" || \
    ! cmp -s "$concurrent_dir/manifest.v1" \
        "$MOUNT/.virtiofs_bench_expected_202/manifest.v1"; then
    echo "concurrent prepare left a mixed data/manifest pair" >&2
    exit 1
fi
if find "$concurrent_dir" -maxdepth 1 \( -name '.seq.tmp.*' -o -name '.manifest.tmp.*' \
    -o -name '.seq.backup.*' -o -name '.manifest.backup.*' \) -print | grep -q .; then
    echo "concurrent prepare left transaction artifacts" >&2
    exit 1
fi
VIRTIOFS_BENCH_RUN_ID=concurrent_verify \
    "$BIN" --mount "$MOUNT" --workload sequential_read --path concurrent_prepare --seed 202 \
    --file-size 16384 --block-size 4096 >"$WORK_DIR/concurrent-verify.log" 2>&1
check_transcript <"$WORK_DIR/concurrent-verify.log"

expect_manifest_failure() {
    name=$1
    set +e
    "$BIN" --mount "$MOUNT" --workload sequential_read --path schema_test \
        --file-size 16384 --block-size 4096 >"$WORK_DIR/manifest-${name}.log" 2>&1
    rc=$?
    set -e
    if [ "$rc" -eq 0 ]; then
        echo "malformed manifest $name unexpectedly accepted" >&2
        exit 1
    fi
}

cp "$WORK_DIR/old-manifest.v1" "$DATASET_DIR/manifest.v1"
printf 'trailing_token\n' >>"$DATASET_DIR/manifest.v1"
expect_manifest_failure trailing

cp "$WORK_DIR/old-manifest.v1" "$DATASET_DIR/manifest.v1"
printf 'size 16384\n' >>"$DATASET_DIR/manifest.v1"
expect_manifest_failure duplicate

sed 's/^size .*/size -1/' "$WORK_DIR/old-manifest.v1" >"$DATASET_DIR/manifest.v1"
expect_manifest_failure negative

sed 's/^size .*/size 18446744073709551616/' "$WORK_DIR/old-manifest.v1" \
    >"$DATASET_DIR/manifest.v1"
expect_manifest_failure overflow

# Cleanup is control-plane recovery: it must not depend on parsing a corrupt
# manifest, must remove transaction leftovers, and must be idempotent.
touch "$DATASET_DIR/.seq.tmp.stale" "$DATASET_DIR/.manifest.tmp.stale" \
    "$DATASET_DIR/.seq.backup.stale" "$DATASET_DIR/.manifest.backup.stale"
VIRTIOFS_BENCH_RUN_ID=cleanup_corrupt \
    "$BIN" --mount "$MOUNT" --workload cleanup --path schema_test \
    >"$WORK_DIR/cleanup.log" 2>&1
check_transcript <"$WORK_DIR/cleanup.log"
if [ -e "$DATASET_DIR" ]; then
    echo "cleanup left known dataset objects behind" >&2
    exit 1
fi
VIRTIOFS_BENCH_RUN_ID=cleanup_again \
    "$BIN" --mount "$MOUNT" --workload cleanup --path schema_test \
    >"$WORK_DIR/cleanup-again.log" 2>&1
check_transcript <"$WORK_DIR/cleanup-again.log"
echo "virtiofs benchmark transcript tests: PASS"
