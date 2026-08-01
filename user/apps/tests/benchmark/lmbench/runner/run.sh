#!/bin/busybox sh
# LMbench benchmark runner (guest side).
#
# Entry point invoked by /etc/init.d/rcS when AUTO_TEST=benchmark. Runs each
# whitelisted case N times, extracts a numeric result per test_cases/<name>.meta,
# computes summary statistics, and emits ONE JSON line per metric (prefixed with
# "LMBENCH_JSON ") to the serial console for the host-side collector to harvest.
#
# Output contract (host collect_results.py depends on it):
#   ===LMBENCH_RUN_BEGIN===
#   LMBENCH_META    {json}          # run-level info known inside the guest
#   LMBENCH_JSON    {json}          # one per metric (JSONL, single line each)
#   LMBENCH_SUMMARY {json}          # totals
#   ===LMBENCH_RUN_END===
#   benchmark测试完成                # completion marker (mirrors gvisor "测试完成")
set -u

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
CASES_DIR="$SCRIPT_DIR/test_cases"
CONFIG_FILE="$SCRIPT_DIR/config"
WHITELIST_FILE="$SCRIPT_DIR/whitelist.txt"
WORK_TMP="${LMBENCH_RUN_TMP:-/tmp/lmbench_run}"

# Source profile for PATH etc., but tolerate profile scripts that reference
# unset vars (set -u would otherwise abort the whole run).
if [ -f /etc/profile ]; then set +u; . /etc/profile 2>/dev/null || true; set -u; fi

# ---- global config (overridable via config file) ----
SAMPLES=5
TIMEOUT_SEC=120
WARMUP=0
SUITE_VERSION="3.0-a9"

# CLI intent variables (set by parse_args, applied after load_config).
CLI_SAMPLES=""; CLI_TIMEOUT=""; CLI_WARMUP=""
CLI_WHITELIST=""; CLI_CONFIG=""; ONLY_NAME=""; LIST_ONLY=""

log() { echo "[lmbench-runner] $*"; }

usage() {
    cat >&2 <<'EOF'
usage: run.sh [--samples N] [--timeout S] [--warmup N]
              [--whitelist FILE] [--config FILE] [--only NAME] [--list]
EOF
}

# Parse CLI args into CLI_* intent variables (applied after load_config).
# Returns 2 on unknown arg (caller exits); does not exit the shell itself
# so it can be unit-tested via `LMBENCH_RUNNER_NO_MAIN=1 . run.sh`.
parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --samples)    CLI_SAMPLES=$2;    shift 2 ;;
            --timeout)    CLI_TIMEOUT=$2;    shift 2 ;;
            --warmup)     CLI_WARMUP=$2;     shift 2 ;;
            --whitelist)  CLI_WHITELIST=$2;  shift 2 ;;
            --config)     CLI_CONFIG=$2;     shift 2 ;;
            --only)       ONLY_NAME=$2;      shift 2 ;;
            --list)       LIST_ONLY=1;       shift ;;
            -h|--help)    usage; return 0 ;;
            *)            usage; return 2 ;;
        esac
    done
}

# Apply CLI overrides on top of config-file values.
apply_cli_overrides() {
    [ -n "$CLI_SAMPLES" ]   && SAMPLES=$CLI_SAMPLES
    [ -n "$CLI_TIMEOUT" ]   && TIMEOUT_SEC=$CLI_TIMEOUT
    [ -n "$CLI_WARMUP" ]    && WARMUP=$CLI_WARMUP
    [ -n "$CLI_WHITELIST" ] && WHITELIST_FILE=$CLI_WHITELIST
}

# Read KEY=VALUE from a file without sourcing it (avoids clobbering runner state).
# Use shell builtins only: DragonOS can stall when repeatedly constructing short
# grep|head|cut pipelines during early boot.
kv_get() {
    _kv_file=$1
    _kv_key=$2
    [ -f "$_kv_file" ] || return 0
    while IFS= read -r _kv_line; do
        case "$_kv_line" in
            "$_kv_key="*)
                printf '%s\n' "${_kv_line#*=}"
                return 0
                ;;
        esac
    done < "$_kv_file"
}

load_config() {
    [ -f "$CONFIG_FILE" ] || return 0
    v=$(kv_get "$CONFIG_FILE" SAMPLES);     [ -n "$v" ] && SAMPLES=$v
    v=$(kv_get "$CONFIG_FILE" TIMEOUT_SEC); [ -n "$v" ] && TIMEOUT_SEC=$v
    v=$(kv_get "$CONFIG_FILE" WARMUP);      [ -n "$v" ] && WARMUP=$v
}

# Minimal JSON string escaper: backslash, quote, then collapse control chars.
json_escape() {
    printf '%s' "$1" \
        | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' \
        | tr '\n\r\t' '   '
}

# Is the argument a plain decimal number?
is_number() {
    case "$1" in
        ''|*[!0-9.eE+-]*) return 1 ;;
        *) printf '%s\n' "$1" | grep -Eq '^[-+]?[0-9]+(\.[0-9]+)?([eE][-+]?[0-9]+)?$' ;;
    esac
}

# Run "$@" in a dedicated process group with a wall-clock timeout.
# GNU timeout uses setpgid (not setsid), avoiding DragonOS's setsid fault while
# still terminating background servers spawned by a benchmark wrapper.
run_with_timeout() {
    secs=$1; outf=$2; shift 2
    /usr/local/bin/timeout -k 2s "${secs}s" "$@" >"$outf" 2>&1
    rc=$?
    case "$rc" in
        124|137) return 124 ;;
        *) return "$rc" ;;
    esac
}

# Extract the metric value from a captured-output file.
# PAT/IDX/NTH are metadata globals. Keep field selection in shell instead of
# interpolating $(NF-1) into awk; DragonOS BusyBox awk rejects that construct.
extract_value() {
    _line=$(grep -E "$PAT" "$1" 2>/dev/null | sed -n "${NTH}p")
    [ -n "$_line" ] || return 1

    set -- $_line
    case "$IDX" in
        NF)
            eval '_value=${'$#'}'
            ;;
        NF-1)
            _field=$(($# - 1))
            [ "$_field" -gt 0 ] || return 1
            eval '_value=${'$_field'}'
            ;;
        *[!0-9]*|'')
            return 1
            ;;
        *)
            [ "$IDX" -le "$#" ] || return 1
            eval '_value=${'$IDX'}'
            ;;
    esac
    printf '%s\n' "$_value"
}

# Compute stats from space-separated numbers on $1. Echoes:
#   count mean median stddev min max cv
# sqrt is done via Newton's method so we never depend on busybox awk's optional
# libm (CONFIG_FEATURE_AWK_LIBM); only + - * / are used.
compute_stats() {
    printf '%s\n' $1 | /bin/busybox sort -n | /bin/busybox awk '
        function nsqrt(x,   g, i) {
            if (x <= 0) return 0
            g = x
            for (i = 0; i < 60; i++) g = (g + x / g) / 2
            return g
        }
        { v[NR] = $1 + 0; sum += $1 }
        END {
            n = NR; if (n == 0) { exit }
            mean = sum / n
            for (i = 1; i <= n; i++) { d = v[i] - mean; ss += d * d }
            sd = (n > 1) ? nsqrt(ss / (n - 1)) : 0
            median = (n % 2) ? v[(n + 1) / 2] : (v[n / 2] + v[n / 2 + 1]) / 2
            cv = (mean != 0) ? sd / mean : 0
            printf "%d %.6f %.6f %.6f %.6f %.6f %.6f", n, mean, median, sd, v[1], v[n], cv
        }'
}

# Common metric JSON prefix built from .meta fields (no trailing brace).
metric_head() {
    _big=false; [ "$BIGGER" = "1" ] && _big=true
    printf '{"name":"%s","category":"%s","binary":"%s","metric_type":"%s","unit":"%s","bigger_is_better":%s,"description":"%s"' \
        "$NAME" "$CATEGORY" "$BINARY" "$MTYPE" "$UNIT" "$_big" "$(json_escape "$DESC")"
}

# Run one whitelisted case; emit its LMBENCH_JSON line.
# Returns 0=ok, 1=failed, 2=skipped.
run_one_case() {
    NAME=$1
    case_sh="$CASES_DIR/$NAME.sh"
    meta="$CASES_DIR/$NAME.meta"

    if [ ! -f "$case_sh" ]; then
        log "SKIP $NAME: test_cases/$NAME.sh not found"
        printf 'LMBENCH_JSON {"name":"%s","metric_type":"other","unit":"","bigger_is_better":false,"status":"skipped","error":"missing script"}\n' "$NAME"
        return 2
    fi

    # metadata (with defaults)
    log "loading metadata for $NAME"
    CATEGORY=$(kv_get "$meta" CATEGORY);       [ -n "$CATEGORY" ] || CATEGORY=other
    BINARY=$(kv_get "$meta" BINARY);           [ -n "$BINARY" ]   || BINARY=""
    MTYPE=$(kv_get "$meta" METRIC_TYPE);       [ -n "$MTYPE" ]    || MTYPE=other
    UNIT=$(kv_get "$meta" UNIT);               [ -n "$UNIT" ]     || UNIT=""
    BIGGER=$(kv_get "$meta" BIGGER_IS_BETTER); [ -n "$BIGGER" ]   || BIGGER=0
    PAT=$(kv_get "$meta" SEARCH_PATTERN);      [ -n "$PAT" ]      || PAT='^[0-9]'
    IDX=$(kv_get "$meta" RESULT_INDEX);        [ -n "$IDX" ]      || IDX=NF
    NTH=$(kv_get "$meta" NTH_OCCURRENCE);      [ -n "$NTH" ]      || NTH=1
    DESC=$(kv_get "$meta" DESCRIPTION)
    n_samples=$(kv_get "$meta" SAMPLES);       [ -n "$n_samples" ] || n_samples=$SAMPLES

    log "RUN  $NAME (samples=$n_samples, timeout=${TIMEOUT_SEC}s)"

    samples=""
    last_out="$WORK_TMP/$NAME.out"
    last_rc=0
    i=1
    total_iter=$((WARMUP + n_samples))
    idx_iter=0
    while [ "$idx_iter" -lt "$total_iter" ]; do
        idx_iter=$((idx_iter + 1))
        run_with_timeout "$TIMEOUT_SEC" "$last_out" /bin/busybox sh "$case_sh"
        last_rc=$?
        [ "$idx_iter" -le "$WARMUP" ] && continue     # discard warmup rounds
        val=$(extract_value "$last_out")
        val=$(printf '%s' "$val" | tr -d ' \t\r')
        if [ "$last_rc" -eq 0 ] && is_number "$val"; then
            samples="$samples $val"
        else
            log "     $NAME sample $i: no valid numeric result (rc=$last_rc)"
        fi
        i=$((i + 1))
    done

    samples=$(printf '%s' "$samples" | sed 's/^ *//')

    if [ -z "$samples" ]; then
        err="no numeric match"
        [ "$last_rc" -eq 124 ] && err="timeout"
        raw=$(tail -c 300 "$last_out" 2>/dev/null)
        printf 'LMBENCH_JSON %s,"status":"failed","error":"%s","raw_tail":"%s"}\n' \
            "$(metric_head)" "$err" "$(json_escape "$raw")"
        log "FAIL $NAME: $err"
        return 1
    fi

    stats=$(compute_stats "$samples")
    if [ -z "$stats" ]; then
        printf 'LMBENCH_JSON %s,"status":"failed","error":"stats computation failed"}\n' "$(metric_head)"
        log "FAIL $NAME: stats computation failed"
        return 1
    fi
    set -- $stats
    s_count=$1; s_mean=$2; s_median=$3; s_sd=$4; s_min=$5; s_max=$6; s_cv=$7
    samples_json="[$(printf '%s' "$samples" | tr ' ' ',')]"
    printf 'LMBENCH_JSON %s,"status":"ok","samples":%s,"stats":{"count":%s,"mean":%s,"median":%s,"stddev":%s,"min":%s,"max":%s,"cv":%s}}\n' \
        "$(metric_head)" "$samples_json" \
        "$s_count" "$s_mean" "$s_median" "$s_sd" "$s_min" "$s_max" "$s_cv"
    log "OK   $NAME: mean=$s_mean $UNIT (n=$s_count, cv=$s_cv)"
    return 0
}

# ============================== main ==============================
run_main() {
    parse_args "$@" || exit $?

    # --config may redirect the config file before load_config reads it.
    [ -n "$CLI_CONFIG" ] && CONFIG_FILE=$CLI_CONFIG
    load_config
    apply_cli_overrides
    mkdir -p "$WORK_TMP"

    if [ "$LIST_ONLY" = "1" ]; then
        ls "$CASES_DIR"/*.sh 2>/dev/null | sed 's|.*/||; s|\.sh$||'
        return 0
    fi

    # --only: replace whitelist with a single-line file, reusing run_one_case
    # unchanged (no second dispatch branch).
    if [ -n "$ONLY_NAME" ]; then
        wl_tmp="$WORK_TMP/only_whitelist"
        printf '%s\n' "$ONLY_NAME" > "$wl_tmp"
        WHITELIST_FILE="$wl_tmp"
    fi

log "LMbench benchmark run starting"
echo "===LMBENCH_RUN_BEGIN==="
printf 'LMBENCH_META {"suite":"lmbench","suite_version":"%s","samples":%s,"timeout_sec":%s,"warmup":%s}\n' \
    "$SUITE_VERSION" "$SAMPLES" "$TIMEOUT_SEC" "$WARMUP"

    if [ -f "$SCRIPT_DIR/init.sh" ]; then
        log "initializing test environment (init.sh)..."
        if ! sh "$SCRIPT_DIR/init.sh"; then
            log "ERROR: test environment initialization failed"
            printf 'LMBENCH_SUMMARY {"total":0,"ok":0,"failed":0,"skipped":0}\n'
            echo "===LMBENCH_RUN_END==="
            echo "benchmark测试完成"
            exit 1
        fi
    fi

    # Pre-compute ENOUGH once. lmbench's BENCH_INNER macro (bench.h) loops
    # `while(__result < 0.95 * get_enough(0))`; under virtualized timing
    # compute_enough() can return SHORT=1000000, making __iterations explode
    # (<<=3 past the 1<<27 break that resets __result=0) into an infinite
    # while — the observed benchmp hangs on WSL2. Keep enough <= 100000 so
    # the inner calibration converges. lmbench's scripts/config-run
    # precomputes ENOUGH via the `enough` binary; we do the same, then clamp
    # to <=100000, with a 50000 (REAL_SHORT) fallback on hang/failure.
    if [ -z "${ENOUGH:-}" ]; then
        . "$SCRIPT_DIR/env.sh" 2>/dev/null || true
        if [ -x "${LMBENCH_BIN_DIR:-}/enough" ]; then
            ENOUGH=$(/usr/local/bin/timeout -k 2s 10s "${LMBENCH_BIN_DIR}/enough" 2>/dev/null) || ENOUGH=50000
            case "$ENOUGH" in *[!0-9]*|'') ENOUGH=50000 ;; esac
            [ "$ENOUGH" -gt 100000 ] 2>/dev/null && ENOUGH=50000
        else
            ENOUGH=50000
        fi
        export ENOUGH
        log "ENOUGH=$ENOUGH (precomputed via enough tool, clamped <=100000, fallback 50000)"
    fi

if [ ! -f "$WHITELIST_FILE" ]; then
    log "ERROR: whitelist not found: $WHITELIST_FILE"
    echo "===LMBENCH_RUN_END==="
    echo "benchmark测试完成"
    exit 1
fi

    total=0; ok=0; failed=0; skipped=0
    while read -r line; do
        case "$line" in ''|\#*) continue ;; esac
        total=$((total + 1))
        log "dispatching $line"
        run_one_case "$line"
    case $? in
        0) ok=$((ok + 1)) ;;
        2) skipped=$((skipped + 1)) ;;
        *) failed=$((failed + 1)) ;;
    esac
    echo "---"
done < "$WHITELIST_FILE"

printf 'LMBENCH_SUMMARY {"total":%s,"ok":%s,"failed":%s,"skipped":%s}\n' \
    "$total" "$ok" "$failed" "$skipped"
echo "===LMBENCH_RUN_END==="

if [ -f "$SCRIPT_DIR/clean_up.sh" ]; then
    sh "$SCRIPT_DIR/clean_up.sh" >/dev/null 2>&1 || true
fi

log "done: total=$total ok=$ok failed=$failed skipped=$skipped"
echo "benchmark测试完成"
}

# Allow sourcing for host-side unit tests without executing the run.
[ "${LMBENCH_RUNNER_NO_MAIN:-0}" = "1" ] || run_main "$@"
