#!/bin/sh
# Capture one Linux virtiofs sequential-read reference window with tracefs.

set -eu

die() {
  printf 'error: %s\n' "$*" >&2
  exit 2
}

usage() {
  cat <<'EOF'
Usage: linux_reference_trace.sh --run-id ID --case-id ID --helper PATH \
  --helper-sha256 SHA256 --mount PATH --dataset NAME --file-size BYTES \
  --block-size BYTES --output-dir DIR

Run this script as root inside an otherwise idle Linux reference guest.  The
dataset must already have been prepared by the same virtiofs_bench helper.
EOF
}

run_id= case_id= helper= helper_sha256= mount_path= dataset=
file_size= block_size= output_dir=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --run-id) [ "$#" -ge 2 ] || die "missing --run-id value"; run_id=$2; shift 2 ;;
    --case-id) [ "$#" -ge 2 ] || die "missing --case-id value"; case_id=$2; shift 2 ;;
    --helper) [ "$#" -ge 2 ] || die "missing --helper value"; helper=$2; shift 2 ;;
    --helper-sha256) [ "$#" -ge 2 ] || die "missing --helper-sha256 value"; helper_sha256=$2; shift 2 ;;
    --mount) [ "$#" -ge 2 ] || die "missing --mount value"; mount_path=$2; shift 2 ;;
    --dataset) [ "$#" -ge 2 ] || die "missing --dataset value"; dataset=$2; shift 2 ;;
    --file-size) [ "$#" -ge 2 ] || die "missing --file-size value"; file_size=$2; shift 2 ;;
    --block-size) [ "$#" -ge 2 ] || die "missing --block-size value"; block_size=$2; shift 2 ;;
    --output-dir) [ "$#" -ge 2 ] || die "missing --output-dir value"; output_dir=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done

for pair in "run-id:$run_id" "case-id:$case_id" "helper:$helper" \
  "helper-sha256:$helper_sha256" "mount:$mount_path" "dataset:$dataset" \
  "file-size:$file_size" "block-size:$block_size" "output-dir:$output_dir"; do
  [ -n "${pair#*:}" ] || die "--${pair%%:*} is required"
done
printf '%s\n' "$run_id" | grep -Eq '^[A-Za-z0-9._-]+$' && \
  printf '%s\n' "$case_id" | grep -Eq '^[A-Za-z0-9._-]+$' || \
  die "run/case IDs must be safe tokens"
printf '%s\n' "$dataset" | grep -Eq '^[A-Za-z0-9._-]+$' || \
  die "dataset must be one safe path component"
[ "$dataset" != . ] && [ "$dataset" != .. ] || die "dataset must be one safe path component"
printf '%s\n' "$file_size" | grep -Eq '^[1-9][0-9]*$' && \
  printf '%s\n' "$block_size" | grep -Eq '^[1-9][0-9]*$' || \
  die "file and block sizes must be positive decimal integers"
printf '%s\n' "$helper_sha256" | grep -Eq '^[0-9a-f]{64}$' || die "helper SHA-256 is invalid"
[ -x "$helper" ] && [ ! -L "$helper" ] || die "helper must be a non-symlink executable"
[ "$(sha256sum "$helper" | awk '{print $1}')" = "$helper_sha256" ] || \
  die "guest helper differs from the attested helper"
[ "$(uname -s)" = Linux ] || die "this collector only runs in a Linux guest"
case "$mount_path" in /*) ;; *) die "mount path must be absolute" ;; esac

mount_record=$(awk -v target="$mount_path" '
  $2 == target { if (found) exit 2; print $1 "\t" $3 "\t" $4; found=1 }
  END { if (!found) exit 1 }
' /proc/mounts) || die "cannot identify one mount record for target"
mount_source=$(printf '%s\n' "$mount_record" | awk -F '\t' '{print $1}')
mount_fstype=$(printf '%s\n' "$mount_record" | awk -F '\t' '{print $2}')
mount_options=$(printf '%s\n' "$mount_record" | awk -F '\t' '{print $3}')
[ "$mount_fstype" = virtiofs ] || [ "$mount_fstype" = fuse.virtiofs ] || \
  die "target is not a virtiofs mount"
[ ! -e "$output_dir" ] || die "refusing to replace output directory"
umask 077
mkdir "$output_dir"
published=0
cleanup_output() {
  if [ "$published" -eq 0 ]; then
    chmod -R u+w "$output_dir" 2>/dev/null || true
    rm -rf "$output_dir"
  fi
}
trap cleanup_output EXIT HUP INT TERM

trace_root=/sys/kernel/tracing
[ -e "$trace_root/kprobe_events" ] || trace_root=/sys/kernel/debug/tracing
[ -w "$trace_root/kprobe_events" ] && [ -w "$trace_root/instances" ] || die "writable tracefs is required"
grep -Eq '^fuse_simple_request([[:space:]]|$)' "$trace_root/available_filter_functions" || \
  die "kernel does not expose fuse_simple_request to tracefs"
grep -Eq '^fuse_simple_background([[:space:]]|$)' "$trace_root/available_filter_functions" || \
  die "kernel does not expose fuse_simple_background to tracefs"

group=dragonos_virtiofs_ref
sync_event=read_sync
async_event=read_async
[ ! -e "$trace_root/events/$group/$sync_event" ] && \
  [ ! -e "$trace_root/events/$group/$async_event" ] || \
  die "reference trace probes already exist; another collection may be active"
instance=$trace_root/instances/dragonos_virtiofs_ref
[ ! -e "$instance" ] || die "reference trace instance already exists"
mkdir "$instance"
tracefs=$instance

remove_probes() {
  printf '%s\n' "-:$group/$sync_event" "-:$group/$async_event" \
    >"$trace_root/kprobe_events" 2>/dev/null || true
}

cleanup() {
  printf '0\n' >"$tracefs/tracing_on" 2>/dev/null || true
  printf '0\n' >"$tracefs/events/$group/$sync_event/enable" 2>/dev/null || true
  printf '0\n' >"$tracefs/events/$group/$async_event/enable" 2>/dev/null || true
  remove_probes
  rmdir "$instance" 2>/dev/null || true
  cleanup_output
}
trap cleanup EXIT HUP INT TERM

# Linux 6.6.139: fuse_args.opcode +8, in_args[0].value +32,
# fuse_read_in.size +16.  The exact definition and generated format are sealed.
sync_definition='p:dragonos_virtiofs_ref/read_sync fuse_simple_request args=$arg2:u64 opcode=+8($arg2):u32 read_size=+16(+32($arg2)):u32'
async_definition='p:dragonos_virtiofs_ref/read_async fuse_simple_background args=$arg2:u64 opcode=+8($arg2):u32 read_size=+16(+32($arg2)):u32'
printf '%s\n' "$sync_definition" "$async_definition" >"$output_dir/probe-definition"
printf '%s\n' "$sync_definition" >>"$trace_root/kprobe_events"
printf '%s\n' "$async_definition" >>"$trace_root/kprobe_events"
printf 'mono\n' >"$tracefs/trace_clock"
cp "$tracefs/events/$group/$sync_event/format" "$output_dir/format-sync"
cp "$tracefs/events/$group/$async_event/format" "$output_dir/format-async"
cat "$tracefs/trace_clock" >"$output_dir/trace-clock"

{
  printf 'schema\tdragonos.virtiofs.linux-guest-identity.v1\n'
  printf 'run_id\t%s\ncase_id\t%s\n' "$run_id" "$case_id"
  printf 'boot_id\t%s\n' "$(tr -d '\n' </proc/sys/kernel/random/boot_id)"
  printf 'sysname\t%s\nrelease\t%s\nversion\t%s\nmachine\t%s\n' \
    "$(uname -s)" "$(uname -r)" "$(uname -v)" "$(uname -m)"
  printf 'kernel_cmdline\t%s\n' "$(tr -d '\n' </proc/cmdline)"
  printf 'helper_path\t%s\nhelper_sha256\t%s\n' "$helper" "$helper_sha256"
  printf 'mount_path\t%s\nmount_source\t%s\nmount_fstype\t%s\nmount_options\t%s\n' \
    "$mount_path" "$mount_source" "$mount_fstype" "$mount_options"
} >"$output_dir/guest-identity.tsv"

: >"$tracefs/trace"
printf '0\n' >"$tracefs/tracing_on"
VIRTIOFS_STATS_PATH= VIRTIOFS_QUIESCENCE_PATH= \
VIRTIOFS_BENCH_MOUNT_OPTIONS="$mount_options" \
VIRTIOFS_BENCH_RUN_ID="$run_id" VIRTIOFS_BENCH_CACHE_MODE=linux-reference \
/bin/sh -c '
  kill -STOP "$$"
  exec "$@"
' sh "$helper" --mount "$mount_path" --workload sequential_read --path "$dataset" \
  --file-size "$file_size" --block-size "$block_size" >"$output_dir/transcript" 2>&1 &
helper_pid=$!
state=
for unused in $(seq 1 200); do
  state=$(awk '{print $3}' "/proc/$helper_pid/stat" 2>/dev/null || true)
  [ "$state" = T ] && break
  sleep 0.01
done
[ "$state" = T ] || die "could not freeze helper before trace activation"
printf 'common_pid == %s\n' "$helper_pid" >"$tracefs/events/$group/$sync_event/filter"
printf 'common_pid == %s\n' "$helper_pid" >"$tracefs/events/$group/$async_event/filter"
printf '1\n' >"$tracefs/events/$group/$sync_event/enable"
printf '1\n' >"$tracefs/events/$group/$async_event/enable"
printf '1\n' >"$tracefs/tracing_on"
printf 'LINUX_REF_BEGIN run_id=%s case_id=%s helper_pid=%s\n' \
  "$run_id" "$case_id" "$helper_pid" >"$tracefs/trace_marker"
kill -CONT "$helper_pid"
set +e
wait "$helper_pid"
helper_rc=$?
set -e
printf 'LINUX_REF_END run_id=%s case_id=%s helper_pid=%s rc=%s\n' \
  "$run_id" "$case_id" "$helper_pid" "$helper_rc" >"$tracefs/trace_marker"
printf '0\n' >"$tracefs/tracing_on"
cat "$tracefs/trace" >"$output_dir/trace"
printf 'helper_pid\t%s\nhelper_rc\t%s\n' "$helper_pid" "$helper_rc" >"$output_dir/execution.tsv"
[ "$helper_rc" -eq 0 ] || die "virtiofs_bench failed with status $helper_rc"
chmod -R a-w "$output_dir"
published=1
printf 'linux_reference_trace=%s\n' "$output_dir"
