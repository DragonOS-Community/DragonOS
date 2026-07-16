#!/usr/bin/env bash
# Host-side evidence orchestrator for DragonOS virtiofs non-DAX benchmarks.
# Guest serial input is deliberately manual until a reliable console transport exists.

set -euo pipefail

readonly RUNNER_VERSION="4"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR
source "${SCRIPT_DIR}/common.sh"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd)"
readonly REPO_ROOT
readonly ORIGINAL_ARGV=("$@")

usage() {
  cat <<'EOF'
Usage:
  non_dax_bench_runner.sh plan [options]
  non_dax_bench_runner.sh collect --run-dir DIR --case ID --status STATUS \
    --artifact NAME=PATH [--artifact NAME=PATH ...]
  non_dax_bench_runner.sh finalize --run-dir DIR
  non_dax_bench_runner.sh verify --run-dir DIR

Plan options:
  --mode performance|light|diagnostic
                                  Observation mode (default: light)
  --phase prepare|read            Dataset phase (default: read)
  --profile quick|full            Case matrix (default: quick)
  --share-dir DIR                 Host directory exported by virtiofsd
  --dataset NAME                  Stable single-component dataset name
  --evidence-root DIR             Parent of the immutable run directory
  --guest-mount DIR               Guest virtiofs mount (default: /tmp/host)
  --helper PATH                   Guest benchmark helper (default: /bin/virtiofs_bench)
  --build-manifest FILE          Trusted clean-tree build manifest (required)
  --block-sizes CSV               Override matrix block sizes in bytes
  --file-sizes CSV                Override matrix file sizes in bytes
  --timeout SECONDS               Manual watchdog threshold recorded in the plan
  --vcpus COUNT                   Planned QEMU vCPU count (default: 2)
  --memory SIZE                   Planned QEMU -m value (default: 2G)
  --accel kvm|tcg                 Planned QEMU accelerator (default: kvm)
  --guest-cache cold|warm         Guest cache label (default: cold)
  --host-cache warm|unknown       Host cache label; "cold" is intentionally rejected

Collect status: completed, failed, timeout, interrupted, skipped.
Every completed case requires an independent pre-workload config artifact. Timeout collection
requires non-empty gdb, serial, and stats artifacts. Existing evidence is never replaced.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 2
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

sha256_or_unavailable() {
  local path="$1"
  if [[ -f "${path}" ]]; then
    sha256sum -- "${path}" | awk '{print $1}'
  else
    printf 'unavailable\n'
  fi
}

stable_manifest_artifact_sha256() {
  local path="$1" expected="$2" label="$3" first second
  [[ -f "${path}" && ! -L "${path}" ]] || die "build artifact is not a regular non-symlink file: ${label}"
  first="$(sha256_or_unavailable "${path}")"
  second="$(sha256_or_unavailable "${path}")"
  [[ "${first}" == "${expected}" && "${second}" == "${expected}" ]] || \
    die "build artifact changed while sealing the run plan: ${label}"
  printf '%s\n' "${second}"
}

verify_run_build_artifact_binding() {
  local run_dir="$1" manifest="${run_dir}/manifest.json" build="${run_dir}/build-manifest.json"
  jq -e '
    ([.artifacts.kernel_sha256, .artifacts.disk_image_sha256,
      .artifacts.guest_helper_sha256, $build[0].artifacts.kernel.sha256,
      $build[0].artifacts.disk_image.sha256, $build[0].artifacts.guest_helper.sha256] |
      all(type == "string" and test("^[0-9a-f]{64}$"))) and
    .artifacts.kernel_sha256 == $build[0].artifacts.kernel.sha256 and
    .artifacts.disk_image_sha256 == $build[0].artifacts.disk_image.sha256 and
    .artifacts.guest_helper_sha256 == $build[0].artifacts.guest_helper.sha256
  ' --slurpfile build "${build}" "${manifest}" >/dev/null || \
    die "run manifest artifact identities differ from the sealed build manifest"
}

verify_plan_seal() {
  local run_dir="$1"
  [[ -f "${run_dir}/plan.sha256" ]] || die "run lacks plan.sha256"
  local expected actual
  expected=$'manifest.json\nbuild-manifest.json\ncase-matrix.tsv\nguest-commands.sh\nhost-facts.txt\ngit-status.txt\nMANUAL-STAGE.txt\nrunner.sh\ncommon.sh'
  actual="$(awk 'NF == 2 && $1 ~ /^[0-9a-f]{64}$/ { sub(/^\*/, "", $2); print $2 }' \
    "${run_dir}/plan.sha256")"
  [[ "${actual}" == "${expected}" && "$(wc -l <"${run_dir}/plan.sha256")" -eq 9 ]] || \
    die "plan seal does not contain the exact fixed evidence manifest"
  (cd "${run_dir}" && sha256sum -c --status plan.sha256) || \
    die "planned evidence inputs changed after plan creation"
  jq -e --arg version "${RUNNER_VERSION}" '
    .schema == "dragonos.virtiofs.non-dax-run.v2" and .runner_version == $version and
    (.repo.build_manifest_sha256 | test("^[0-9a-f]{64}$"))
  ' "${run_dir}/manifest.json" >/dev/null || die "plan manifest schema/version is incompatible"
  [[ "$(sha256_or_unavailable "${run_dir}/build-manifest.json")" == \
     "$(jq -r '.repo.build_manifest_sha256' "${run_dir}/manifest.json")" ]] || \
    die "sealed build manifest digest differs from run manifest"
  verify_run_build_artifact_binding "${run_dir}"
  [[ "$(head -n 1 -- "${run_dir}/case-matrix.tsv")" == \
     $'case_id\tmode\tphase\tfile_size\tblock_size\tguest_cache\thost_cache' ]] || \
    die "case matrix header is incompatible"
  awk -F '\t' 'NR > 1 {
    if (NF != 7 || $1 !~ /^[A-Za-z0-9][A-Za-z0-9._-]*$/ ||
        $2 !~ /^(performance|light|diagnostic)$/ || $3 !~ /^(prepare|read)$/ ||
        $4 !~ /^[1-9][0-9]*$/ || $4 > 1073741824 ||
        $5 !~ /^[1-9][0-9]*$/ || $5 > 16777216 ||
        $6 !~ /^(cold|warm)$/ || $7 !~ /^(warm|unknown)$/ || seen[$1]++) exit 1
    rows++
  } END { if (rows == 0) exit 1 }' "${run_dir}/case-matrix.tsv" || \
    die "case matrix schema is invalid"
}

verify_collected_case() {
  local run_dir="$1" case_id="$2" case_dir status artifacts_sha expected_sha
  case_dir="${run_dir}/cases/${case_id}"
  [[ -f "${case_dir}/status.json" && -f "${case_dir}/artifacts.tsv" ]] || \
    die "case ${case_id} lacks collector status or artifact index"
  jq -e --arg case_id "${case_id}" --arg version "${RUNNER_VERSION}" '
    .schema == "dragonos.virtiofs.non-dax-case.v4" and
    .runner_version == $version and .case_id == $case_id and
    (.status | IN("completed","failed","timeout","interrupted","skipped")) and
    (.plan_seal_sha256 | type == "string") and
    (.artifacts_tsv_sha256 | type == "string")
  ' "${case_dir}/status.json" >/dev/null || die "case ${case_id} status schema is invalid"
  status="$(jq -r '.status' "${case_dir}/status.json")"
  expected_sha="$(sha256_or_unavailable "${run_dir}/plan.sha256")"
  [[ "$(jq -r '.plan_seal_sha256' "${case_dir}/status.json")" == "${expected_sha}" ]] || \
    die "case ${case_id} was not collected against this plan seal"
  artifacts_sha="$(sha256_or_unavailable "${case_dir}/artifacts.tsv")"
  [[ "$(jq -r '.artifacts_tsv_sha256' "${case_dir}/status.json")" == "${artifacts_sha}" ]] || \
    die "case ${case_id} artifact index changed after collection"

  local name digest size path extra rows=0
  local -a replay_artifacts=()
  declare -A present=()
  while IFS=$'\t' read -r name digest size extra; do
    [[ -n "${name}" && -z "${extra:-}" && "${name}" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ &&
       "${digest}" =~ ^[0-9a-f]{64}$ && "${size}" =~ ^[0-9]+$ ]] || \
      die "case ${case_id} has an invalid artifact index row"
    [[ -z "${present[${name}]:-}" ]] || die "case ${case_id} repeats artifact ${name}"
    present["${name}"]=1
    path="${case_dir}/${name}"
    [[ -f "${path}" && ! -L "${path}" ]] || die "case ${case_id} artifact ${name} is missing"
    [[ "$(sha256_or_unavailable "${path}")" == "${digest}" &&
       "$(stat -c '%s' -- "${path}")" == "${size}" ]] || \
      die "case ${case_id} artifact ${name} changed after collection"
    replay_artifacts+=(--artifact "${name}=${path}")
    ((rows += 1))
  done <"${case_dir}/artifacts.tsv"
  local member
  while IFS= read -r -d '' member; do
    [[ "${member}" == "status.json" || "${member}" == "artifacts.tsv" ||
       -n "${present[${member}]:-}" ]] || \
      die "case ${case_id} contains unindexed member ${member}"
  done < <(find "${case_dir}" -mindepth 1 -maxdepth 1 -printf '%f\0')
  if [[ "${status}" == "skipped" ]]; then
    return 0
  fi
  ((rows > 0)) || die "case ${case_id} has no captured evidence"
  [[ -n "${present[serial]:-}" && -n "${present[qemu_cmdline]:-}" &&
     -n "${present[virtiofsd_cmdline]:-}" ]] || \
    die "case ${case_id} lacks mandatory process/transcript evidence"
  if [[ "${status}" == "timeout" ]]; then
    [[ -n "${present[gdb]:-}" && -n "${present[stats]:-}" ]] || \
      die "timeout case ${case_id} lacks GDB or stats evidence"
  fi
  if [[ "${status}" == "completed" &&
        "$(awk -F '\t' -v id="${case_id}" '$1 == id {print $2}' "${run_dir}/case-matrix.tsv")" != "performance" ]]; then
    [[ -n "${present[stats]:-}" ]] || die "light/diagnostic case ${case_id} lacks stats evidence"
  fi
  if [[ "${status}" == "completed" ]]; then
    [[ -n "${present[config]:-}" && -n "${present[case-result.json]:-}" ]] || \
      die "completed case ${case_id} lacks config or parsed case result evidence"
  fi

  # Hashes prove immutability; replaying the collector's static checks proves
  # that the immutable bytes still have the transcript/stats/argv semantics
  # required by this exact plan and case.
  collect_case --verify-only --run-dir "${run_dir}" --case "${case_id}" \
    --status "${status}" "${replay_artifacts[@]}"
}

version_line() {
  local binary="$1"
  if [[ -x "${binary}" ]]; then
    "${binary}" --version 2>&1 | head -n 1 || true
  else
    printf 'unavailable\n'
  fi
}

count_exact_line() {
  local path="$1" expected="$2"
  awk -v expected="${expected}" '{ sub(/\r$/, ""); if ($0 == expected) count++ }
    END { print count + 0 }' "${path}"
}

extract_stats_delta() {
  local path="$1" workload="$2" key="$3"
  awk -v workload="${workload}" -v key="${key}" '
    function token(line, wanted, parts, n, i, prefix) {
      n=split(line, parts, " "); prefix=wanted "="
      for (i=1; i<=n; i++) if (index(parts[i], prefix)==1) return substr(parts[i], length(prefix)+1)
      return ""
    }
    substr($0,1,12)=="stats_delta " && token($0,"workload")==workload && token($0,"key")==key {
      value=token($0,"delta"); if (value !~ /^-?[0-9]+$/) bad=1; count++
    }
    END { if (bad || count != 1) exit 1; print value }
  ' "${path}"
}

extract_stats_delta_or_zero() {
  local path="$1" workload="$2" key="$3"
  awk -v workload="${workload}" -v key="${key}" '
    function token(line, wanted, parts, n, i, prefix) {
      n=split(line, parts, " "); prefix=wanted "="
      for (i=1; i<=n; i++) if (index(parts[i], prefix)==1) return substr(parts[i], length(prefix)+1)
      return ""
    }
    substr($0,1,12)=="stats_delta " && token($0,"workload")==workload && token($0,"key")==key {
      value=token($0,"delta"); if (value !~ /^-?[0-9]+$/) bad=1; count++
    }
    END { if (bad || count > 1) exit 1; print count == 1 ? value : 0 }
  ' "${path}"
}

stats_snapshot_value() {
  local path="$1" section="$2" key="$3"
  awk -v wanted_section="${section}" -v wanted_key="${key}" '
    /^\[[^]]+\]\r?$/ { section=$0; gsub(/^\[|\]\r?$/, "", section); next }
    section == wanted_section && $1 == wanted_key && $2 ~ /^[0-9]+\r?$/ {
      gsub(/\r$/, "", $2); value=$2; count++
    }
    END { if (count != 1) exit 1; print value }
  ' "${path}"
}

parse_completed_result() {
  local path="$1" workload="$2" dataset="$3" file_size="$4" block_size="$5"
  local begin_marker="$6" end_marker="$7"
  awk -v workload="${workload}" -v dataset="${dataset}" \
    -v file_size="${file_size}" -v block_size="${block_size}" \
    -v begin_marker="${begin_marker}" -v end_marker="${end_marker}" '
    function fail() { invalid=1 }
    { sub(/\r$/, "") }
    $0 == begin_marker {
      if (inside || begin_count++) fail()
      inside=1; next
    }
    $0 == end_marker {
      if (!inside || end_count++) fail()
      inside=0; next
    }
    inside && substr($0, 1, 7) == "result " {
      lines++
      n=split($0, fields, " ")
      for (i=2; i<=n; i++) {
        split_at=index(fields[i], "=")
        if (split_at <= 1) { fail(); continue }
        key=substr(fields[i], 1, split_at-1)
        value=substr(fields[i], split_at+1)
        if (++seen[key] != 1) fail()
        values[key]=value
      }
    }
    END {
      required_numeric[1]="errno"; required_numeric[2]="elapsed_us"
      required_numeric[3]="bytes"; required_numeric[4]="ops"
      required_numeric[5]="seed"; required_numeric[6]="files"
      required_numeric[7]="file_size"; required_numeric[8]="block_size"
      required_numeric[9]="iterations"; required_numeric[10]="workers"
      required_numeric[11]="syscalls"; required_numeric[12]="short_io"
      required_numeric[13]="eintr"
      for (i=1; i<=13; i++) {
        key=required_numeric[i]
        if (seen[key] != 1 || values[key] !~ /^[0-9]+$/) fail()
      }
      if (inside || begin_count != 1 || end_count != 1 || lines != 1 ||
          seen["workload"] != 1 || values["workload"] != workload ||
          seen["status"] != 1 || values["status"] != "ok" || values["errno"] != "0" ||
          seen["dataset"] != 1 || values["dataset"] != dataset ||
          values["file_size"] != file_size || values["block_size"] != block_size ||
          values["bytes"] != file_size || seen["checksum"] != 1 ||
          values["checksum"] !~ /^[0-9a-f]{16}$/) fail()
      if (invalid) exit 1
      print values["elapsed_us"] "\t" values["bytes"] "\t" values["ops"] "\t" \
            values["syscalls"] "\t" values["short_io"] "\t" values["eintr"] "\t" \
            values["checksum"]
    }
  ' "${path}"
}

parse_negotiated_config() {
  local path="$1" run_id="$2" case_id="$3" header
  IFS= read -r header <"${path}" || true
  [[ "${header}" == "P0_CONFIG_RUN:${run_id}:${case_id}" ]] || return 1
  local epoch max_read max_pages max_readahead async_read sg_pages effective_bytes expected_bytes
  epoch="$(stats_snapshot_value "${path}" fuse init_epoch)" || return 1
  max_read="$(stats_snapshot_value "${path}" fuse negotiated_max_read_bytes)" || return 1
  max_pages="$(stats_snapshot_value "${path}" fuse negotiated_max_pages)" || return 1
  max_readahead="$(stats_snapshot_value "${path}" fuse negotiated_max_readahead_bytes)" || return 1
  async_read="$(stats_snapshot_value "${path}" fuse negotiated_async_read)" || return 1
  effective_bytes="$(stats_snapshot_value "${path}" fuse effective_read_payload_limit_bytes)" || return 1
  sg_pages="$(stats_snapshot_value "${path}" virtiofs sg_limit_pages_configured)" || return 1
  ((epoch == 1 && max_read > 0 && max_pages > 0 && sg_pages > 0 && effective_bytes > 0)) || return 1
  [[ "${async_read}" == "0" || "${async_read}" == "1" ]] || return 1
  expected_bytes="${max_read}"
  ((max_pages * 4096 < expected_bytes)) && expected_bytes=$((max_pages * 4096))
  ((sg_pages * 4096 < expected_bytes)) && expected_bytes=$((sg_pages * 4096))
  ((effective_bytes == expected_bytes)) || return 1
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "${epoch}" "${max_read}" "${max_pages}" "${max_readahead}" "${async_read}" \
    "${sg_pages}" "${effective_bytes}"
}

read_amplification_valid() {
  local completed="$1" requested="$2"
  [[ "${completed}" =~ ^[0-9]+$ && "${requested}" =~ ^[0-9]+$ ]] || return 1
  ((requested >= completed && requested <= (completed * 5 + 3) / 4))
}

read_nul_argv() {
  local path="$1" output_name="$2" last_byte
  [[ -r "${path}" ]] || return 1
  last_byte="$(tail -c 1 -- "${path}" | od -An -tu1 | tr -d '[:space:]')"
  [[ "${last_byte}" == "0" ]] || return 1
  local -n output="${output_name}"
  mapfile -d '' -t output <"${path}"
  ((${#output[@]} > 0))
}

argv_value_after() {
  local option="$1"; shift
  local -a argv=("$@")
  local i
  for ((i = 0; i + 1 < ${#argv[@]}; i++)); do
    if [[ "${argv[i]}" == "${option}" ]]; then
      printf '%s\n' "${argv[i + 1]}"
      return 0
    fi
  done
  return 1
}

csv_option_value() {
  local csv="$1" key="$2" field
  IFS=',' read -r -a fields <<<"${csv}"
  for field in "${fields[@]}"; do
    if [[ "${field}" == "${key}="* ]]; then
      printf '%s\n' "${field#*=}"
      return 0
    fi
  done
  return 1
}

validate_cmdline_artifacts() {
  local qemu_path="$1" daemon_path="$2" manifest="$3" context_path="$4"
  local -a qemu_argv=() daemon_argv=()
  read_nul_argv "${qemu_path}" qemu_argv || die "qemu_cmdline is not a NUL-terminated argv artifact"
  read_nul_argv "${daemon_path}" daemon_argv || die "virtiofsd_cmdline is not a NUL-terminated argv artifact"
  jq -e '
    .schema == "dragonos.virtiofs.collector-process-context.v3" and
    (.host_boot_id | type == "string") and
    (.qemu.pid | type == "number") and (.qemu.start_ticks | type == "number") and
    (.qemu.exe | type == "string") and (.qemu.cwd | type == "string") and
    (.qemu.cmdline_sha256 | test("^[0-9a-f]{64}$")) and
    (.virtiofsd.pid | type == "number") and (.virtiofsd.start_ticks | type == "number") and
    (.virtiofsd.exe | type == "string") and (.virtiofsd.cwd | type == "string") and
    (.virtiofsd.cmdline_sha256 | test("^[0-9a-f]{64}$")) and
    (.virtiofsd.socket_path | startswith("/")) and
    (.virtiofsd.socket_inode | type == "number") and (.virtiofsd.socket_inode > 0) and
    (.binding.worker_pid | type == "number") and (.binding.worker_pid > 0) and
    (.binding.worker_start_ticks | type == "number") and
    (.binding.worker_exe | type == "string") and
    (.binding.worker_cmdline_sha256 | test("^[0-9a-f]{64}$")) and
    (.binding.peer_pairs | type == "array") and (.binding.peer_pairs | length > 0) and
    all(.binding.peer_pairs[];
      (.qemu_inode | type == "number") and (.qemu_inode > 0) and
      (.virtiofsd_inode | type == "number") and (.virtiofsd_inode > 0))
  ' "${context_path}" >/dev/null || die "collector process context is invalid"
  local qemu_pid daemon_pid qemu_cwd
  qemu_pid="$(jq -r '.qemu.pid' "${context_path}")"
  daemon_pid="$(jq -r '.virtiofsd.pid' "${context_path}")"
  qemu_cwd="$(jq -r '.qemu.cwd' "${context_path}")"

  local expected_qemu expected_daemon qemu_real daemon_real
  expected_qemu="$(jq -r '.artifacts.qemu' "${manifest}")"
  expected_daemon="$(jq -r '.artifacts.virtiofsd' "${manifest}")"
  qemu_real="$(realpath -e -- "${qemu_argv[0]}" 2>/dev/null || true)"
  daemon_real="$(realpath -e -- "${daemon_argv[0]}" 2>/dev/null || true)"
  [[ "${qemu_real}" == "$(realpath -e -- "${expected_qemu}")" ]] || die "qemu argv[0] differs from the planned binary"
  [[ "${daemon_real}" == "$(realpath -e -- "${expected_daemon}")" ]] || die "virtiofsd argv[0] differs from the planned binary"
  [[ "$(jq -r '.qemu.exe' "${context_path}")" == "${qemu_real}" ]] || die "qemu cmdline does not belong to the captured executable"
  [[ "$(jq -r '.virtiofsd.exe' "${context_path}")" == "${daemon_real}" ]] || die "virtiofsd cmdline does not belong to the captured executable"
  [[ "$(jq -r '.qemu.cmdline_sha256' "${context_path}")" == "$(sha256_or_unavailable "${qemu_path}")" &&
     "$(jq -r '.virtiofsd.cmdline_sha256' "${context_path}")" == "$(sha256_or_unavailable "${daemon_path}")" ]] || \
    die "collector process context is not bound to the sealed cmdline bytes"
  [[ "$(sha256_or_unavailable "${qemu_real}")" == "$(jq -r '.artifacts.qemu_sha256' "${manifest}")" ]] || die "qemu binary hash changed"
  [[ "$(sha256_or_unavailable "${daemon_real}")" == "$(jq -r '.artifacts.virtiofsd_sha256' "${manifest}")" ]] || die "virtiofsd binary hash changed"

  local kernel_arg drive_arg smp_arg expected_kernel expected_disk
  kernel_arg="$(argv_value_after -kernel "${qemu_argv[@]}" || true)"
  drive_arg="$(argv_value_after -drive "${qemu_argv[@]}" || true)"
  smp_arg="$(argv_value_after -smp "${qemu_argv[@]}" || true)"
  expected_kernel="$(jq -r '.artifacts.kernel' "${manifest}")"
  expected_disk="$(jq -r '.artifacts.disk_image' "${manifest}")"
  local kernel_real drive_real
  if [[ "${kernel_arg}" == /* ]]; then
    kernel_real="$(realpath -e -- "${kernel_arg}" 2>/dev/null || true)"
  else
    kernel_real="$(realpath -e -- "${qemu_cwd}/${kernel_arg}" 2>/dev/null || true)"
  fi
  [[ -n "${kernel_real}" && "${kernel_real}" == "$(realpath -e -- "${expected_kernel}")" ]] || die "qemu -kernel differs from the planned artifact"
  local drive_file
  drive_file="$(csv_option_value "${drive_arg}" file || true)"
  if [[ "${drive_file}" == /* ]]; then
    drive_real="$(realpath -e -- "${drive_file}" 2>/dev/null || true)"
  else
    drive_real="$(realpath -e -- "${qemu_cwd}/${drive_file}" 2>/dev/null || true)"
  fi
  [[ -n "${drive_real}" && "${drive_real}" == "$(realpath -e -- "${expected_disk}")" ]] || die "qemu -drive differs from the planned disk image"
  [[ "$(sha256_or_unavailable "${kernel_real}")" == "$(jq -r '.artifacts.kernel_sha256' "${manifest}")" ]] || \
    die "planned kernel hash changed"
  [[ "$(sha256_or_unavailable "${drive_real}")" == "$(jq -r '.artifacts.disk_image_sha256' "${manifest}")" ]] || \
    die "planned disk image hash changed"
  local memory_arg machine_arg expected_vcpus expected_memory expected_accel actual_vcpus
  memory_arg="$(argv_value_after -m "${qemu_argv[@]}" || true)"
  machine_arg="$(argv_value_after -machine "${qemu_argv[@]}" || true)"
  expected_vcpus="$(jq -r '.guest.vcpus' "${manifest}")"
  expected_memory="$(jq -r '.guest.memory' "${manifest}")"
  expected_accel="$(jq -r '.guest.accel' "${manifest}")"
  actual_vcpus="${smp_arg%%,*}"
  [[ "${actual_vcpus}" == "${expected_vcpus}" && "${memory_arg}" == "${expected_memory}" ]] || \
    die "qemu CPU or memory topology differs from the plan"
  [[ "$(csv_option_value "${machine_arg}" accel || true)" == "${expected_accel}" ]] || \
    die "qemu -machine does not use the planned acceleration"

  local value vhost_fs_devices=0 matching_virtiofs=0 device_chardev="" i
  for ((i = 1; i < ${#qemu_argv[@]}; i++)); do
    [[ "${qemu_argv[i]}" == "-device" ]] || continue
    ((i + 1 < ${#qemu_argv[@]})) || die "qemu -device lacks a value"
    value="${qemu_argv[i + 1]}"
    if [[ "${value}" == vhost-user-fs-pci,* ]]; then
      ((vhost_fs_devices += 1))
      if [[ "$(csv_option_value "${value}" tag || true)" == "hostshare" &&
            "${value}" != *cache-size=* ]]; then
        ((matching_virtiofs += 1))
        device_chardev="$(csv_option_value "${value}" chardev || true)"
      fi
    fi
    ((i += 1))
  done
  ((vhost_fs_devices == 1 && matching_virtiofs == 1)) || \
    die "qemu argv must contain exactly one expected non-DAX vhost-user-fs device"

  local planned_socket="" daemon_arg chardev_arg="" chardev_path="" matching_chardevs=0
  for daemon_arg in "${daemon_argv[@]}"; do
    [[ "${daemon_arg}" == --socket-path=* ]] && planned_socket="${daemon_arg#*=}"
  done
  [[ "$(jq -r '.virtiofsd.socket_path' "${context_path}")" == "${planned_socket}" ]] || \
    die "collector context is not bound to the planned virtiofsd socket"
  for ((i = 1; i < ${#qemu_argv[@]}; i++)); do
    [[ "${qemu_argv[i]}" == "-chardev" ]] || continue
    ((i + 1 < ${#qemu_argv[@]})) || die "qemu -chardev lacks a value"
    value="${qemu_argv[i + 1]}"
    if [[ "${value}" == socket,* && "$(csv_option_value "${value}" id || true)" == "${device_chardev}" ]]; then
      ((matching_chardevs += 1))
      chardev_arg="${value}"
      chardev_path="$(csv_option_value "${value}" path || true)"
    fi
    ((i += 1))
  done
  [[ -n "${device_chardev}" && "${matching_chardevs}" -eq 1 && -n "${chardev_arg}" &&
     "${chardev_path}" == "${planned_socket}" ]] || \
    die "qemu chardev is not uniquely bound to the planned virtiofsd socket"

  local actual_daemon_json planned_daemon_json
  actual_daemon_json="$(jq -cn '$ARGS.positional' --args -- "${daemon_argv[@]}")"
  planned_daemon_json="$(jq -c '.artifacts.virtiofsd_planned_argv' "${manifest}")"
  [[ "${actual_daemon_json}" == "${planned_daemon_json}" ]] || die "virtiofsd argv differs from the planned command"

  local boot_id planned_boot_id planned_uptime_ticks ticks role
  boot_id="$(jq -r '.host_boot_id' "${context_path}")"
  planned_boot_id="$(jq -r '.host.boot_id' "${manifest}")"
  [[ "${boot_id}" == "${planned_boot_id}" ]] || die "process boot identity differs from the plan"
  planned_uptime_ticks="$(jq -r '.host.plan_uptime_ticks' "${manifest}")"
  for role in qemu virtiofsd; do
    ticks="$(jq -r --arg role "${role}" '.[$role].start_ticks' "${context_path}")"
    ((ticks >= planned_uptime_ticks)) || die "${role} process predates this run; guest-cold evidence requires fresh processes"
  done
}

process_start_ticks() {
  local pid="$1" stat rest
  stat="$(<"/proc/${pid}/stat")"
  rest="${stat##*) }"
  set -- ${rest}
  (($# >= 20)) || return 1
  printf '%s\n' "${20}"
}

connected_unix_socket_pairs() {
  local qemu_pid="$1" daemon_pid="$2" fd target local_inode peer_inode key
  declare -A qemu_sockets=() daemon_sockets=() pairs=()
  for fd in "/proc/${qemu_pid}/fd/"*; do
    target="$(readlink "${fd}" 2>/dev/null || true)"
    [[ "${target}" =~ ^socket:\[([0-9]+)\]$ ]] && qemu_sockets["${BASH_REMATCH[1]}"]=1
  done
  for fd in "/proc/${daemon_pid}/fd/"*; do
    target="$(readlink "${fd}" 2>/dev/null || true)"
    [[ "${target}" =~ ^socket:\[([0-9]+)\]$ ]] && daemon_sockets["${BASH_REMATCH[1]}"]=1
  done
  local netid state recv_q send_q local_addr peer_addr process
  while read -r netid state recv_q send_q local_addr local_inode peer_addr peer_inode process; do
    [[ "${netid}" == "u_str" && "${state}" == "ESTAB" &&
       "${local_inode}" =~ ^[1-9][0-9]*$ && "${peer_inode}" =~ ^[1-9][0-9]*$ ]] || continue
    if [[ -n "${qemu_sockets[${local_inode}]:-}" && -n "${daemon_sockets[${peer_inode}]:-}" ]]; then
      pairs["${local_inode}:${peer_inode}"]=1
    elif [[ -n "${daemon_sockets[${local_inode}]:-}" && -n "${qemu_sockets[${peer_inode}]:-}" ]]; then
      pairs["${peer_inode}:${local_inode}"]=1
    fi
  done < <(ss -H -xnp)
  ((${#pairs[@]} > 0)) || return 1
  for key in "${!pairs[@]}"; do
    printf '%s %s\n' "${key%%:*}" "${key#*:}"
  done | sort -n -k1,1 -k2,2
}

find_virtiofs_peer_process() {
  local qemu_pid="$1" daemon_pid="$2" candidate pairs matches=0 selected_pid="" selected_pairs=""
  local -a candidates=("${daemon_pid}") children=()
  if [[ -r "/proc/${daemon_pid}/task/${daemon_pid}/children" ]]; then
    read -r -a children <"/proc/${daemon_pid}/task/${daemon_pid}/children"
    candidates+=("${children[@]}")
  fi
  for candidate in "${candidates[@]}"; do
    [[ -d "/proc/${candidate}" ]] || continue
    pairs="$(connected_unix_socket_pairs "${qemu_pid}" "${candidate}" || true)"
    [[ -n "${pairs}" ]] || continue
    ((matches += 1))
    selected_pid="${candidate}"
    selected_pairs="${pairs}"
  done
  ((matches == 1)) || return 1
  printf '%s\n%s\n' "${selected_pid}" "${selected_pairs}"
}

capture_process_context() {
  local qemu_source="$1" daemon_source="$2" output="$3"
  [[ "${qemu_source}" =~ ^/proc/([0-9]+)/cmdline$ ]] || \
    die "qemu_cmdline source must be a live /proc/PID/cmdline path"
  local qemu_pid="${BASH_REMATCH[1]}"
  [[ "${daemon_source}" =~ ^/proc/([0-9]+)/cmdline$ ]] || \
    die "virtiofsd_cmdline source must be a live /proc/PID/cmdline path"
  local daemon_pid="${BASH_REMATCH[1]}" qemu_exe daemon_exe qemu_cwd daemon_cwd
  local qemu_ticks qemu_ticks_after daemon_ticks daemon_ticks_after qemu_cmdline_sha daemon_cmdline_sha
  local daemon_socket="" daemon_socket_inode daemon_arg worker_pid worker_ticks worker_exe
  local worker_cmdline_sha peer_capture peer_pairs_text peer_pairs_json
  qemu_ticks="$(process_start_ticks "${qemu_pid}")" || die "cannot capture qemu start time"
  daemon_ticks="$(process_start_ticks "${daemon_pid}")" || die "cannot capture virtiofsd start time"
  qemu_exe="$(realpath -e -- "/proc/${qemu_pid}/exe")"
  daemon_exe="$(realpath -e -- "/proc/${daemon_pid}/exe")"
  qemu_cwd="$(realpath -e -- "/proc/${qemu_pid}/cwd")"
  daemon_cwd="$(realpath -e -- "/proc/${daemon_pid}/cwd")"
  qemu_cmdline_sha="$(sha256_or_unavailable "${qemu_source}")"
  daemon_cmdline_sha="$(sha256_or_unavailable "${daemon_source}")"
  local -a daemon_argv=()
  read_nul_argv "${daemon_source}" daemon_argv || die "cannot read live virtiofsd argv"
  for daemon_arg in "${daemon_argv[@]}"; do
    [[ "${daemon_arg}" == --socket-path=* ]] && daemon_socket="${daemon_arg#*=}"
  done
  [[ "${daemon_socket}" == /* ]] || die "live virtiofsd lacks an absolute socket path"
  daemon_socket_inode="$(virtiofs_socket_inode_for_process "${daemon_pid}" "${daemon_socket}" || true)"
  [[ "${daemon_socket_inode}" =~ ^[1-9][0-9]*$ ]] || \
    die "virtiofsd does not own the planned live Unix socket"
  peer_capture="$(find_virtiofs_peer_process "${qemu_pid}" "${daemon_pid}" || true)"
  worker_pid="${peer_capture%%$'\n'*}"
  peer_pairs_text="${peer_capture#*$'\n'}"
  [[ "${worker_pid}" =~ ^[1-9][0-9]*$ && -n "${peer_pairs_text}" ]] || \
    die "QEMU is not uniquely connected to the selected virtiofsd process"
  worker_ticks="$(process_start_ticks "${worker_pid}")" || die "cannot capture virtiofsd worker start time"
  worker_exe="$(realpath -e -- "/proc/${worker_pid}/exe")"
  worker_cmdline_sha="$(sha256_or_unavailable "/proc/${worker_pid}/cmdline")"
  [[ "${worker_exe}" == "${daemon_exe}" && "${worker_cmdline_sha}" == "${daemon_cmdline_sha}" ]] || \
    die "virtiofsd peer worker identity differs from the selected daemon"
  peer_pairs_json="$(printf '%s\n' "${peer_pairs_text}" | jq -Rn \
    '[inputs | split(" ") | {qemu_inode:(.[0]|tonumber),virtiofsd_inode:(.[1]|tonumber)}]')"
  qemu_ticks_after="$(process_start_ticks "${qemu_pid}")" || die "qemu exited during context capture"
  daemon_ticks_after="$(process_start_ticks "${daemon_pid}")" || \
    die "virtiofsd exited during context capture"
  [[ "${qemu_ticks}" == "${qemu_ticks_after}" && "${daemon_ticks}" == "${daemon_ticks_after}" ]] || \
    die "process identity changed during context capture"
  [[ "$(virtiofs_socket_inode_for_process "${daemon_pid}" "${daemon_socket}" || true)" == \
     "${daemon_socket_inode}" ]] || die "virtiofsd socket ownership changed during context capture"
  [[ "$(find_virtiofs_peer_process "${qemu_pid}" "${daemon_pid}" || true)" == "${peer_capture}" &&
     "$(process_start_ticks "${worker_pid}" || true)" == "${worker_ticks}" ]] || \
    die "QEMU/virtiofsd peer binding changed during context capture"
  jq -n --arg schema "dragonos.virtiofs.collector-process-context.v3" \
    --arg boot_id "$(tr -d '\n' </proc/sys/kernel/random/boot_id)" \
    --argjson qemu_pid "${qemu_pid}" --argjson qemu_ticks "${qemu_ticks}" \
    --arg qemu_exe "${qemu_exe}" --arg qemu_cwd "${qemu_cwd}" \
    --arg qemu_cmdline_sha "${qemu_cmdline_sha}" \
    --argjson daemon_pid "${daemon_pid}" --argjson daemon_ticks "${daemon_ticks}" \
    --arg daemon_exe "${daemon_exe}" --arg daemon_cwd "${daemon_cwd}" \
    --arg daemon_cmdline_sha "${daemon_cmdline_sha}" \
    --arg daemon_socket "${daemon_socket}" --argjson daemon_socket_inode "${daemon_socket_inode}" \
    --argjson worker_pid "${worker_pid}" --argjson worker_ticks "${worker_ticks}" \
    --arg worker_exe "${worker_exe}" --arg worker_cmdline_sha "${worker_cmdline_sha}" \
    --argjson peer_pairs "${peer_pairs_json}" \
    '{schema:$schema,host_boot_id:$boot_id,
      qemu:{pid:$qemu_pid,start_ticks:$qemu_ticks,exe:$qemu_exe,cwd:$qemu_cwd,
            cmdline_sha256:$qemu_cmdline_sha},
      virtiofsd:{pid:$daemon_pid,start_ticks:$daemon_ticks,exe:$daemon_exe,cwd:$daemon_cwd,
                 cmdline_sha256:$daemon_cmdline_sha,socket_path:$daemon_socket,
                 socket_inode:$daemon_socket_inode},
      binding:{worker_pid:$worker_pid,worker_start_ticks:$worker_ticks,
               worker_exe:$worker_exe,worker_cmdline_sha256:$worker_cmdline_sha,
               peer_pairs:$peer_pairs}}' \
    >"${output}"
}

verify_capture_sources_stable() {
  local staging_dir="$1" artifacts_name="$2" fingerprints_name="$3"
  local -n originals="${artifacts_name}" fingerprints="${fingerprints_name}"
  local artifact_index original captured fingerprint name
  for artifact_index in "${!originals[@]}"; do
    original="${originals[artifact_index]#*=}"
    name="${originals[artifact_index]%%=*}"
    captured="${staging_dir}/${name}"
    fingerprint="${fingerprints[artifact_index]}"
    [[ -f "${original}" && ! -L "${original}" &&
       "$(stat -Lc '%d:%i:%s:%y:%z' -- "${original}")" == "${fingerprint}" &&
       "$(sha256_or_unavailable "${original}")" == "$(sha256_or_unavailable "${captured}")" ]] || \
      die "artifact changed before capture was sealed: ${name}"
  done
}

canonical_existing_dir() {
  local path="$1"
  [[ -d "${path}" ]] || die "directory does not exist: ${path}"
  realpath -e -- "${path}"
}

validate_dataset_name() {
  local path="$1"
  [[ "${path}" =~ ^[A-Za-z0-9._-]{1,128}$ ]] || \
    die "dataset must be one safe component (letters, digits, '.', '_' or '-')"
  [[ "${path}" != "." && "${path}" != ".." ]] || die "invalid dataset name: ${path}"
}

validate_csv_numbers() {
  local value="$1" label="$2" maximum="$3" item
  declare -A seen=()
  IFS=',' read -r -a items <<<"${value}"
  ((${#items[@]} > 0)) || die "empty ${label} matrix"
  for item in "${items[@]}"; do
    [[ "${item}" =~ ^[1-9][0-9]*$ ]] || die "invalid ${label}: ${item}"
    if ((${#item} > ${#maximum})) || \
       { ((${#item} == ${#maximum})) && [[ "${item}" > "${maximum}" ]]; }; then
      die "${label} exceeds helper limit ${maximum}: ${item}"
    fi
    [[ -z "${seen[${item}]:-}" ]] || die "duplicate ${label}: ${item}"
    seen["${item}"]=1
  done
}

write_host_facts() {
  local out="$1" share_dir="$2"
  {
    printf 'host_uname='; uname -a
    printf 'host_filesystem=%s\n' "$(stat -f -c '%T' -- "${share_dir}" 2>/dev/null || printf 'unknown')"
    printf 'host_load='; cat /proc/loadavg 2>/dev/null || printf 'unavailable\n'
    printf 'ss_version='; ss -V 2>&1 || printf 'unavailable\n'
    printf 'cpu_online='; cat /sys/devices/system/cpu/online 2>/dev/null || printf 'unavailable\n'
    printf 'cpu_governors='
    local governor first=1
    for governor in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do
      [[ -r "${governor}" ]] || continue
      ((first == 1)) || printf ','
      first=0
      tr -d '\n' <"${governor}"
    done
    printf '\n'
    printf 'numa_online='; cat /sys/devices/system/node/online 2>/dev/null || printf 'unavailable\n'
  } >"${out}"
}

validate_build_manifest() {
  local manifest="$1" helper_guest_path="$2"
  [[ -f "${manifest}" && ! -L "${manifest}" ]] || die "build manifest must be a regular non-symlink file"
  jq -e '
    .schema == "dragonos.virtiofs.build-manifest.v1" and
    .repo.clean == true and (.repo.commit | test("^[0-9a-f]{40}$")) and
    (.repo.tree | test("^[0-9a-f]{40}$")) and
    (.build.commands | type == "array" and length > 0) and
    (.build.toolchain_fingerprint_sha256 | test("^[0-9a-f]{64}$")) and
    ([.artifacts.kernel,.artifacts.disk_image,.artifacts.guest_helper] | all(
      (.sha256 | test("^[0-9a-f]{64}$")) and (.size | type == "number" and . > 0)))
  ' "${manifest}" >/dev/null || die "build manifest schema is invalid"
  [[ -z "$(git -C "${REPO_ROOT}" status --porcelain=v1 --untracked-files=all)" ]] || \
    die "formal evidence plan requires the same clean tree used by the build manifest"
  [[ "$(jq -r '.repo.commit' "${manifest}")" == "$(git -C "${REPO_ROOT}" rev-parse HEAD)" &&
     "$(jq -r '.repo.tree' "${manifest}")" == "$(git -C "${REPO_ROOT}" rev-parse 'HEAD^{tree}')" ]] || \
    die "build manifest repository identity differs from the current clean tree"
  [[ "$(jq -r '.artifacts.kernel.path' "${manifest}")" == "bin/kernel/kernel.elf" &&
     "$(jq -r '.artifacts.disk_image.path' "${manifest}")" == "bin/disk-image-x86_64.img" &&
     "$(jq -r '.artifacts.guest_helper.host_path' "${manifest}")" == "bin/sysroot/bin/virtiofs_bench" &&
     "$(jq -r '.artifacts.guest_helper.guest_path' "${manifest}")" == "${helper_guest_path}" ]] || \
    die "build manifest artifact paths differ from the runner contract"
  local key relative path
  for key in kernel disk_image guest_helper; do
    if [[ "${key}" == "guest_helper" ]]; then
      relative="$(jq -r '.artifacts.guest_helper.host_path' "${manifest}")"
    else
      relative="$(jq -r ".artifacts.${key}.path" "${manifest}")"
    fi
    path="${REPO_ROOT}/${relative}"
    [[ -f "${path}" && ! -L "${path}" &&
       "$(sha256_or_unavailable "${path}")" == "$(jq -r ".artifacts.${key}.sha256" "${manifest}")" &&
       "$(stat -c '%s' -- "${path}")" == "$(jq -r ".artifacts.${key}.size" "${manifest}")" ]] || \
      die "build artifact differs from manifest: ${key}"
  done
}

plan_run() {
  local mode="light" phase="read" profile="quick"
  local share_dir="" dataset="non-dax-p0"
  local evidence_root="${REPO_ROOT}/bin/virtiofs-evidence"
  local guest_mount="/tmp/host" helper="/bin/virtiofs_bench"
  local build_manifest=""
  local block_sizes="" file_sizes="" timeout_seconds=""
  local guest_cache="cold" host_cache="unknown"
  local vcpus="2" memory="2G" accel="kvm"

  while (($#)); do
    case "$1" in
      --mode) mode="${2:?missing --mode value}"; shift 2 ;;
      --phase) phase="${2:?missing --phase value}"; shift 2 ;;
      --profile) profile="${2:?missing --profile value}"; shift 2 ;;
      --share-dir) share_dir="${2:?missing --share-dir value}"; shift 2 ;;
      --dataset) dataset="${2:?missing --dataset value}"; shift 2 ;;
      --evidence-root) evidence_root="${2:?missing --evidence-root value}"; shift 2 ;;
      --guest-mount) guest_mount="${2:?missing --guest-mount value}"; shift 2 ;;
      --helper) helper="${2:?missing --helper value}"; shift 2 ;;
      --build-manifest) build_manifest="${2:?missing --build-manifest value}"; shift 2 ;;
      --block-sizes) block_sizes="${2:?missing --block-sizes value}"; shift 2 ;;
      --file-sizes) file_sizes="${2:?missing --file-sizes value}"; shift 2 ;;
      --timeout) timeout_seconds="${2:?missing --timeout value}"; shift 2 ;;
      --vcpus) vcpus="${2:?missing --vcpus value}"; shift 2 ;;
      --memory) memory="${2:?missing --memory value}"; shift 2 ;;
      --accel) accel="${2:?missing --accel value}"; shift 2 ;;
      --guest-cache) guest_cache="${2:?missing --guest-cache value}"; shift 2 ;;
      --host-cache) host_cache="${2:?missing --host-cache value}"; shift 2 ;;
      -h|--help) usage; exit 0 ;;
      *) die "unknown plan option: $1" ;;
    esac
  done

  [[ "${mode}" == "performance" || "${mode}" == "light" || "${mode}" == "diagnostic" ]] || \
    die "invalid mode: ${mode}"
  [[ "${phase}" == "prepare" || "${phase}" == "read" ]] || die "invalid phase: ${phase}"
  [[ "${profile}" == "quick" || "${profile}" == "full" ]] || die "invalid profile: ${profile}"
  [[ "${guest_cache}" == "cold" || "${guest_cache}" == "warm" ]] || die "invalid guest cache label"
  [[ "${host_cache}" == "warm" || "${host_cache}" == "unknown" ]] || \
    die "host cache must be warm or unknown; this runner cannot prove host-cold state"
  if [[ "${guest_cache}" == "warm" && "${host_cache}" != "warm" ]]; then
    die "guest warm preheat also warms the backend path; label host cache as warm"
  fi
  if [[ "${guest_cache}" == "warm" && "${mode}" != "performance" ]]; then
    die "guest-warm cases are performance-only; READ-structure evidence requires a fresh guest"
  fi
  [[ "${guest_mount}" == /* && "${helper}" == /* ]] || die "guest mount and helper must be absolute"
  [[ -n "${build_manifest}" ]] || die "--build-manifest is required for formal evidence"
  build_manifest="$(realpath -e -- "${build_manifest}")"
  local build_manifest_input_sha
  build_manifest_input_sha="$(sha256_or_unavailable "${build_manifest}")"
  validate_build_manifest "${build_manifest}" "${helper}"
  [[ "$(sha256_or_unavailable "${build_manifest}")" == "${build_manifest_input_sha}" ]] || \
    die "build manifest changed while it was being validated"
  [[ "${timeout_seconds:-1}" =~ ^[1-9][0-9]*$ ]] || die "timeout must be a positive integer"
  [[ "${vcpus}" =~ ^[1-9][0-9]*$ ]] || die "vcpus must be a positive integer"
  [[ "${memory}" =~ ^[1-9][0-9]*[KMGTP]?$ ]] || die "memory must be a QEMU size"
  [[ "${accel}" == "kvm" || "${accel}" == "tcg" ]] || die "accel must be kvm or tcg"
  validate_dataset_name "${dataset}"

  local env_file="${DRAGONOS_VIRTIOFS_ENV_FILE:-${SCRIPT_DIR}/env.sh}"
  if [[ -f "${env_file}" ]]; then
    # shellcheck source=/dev/null
    source "${env_file}"
  fi
  if [[ -z "${share_dir}" ]]; then
    share_dir="${HOST_SHARE_DIR:-}"
  fi
  [[ -n "${share_dir}" ]] || die "set --share-dir or configure HOST_SHARE_DIR in tools/virtiofs/env.sh"
  share_dir="$(canonical_existing_dir "${share_dir}")"
  evidence_root="$(mkdir -p -- "${evidence_root}" && canonical_existing_dir "${evidence_root}")"

  if [[ -z "${block_sizes}" ]]; then
    # A guest-cold sample is only meaningful when every case starts in a
    # fresh VM. Keep the built-in profiles to one case; callers can request a
    # warm-cache matrix explicitly or create one cold run per block size.
    [[ "${profile}" == "quick" ]] && block_sizes="4096" || block_sizes="131072"
  fi
  if [[ -z "${file_sizes}" ]]; then
    [[ "${profile}" == "quick" ]] && file_sizes="1048576" || file_sizes="16777216"
  fi
  if [[ -z "${timeout_seconds}" ]]; then
    [[ "${profile}" == "quick" ]] && timeout_seconds="30" || timeout_seconds="180"
  fi
  validate_csv_numbers "${block_sizes}" "block size" 16777216
  validate_csv_numbers "${file_sizes}" "file size" 1073741824
  [[ "${file_sizes}" != *,* ]] || \
    die "one evidence run accepts exactly one file size; use separate prepared datasets"
  if [[ "${guest_cache}" == "cold" && "${block_sizes}" == *,* ]]; then
    die "guest-cold evidence accepts one block size per fresh VM; create separate runs"
  fi

  local dataset_dir_host="${share_dir}/.virtiofs_bench_${dataset}"
  local dataset_host="${dataset_dir_host}/seq.dat"
  local dataset_manifest_host="${dataset_dir_host}/manifest.v1"
  case "$(realpath -m -- "${dataset_host}")" in
    "${share_dir}"/*) ;;
    *) die "dataset resolves outside host share" ;;
  esac
  if [[ "${phase}" == "read" ]]; then
    [[ -f "${dataset_host}" && ! -L "${dataset_host}" ]] || \
      die "read phase requires an existing non-symlink dataset: ${dataset_host}"
    [[ -f "${dataset_manifest_host}" && ! -L "${dataset_manifest_host}" ]] || \
      die "read phase requires the prepare manifest: ${dataset_manifest_host}"
  fi

  local kernel="${REPO_ROOT}/bin/kernel/kernel.elf"
  local disk_image="${REPO_ROOT}/bin/disk-image-x86_64.img"
  local guest_helper_host="${REPO_ROOT}/bin/sysroot/bin/virtiofs_bench"
  local qemu_bin="${DRAGONOS_VIRTIOFS_QEMU_BIN:-$(command -v qemu-system-x86_64 || true)}"
  local virtiofsd_bin
  virtiofsd_bin="$(virtiofs_detect_daemon || true)"
  [[ -f "${kernel}" ]] || die "kernel artifact does not exist: ${kernel}"
  [[ -f "${disk_image}" ]] || die "disk image does not exist: ${disk_image}"
  [[ -x "${qemu_bin:-}" ]] || die "QEMU binary is unavailable; set DRAGONOS_VIRTIOFS_QEMU_BIN"
  [[ -x "${virtiofsd_bin:-}" ]] || die "virtiofsd binary is unavailable; set VIRTIOFSD_BIN"

  virtiofs_build_daemon_command "${virtiofsd_bin}" "${SOCKET_PATH:-/tmp/dragonos-virtiofsd.sock}" \
    "${share_dir}" "${VIRTIOFSD_CACHE:-auto}" "${VIRTIOFSD_EXTRA_ARGS:-}"
  local planned_socket_path="${SOCKET_PATH:-/tmp/dragonos-virtiofsd.sock}"
  [[ "${planned_socket_path}" == /* ]] || die "virtiofsd socket path must be absolute"
  [[ ! "${planned_socket_path}" =~ [[:space:]] ]] || \
    die "virtiofsd socket path must not contain whitespace"
  local virtiofsd_planned_argv_json
  virtiofsd_planned_argv_json="$(jq -n '$ARGS.positional' --args -- "${VIRTIOFSD_COMMAND[@]}")"

  local run_id run_dir
  run_id="$(date -u +'%Y%m%dT%H%M%SZ')-$$-$(printf '%s' "${RANDOM}" | sha256sum | cut -c1-8)"
  run_dir="${evidence_root}/${run_id}"
  (umask 077 && mkdir -- "${run_dir}") || die "cannot create unique run directory: ${run_dir}"
  mkdir -- "${run_dir}/cases"

  write_host_facts "${run_dir}/host-facts.txt" "${share_dir}"
  git -C "${REPO_ROOT}" status --short >"${run_dir}/git-status.txt"
  [[ ! -s "${run_dir}/git-status.txt" ]] || die "formal evidence cannot seal a dirty tree"
  cp -- "${build_manifest}" "${run_dir}/build-manifest.json"
  [[ "$(sha256_or_unavailable "${run_dir}/build-manifest.json")" == "${build_manifest_input_sha}" ]] || \
    die "build manifest changed while it was being sealed"

  # validate_build_manifest performed the first identity check. Hash each
  # artifact twice again immediately before publishing manifest.json, and
  # require both observations to equal the trusted build manifest.
  local kernel_sha disk_image_sha guest_helper_sha
  kernel_sha="$(stable_manifest_artifact_sha256 "${kernel}" \
    "$(jq -r '.artifacts.kernel.sha256' "${run_dir}/build-manifest.json")" kernel)"
  disk_image_sha="$(stable_manifest_artifact_sha256 "${disk_image}" \
    "$(jq -r '.artifacts.disk_image.sha256' "${run_dir}/build-manifest.json")" disk_image)"
  guest_helper_sha="$(stable_manifest_artifact_sha256 "${guest_helper_host}" \
    "$(jq -r '.artifacts.guest_helper.sha256' "${run_dir}/build-manifest.json")" guest_helper)"

  local dataset_sha="unavailable" dataset_manifest_sha="unavailable"
  if [[ -f "${dataset_host}" ]]; then
    dataset_sha="$(sha256_or_unavailable "${dataset_host}")"
  fi
  if [[ -f "${dataset_manifest_host}" ]]; then
    dataset_manifest_sha="$(sha256_or_unavailable "${dataset_manifest_host}")"
  fi

  local created_utc created_epoch host_boot_id plan_uptime_ticks
  created_utc="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
  created_epoch="$(date -u +%s)"
  host_boot_id="$(tr -d '\n' </proc/sys/kernel/random/boot_id)"
  plan_uptime_ticks="$(awk -v hz="$(getconf CLK_TCK)" '{printf "%.0f", $1 * hz}' /proc/uptime)"
  jq -n \
    --arg schema "dragonos.virtiofs.non-dax-run.v2" \
    --arg runner_version "${RUNNER_VERSION}" \
    --arg run_id "${run_id}" \
    --arg created_utc "${created_utc}" --arg created_epoch "${created_epoch}" \
    --arg host_boot_id "${host_boot_id}" \
    --arg plan_uptime_ticks "${plan_uptime_ticks}" \
    --arg repo_commit "$(git -C "${REPO_ROOT}" rev-parse HEAD)" \
    --arg repo_tree "$(git -C "${REPO_ROOT}" rev-parse 'HEAD^{tree}')" \
    --arg build_manifest_sha256 "$(sha256_or_unavailable "${run_dir}/build-manifest.json")" \
    --arg mode "${mode}" --arg phase "${phase}" --arg profile "${profile}" \
    --arg guest_cache "${guest_cache}" --arg host_cache "${host_cache}" \
    --arg share_dir "${share_dir}" --arg dataset "${dataset}" \
    --arg dataset_sha256 "${dataset_sha}" --arg dataset_manifest_sha256 "${dataset_manifest_sha}" \
    --arg kernel "${kernel}" --arg kernel_sha256 "${kernel_sha}" \
    --arg disk_image "${disk_image}" --arg disk_image_sha256 "${disk_image_sha}" \
    --arg guest_helper_host "${guest_helper_host}" --arg guest_helper_sha256 "${guest_helper_sha}" \
    --arg qemu_bin "${qemu_bin:-unavailable}" --arg qemu_sha256 "$(sha256_or_unavailable "${qemu_bin:-}")" \
    --arg qemu_version "$(version_line "${qemu_bin:-}")" \
    --arg virtiofsd_bin "${virtiofsd_bin:-unavailable}" \
    --arg virtiofsd_sha256 "$(sha256_or_unavailable "${virtiofsd_bin:-}")" \
    --arg virtiofsd_version "$(version_line "${virtiofsd_bin:-}")" \
    --argjson virtiofsd_planned_argv "${virtiofsd_planned_argv_json}" \
    --arg guest_mount "${guest_mount}" --arg helper "${helper}" \
    --arg vcpus "${vcpus}" --arg memory "${memory}" --arg accel "${accel}" \
    --arg timeout_seconds "${timeout_seconds}" \
    --arg qemu_argv_status "manual-capture-required" \
    '{schema:$schema, runner_version:$runner_version, run_id:$run_id,
      created_utc:$created_utc, created_epoch:($created_epoch|tonumber), runner_argv:$ARGS.positional,
      repo:{commit:$repo_commit,tree:$repo_tree,build_manifest_sha256:$build_manifest_sha256}, host:{boot_id:$host_boot_id,plan_uptime_ticks:($plan_uptime_ticks|tonumber)}, mode:$mode, phase:$phase, profile:$profile,
      cache:{guest:$guest_cache,host:$host_cache},
      dataset:{share_dir:$share_dir,path:$dataset,sha256:$dataset_sha256,
        manifest_sha256:$dataset_manifest_sha256},
      artifacts:{kernel:$kernel,kernel_sha256:$kernel_sha256,
        disk_image:$disk_image,disk_image_sha256:$disk_image_sha256,
        guest_helper_host:$guest_helper_host,guest_helper_sha256:$guest_helper_sha256,
        qemu:$qemu_bin,qemu_sha256:$qemu_sha256,qemu_version:$qemu_version,
        virtiofsd:$virtiofsd_bin,virtiofsd_sha256:$virtiofsd_sha256,
        virtiofsd_version:$virtiofsd_version,virtiofsd_planned_argv:$virtiofsd_planned_argv},
      guest:{mount:$guest_mount,helper:$helper,vcpus:($vcpus|tonumber),memory:$memory,accel:$accel}, timeout_seconds:($timeout_seconds|tonumber),
      qemu_argv_status:$qemu_argv_status}' \
    --args -- "${ORIGINAL_ARGV[@]}" >"${run_dir}/manifest.json"

  printf 'case_id\tmode\tphase\tfile_size\tblock_size\tguest_cache\thost_cache\n' \
    >"${run_dir}/case-matrix.tsv"
  local file_size block_size case_id
  if [[ "${phase}" == "prepare" ]]; then
    file_size="${file_sizes%%,*}"
    block_size="${block_sizes%%,*}"
    printf 'prepare-f%s-b%s\t%s\tprepare\t%s\t%s\t%s\t%s\n' \
      "${file_size}" "${block_size}" "${mode}" "${file_size}" "${block_size}" \
      "${guest_cache}" "${host_cache}" >>"${run_dir}/case-matrix.tsv"
  else
    IFS=',' read -r -a files <<<"${file_sizes}"
    IFS=',' read -r -a blocks <<<"${block_sizes}"
    for file_size in "${files[@]}"; do
      for block_size in "${blocks[@]}"; do
        case_id="read-f${file_size}-b${block_size}"
        printf '%s\t%s\tread\t%s\t%s\t%s\t%s\n' "${case_id}" "${mode}" \
          "${file_size}" "${block_size}" "${guest_cache}" "${host_cache}" \
          >>"${run_dir}/case-matrix.tsv"
      done
    done
  fi

  {
    printf '# Execute manually in the DragonOS serial console. Do not source this file.\n'
    printf '# First confirm the new split-workload CLI; failure is terminal.\n'
    printf '%q --help 2>&1 | grep -q sequential_read && %q --help 2>&1 | grep -q -- --path || { echo P0_HELPER_CLI_UNSUPPORTED; false; }\n' \
      "${helper}" "${helper}"
    printf 'mkdir -p %q\n' "${guest_mount}"
    printf 'mount | grep -F %q >/dev/null 2>&1 || mount -t virtiofs hostshare %q\n' \
      " on ${guest_mount} type virtiofs " "${guest_mount}"
    printf 'echo "P0_MOUNT:%s:$(mount | grep -F %q)"\n' "${run_id}" \
      " on ${guest_mount} type virtiofs "
    printf 'mkdir -p /tmp/dbg\n'
    printf 'mount | grep -F %q >/dev/null 2>&1 || mount -t debugfs debugfs /tmp/dbg\n' \
      " on /tmp/dbg type debugfs "
    printf 'test -r /tmp/dbg/fuse/stats_mode || { echo P0_STATS_MODE_UNAVAILABLE; false; }\n'
    if [[ "${mode}" == "diagnostic" ]]; then
      printf 'test -r /tmp/dbg/fuse/stats || { echo P0_STATS_UNAVAILABLE; false; }\n'
      printf '%s\n' "printf 'detailed\\n' 1<> /tmp/dbg/fuse/stats_mode"
      printf 'test "$(cat /tmp/dbg/fuse/stats_mode)" = detailed || { echo P0_STATS_MODE_FAILED; false; }\n'
      printf 'echo P0_STATS_MODE:detailed:run_id=%s\n' "${run_id}"
    elif [[ "${mode}" == "light" ]]; then
      printf 'test -r /tmp/dbg/fuse/stats || { echo P0_STATS_UNAVAILABLE; false; }\n'
      printf '%s\n' "printf 'light\\n' 1<> /tmp/dbg/fuse/stats_mode"
      printf 'test "$(cat /tmp/dbg/fuse/stats_mode)" = light || { echo P0_STATS_MODE_FAILED; false; }\n'
      printf 'echo P0_STATS_MODE:light:run_id=%s\n' "${run_id}"
    else
      printf '%s\n' "printf 'off\\n' 1<> /tmp/dbg/fuse/stats_mode"
      printf 'test "$(cat /tmp/dbg/fuse/stats_mode)" = off || { echo P0_STATS_MODE_FAILED; false; }\n'
      printf 'echo P0_STATS_MODE:off:run_id=%s\n' "${run_id}"
    fi
    if [[ "${phase}" == "read" ]]; then
      printf 'test -f %q && test -f %q || { echo P0_DATASET_MISSING; false; }\n' \
        "${guest_mount}/.virtiofs_bench_${dataset}/seq.dat" \
        "${guest_mount}/.virtiofs_bench_${dataset}/manifest.v1"
    fi
    tail -n +2 "${run_dir}/case-matrix.tsv" | while IFS=$'\t' read -r case_id _ case_phase file_size block_size _; do
      if [[ "${case_phase}" == "read" && "${guest_cache}" == "warm" ]]; then
        printf 'echo P0_WARMUP_BEGIN:%s:run_id=%s\n' "${case_id}" "${run_id}"
        printf 'VIRTIOFS_STATS_PATH= VIRTIOFS_QUIESCENCE_PATH=/tmp/dbg/fuse/stats VIRTIOFS_BENCH_RUN_ID=%q ' \
          "${run_id}-warmup-${case_id}"
        printf '%q --mount %q --workload sequential_read --path %q --file-size %q --block-size %q\n' \
          "${helper}" "${guest_mount}" "${dataset}" "${file_size}" "${block_size}"
        printf 'warmup_rc=$?; echo P0_WARMUP_END:%s:run_id=%s:rc=$warmup_rc; test "$warmup_rc" -eq 0\n' \
          "${case_id}" "${run_id}"
      fi
      printf '{ echo P0_CONFIG_RUN:%s:%s; cat /tmp/dbg/fuse/stats; } > %q\n' \
        "${run_id}" "${case_id}" \
        "${guest_mount}/.virtiofs_bench_config_${run_id}_${case_id}.txt"
      printf 'echo P0_CASE_BEGIN:%s:run_id=%s\n' "${case_id}" "${run_id}"
      if [[ "${mode}" == "performance" ]]; then
        printf 'VIRTIOFS_STATS_PATH= VIRTIOFS_QUIESCENCE_PATH=/tmp/dbg/fuse/stats VIRTIOFS_BENCH_RUN_ID=%q ' "${run_id}"
      else
        printf 'VIRTIOFS_STATS_PATH=/tmp/dbg/fuse/stats VIRTIOFS_QUIESCENCE_PATH=/tmp/dbg/fuse/stats VIRTIOFS_BENCH_RUN_ID=%q ' "${run_id}"
      fi
      if [[ "${case_phase}" == "prepare" ]]; then
        printf '%q --mount %q --workload prepare --path %q --file-size %q --block-size %q\n' \
          "${helper}" "${guest_mount}" "${dataset}" "${file_size}" "${block_size}"
      else
        printf '%q --mount %q --workload sequential_read --path %q --file-size %q --block-size %q\n' \
          "${helper}" "${guest_mount}" "${dataset}" "${file_size}" "${block_size}"
      fi
      printf 'p0_rc=$?; echo P0_CASE_END:%s:run_id=%s:rc=$p0_rc\n' "${case_id}" "${run_id}"
      if [[ "${mode}" != "performance" ]]; then
        printf '{ echo P0_STATS_RUN:%s:%s; cat /tmp/dbg/fuse/stats; } > %q\n' \
          "${run_id}" "${case_id}" \
          "${guest_mount}/.virtiofs_bench_stats_${run_id}_${case_id}.txt"
      fi
      printf 'test "$p0_rc" -eq 0\n'
    done
  } >"${run_dir}/guest-commands.sh"
  chmod 0400 "${run_dir}/guest-commands.sh"

  local runner_quoted run_dir_quoted stats_example completed_stats_note
  printf -v runner_quoted '%q' "${SCRIPT_DIR}/non_dax_bench_runner.sh"
  printf -v run_dir_quoted '%q' "${run_dir}"
  stats_example=''
  completed_stats_note='Performance completed cases do not require a stats artifact.'
  if [[ "${mode}" != "performance" ]]; then
    stats_example=$' \\\n    --artifact stats=/path/to/stats.txt'
    completed_stats_note="${mode^} completed cases must capture 'cat /tmp/dbg/fuse/stats' after the case."
  fi
  cat >"${run_dir}/MANUAL-STAGE.txt" <<EOF
This runner does not type into the QEMU serial console. That transport is not reliable enough
to claim automated execution. Start virtiofsd and a fresh DragonOS VM manually, capture the
actual QEMU and virtiofsd /proc/PID/cmdline files, then execute guest-commands.sh line by line.

The watchdog is ${timeout_seconds}s and is intentionally manual: on timeout, capture GDB CPU
backtraces and stats before sending a signal. Preserve serial_opt.txt and daemon logs. Record:

  ${runner_quoted} collect --run-dir ${run_dir_quoted} \\
    --case CASE_ID --status completed --artifact serial=/path/to/serial_opt.txt \\
    --artifact qemu_cmdline=/proc/QEMU_PID/cmdline --artifact virtiofsd_cmdline=/proc/VIRTIOFSD_PID/cmdline \\
    --artifact config=/path/to/.virtiofs_bench_config_RUN_ID_CASE_ID.txt${stats_example}

Every completed mode requires the independent config snapshot written before its workload.
${completed_stats_note} Collection verifies helper result fields, case markers, stats mode, and
the prepared host dataset hashes before accepting completed.

For timeout status, non-empty gdb, serial, and stats artifacts are mandatory. Finalize only after every case:

  ${runner_quoted} finalize --run-dir ${run_dir_quoted}
EOF

  cp -- "${BASH_SOURCE[0]}" "${run_dir}/runner.sh"
  cp -- "${SCRIPT_DIR}/common.sh" "${run_dir}/common.sh"
  (cd "${run_dir}" && sha256sum -- manifest.json build-manifest.json case-matrix.tsv guest-commands.sh \
    host-facts.txt git-status.txt MANUAL-STAGE.txt runner.sh common.sh >plan.sha256)
  chmod 0400 "${run_dir}/manifest.json" "${run_dir}/build-manifest.json" "${run_dir}/case-matrix.tsv" \
    "${run_dir}/host-facts.txt" "${run_dir}/git-status.txt" \
    "${run_dir}/MANUAL-STAGE.txt" "${run_dir}/runner.sh" "${run_dir}/common.sh" \
    "${run_dir}/plan.sha256"

  printf 'run_dir=%s\n' "${run_dir}"
  printf 'manual_stage=%s\n' "${run_dir}/MANUAL-STAGE.txt"
  printf 'guest_commands=%s\n' "${run_dir}/guest-commands.sh"
}

collect_case() {
  local run_dir="" case_id="" status="" spec name source destination verify_only=0
  local -a artifacts=()
  while (($#)); do
    case "$1" in
      --verify-only) verify_only=1; shift ;;
      --run-dir) run_dir="${2:?missing --run-dir value}"; shift 2 ;;
      --case) case_id="${2:?missing --case value}"; shift 2 ;;
      --status) status="${2:?missing --status value}"; shift 2 ;;
      --artifact) artifacts+=("${2:?missing --artifact value}"); shift 2 ;;
      -h|--help) usage; exit 0 ;;
      *) die "unknown collect option: $1" ;;
    esac
  done
  [[ -n "${run_dir}" && -n "${case_id}" && -n "${status}" ]] || die "collect requires run-dir, case, and status"
  run_dir="$(canonical_existing_dir "${run_dir}")"
  [[ -f "${run_dir}/manifest.json" && -f "${run_dir}/case-matrix.tsv" ]] || die "not a runner evidence directory"
  verify_plan_seal "${run_dir}"
  [[ "${case_id}" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || die "invalid case id"
  awk -F '\t' -v id="${case_id}" 'NR > 1 && $1 == id { found=1 } END { exit !found }' \
    "${run_dir}/case-matrix.tsv" || die "case is not in the planned matrix: ${case_id}"
  case "${status}" in completed|failed|timeout|interrupted|skipped) ;; *) die "invalid status: ${status}" ;; esac

  local case_dir="${run_dir}/cases/${case_id}" staging_dir=""
  if [[ "${verify_only}" -eq 0 ]]; then
    [[ ! -e "${case_dir}" ]] || die "case was already collected; evidence is immutable"
    mkdir -p -- "${run_dir}/cases"
    staging_dir="$(mktemp -d -- "${run_dir}/cases/.${case_id}.tmp.XXXXXX")"
    trap 'if [[ -n "${staging_dir:-}" ]]; then chmod -R u+w -- "${staging_dir}" 2>/dev/null || true; rm -rf -- "${staging_dir}"; fi' EXIT
  fi
  local have_gdb=0 have_serial=0 have_stats=0 have_config=0
  local have_qemu_cmdline=0 have_virtiofsd_cmdline=0
  local serial_source="" stats_source="" config_source="" case_result_source=""
  local qemu_cmdline_source="" virtiofsd_cmdline_source=""
  local qemu_cmdline_live_source="" virtiofsd_cmdline_live_source="" context_source=""
  local -a captured_artifacts=() original_artifacts=() original_fingerprints=()
  declare -A seen_artifacts=()
  for spec in "${artifacts[@]}"; do
    [[ "${spec}" == *=* ]] || die "artifact must be NAME=PATH: ${spec}"
    name="${spec%%=*}"
    source="${spec#*=}"
    [[ "${name}" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || die "invalid artifact name: ${name}"
    case "${name}" in
      status.json|artifacts.tsv)
        die "artifact name is reserved by the collector: ${name}" ;;
      collector_context)
        [[ "${verify_only}" -eq 1 ]] || \
          die "artifact name is reserved by the collector: ${name}" ;;
      case-result.json)
        [[ "${verify_only}" -eq 1 ]] || \
          die "artifact name is reserved by the collector: ${name}" ;;
    esac
    [[ -z "${seen_artifacts[${name}]:-}" ]] || die "duplicate artifact name: ${name}"
    seen_artifacts["${name}"]=1
    [[ -f "${source}" && ! -L "${source}" ]] || die "artifact is not a regular non-symlink file: ${source}"
    if [[ "${verify_only}" -eq 0 ]]; then
      local source_fingerprint
      source_fingerprint="$(stat -Lc '%d:%i:%s:%y:%z' -- "${source}")"
      destination="${staging_dir}/${name}"
      cp -- "${source}" "${destination}"
      [[ "$(stat -Lc '%d:%i:%s:%y:%z' -- "${source}")" == "${source_fingerprint}" &&
         "$(sha256_or_unavailable "${source}")" == "$(sha256_or_unavailable "${destination}")" ]] || \
        die "artifact changed while being captured: ${name}"
      original_artifacts+=("${name}=${source}")
      original_fingerprints+=("${source_fingerprint}")
      source="${destination}"
    fi
    captured_artifacts+=("${name}=${source}")
    [[ "${name}" == "gdb" ]] && have_gdb=1
    if [[ "${name}" == "serial" ]]; then have_serial=1; serial_source="${source}"; fi
    if [[ "${name}" == "stats" ]]; then have_stats=1; stats_source="${source}"; fi
    if [[ "${name}" == "config" ]]; then have_config=1; config_source="${source}"; fi
    [[ "${name}" == "case-result.json" ]] && case_result_source="${source}"
    if [[ "${name}" == "qemu_cmdline" ]]; then
      have_qemu_cmdline=1
      [[ "${verify_only}" -eq 1 ]] || qemu_cmdline_live_source="${spec#*=}"
      qemu_cmdline_source="${source}"
    fi
    if [[ "${name}" == "virtiofsd_cmdline" ]]; then
      have_virtiofsd_cmdline=1
      [[ "${verify_only}" -eq 1 ]] || virtiofsd_cmdline_live_source="${spec#*=}"
      virtiofsd_cmdline_source="${source}"
    fi
    [[ "${name}" == "collector_context" ]] && context_source="${source}"
  done
  if [[ "${verify_only}" -eq 0 ]]; then
    verify_capture_sources_stable "${staging_dir}" original_artifacts original_fingerprints
  fi
  if [[ "${status}" == "timeout" &&
        (${have_gdb} -ne 1 || ${have_serial} -ne 1 || ${have_stats} -ne 1) ]]; then
    die "timeout collection requires gdb, serial, and stats artifacts"
  fi
  if [[ "${status}" == "timeout" ]]; then
    [[ -s "${serial_source}" && -s "${stats_source}" ]] || \
      die "timeout serial and stats artifacts must be non-empty"
    local timeout_gdb_source=""
    for spec in "${captured_artifacts[@]}"; do
      [[ "${spec%%=*}" == "gdb" ]] && timeout_gdb_source="${spec#*=}"
    done
    [[ -s "${timeout_gdb_source}" ]] || die "timeout GDB artifact must be non-empty"
  fi
  if [[ "${status}" != "skipped" && (${have_serial} -ne 1 || ${have_qemu_cmdline} -ne 1 || ${have_virtiofsd_cmdline} -ne 1) ]]; then
    die "non-skipped collection requires serial, qemu_cmdline, and virtiofsd_cmdline artifacts"
  fi
  if [[ "${status}" == "completed" && ${have_config} -ne 1 ]]; then
    die "completed collection requires an independent config artifact in every mode"
  fi
  if [[ "${status}" != "skipped" ]]; then
    if [[ "${verify_only}" -eq 0 ]]; then
      context_source="${staging_dir}/collector_context"
      capture_process_context "${qemu_cmdline_live_source}" \
        "${virtiofsd_cmdline_live_source}" "${context_source}"
      verify_capture_sources_stable "${staging_dir}" original_artifacts original_fingerprints
      captured_artifacts+=("collector_context=${context_source}")
    fi
    [[ -n "${context_source}" ]] || die "sealed case lacks collector process context"
    validate_cmdline_artifacts \
      "${qemu_cmdline_source}" "${virtiofsd_cmdline_source}" \
      "${run_dir}/manifest.json" "${context_source}"
  fi

  local mode phase file_size block_size guest_cache host_cache
  IFS=$'\t' read -r _ mode phase file_size block_size guest_cache host_cache < <(
    awk -F '\t' -v id="${case_id}" '$1 == id { print; exit }' "${run_dir}/case-matrix.tsv"
  )
  if [[ "${status}" == "completed" ]]; then
    local expected_workload dataset run_id begin_marker end_marker transcript_ok
    dataset="$(jq -r '.dataset.path' "${run_dir}/manifest.json")"
    run_id="$(jq -r '.run_id' "${run_dir}/manifest.json")"
    [[ "${phase}" == "prepare" ]] && expected_workload="prepare" || expected_workload="sequential_read"
    local result_values result_elapsed_us result_bytes result_ops result_syscalls
    local result_short_io result_eintr result_checksum
    local result_read_requests_json=null result_requested_bytes_json=null
    begin_marker="P0_CASE_BEGIN:${case_id}:run_id=${run_id}"
    end_marker="P0_CASE_END:${case_id}:run_id=${run_id}:rc=0"
    result_values="$(parse_completed_result "${serial_source}" "${expected_workload}" \
      "${dataset}" "${file_size}" "${block_size}" "${begin_marker}" "${end_marker}")" || \
      die "completed result lacks one valid result inside its case markers"
    IFS=$'\t' read -r result_elapsed_us result_bytes result_ops result_syscalls \
      result_short_io result_eintr result_checksum <<<"${result_values}"
    local config_values config_epoch config_max_read config_max_pages config_max_readahead
    local config_async_read config_sg_pages config_effective_bytes
    config_values="$(parse_negotiated_config "${config_source}" "${run_id}" "${case_id}")" || \
      die "config artifact lacks valid negotiated non-DAX read limits"
    IFS=$'\t' read -r config_epoch config_max_read config_max_pages config_max_readahead \
      config_async_read config_sg_pages config_effective_bytes <<<"${config_values}"
    if [[ "${guest_cache}" == "warm" ]]; then
      local warm_begin="P0_WARMUP_BEGIN:${case_id}:run_id=${run_id}"
      local warm_end="P0_WARMUP_END:${case_id}:run_id=${run_id}:rc=0"
      parse_completed_result "${serial_source}" "${expected_workload}" "${dataset}" \
        "${file_size}" "${block_size}" "${warm_begin}" "${warm_end}" >/dev/null || \
        die "warm case lacks one successful, run-bound preheat result"
    fi
    local expected_mount="hostshare on $(jq -r '.guest.mount' "${run_dir}/manifest.json") type virtiofs "
    [[ "$(awk -v prefix="P0_MOUNT:${run_id}:" -v expected_mount="${expected_mount}" '
      { sub(/\r$/, "") }
      index($0, prefix expected_mount) == 1 { count++ }
      END { print count + 0 }
    ' "${serial_source}")" == "1" ]] || \
      die "completed case lacks one run-bound non-DAX mount assertion"
    transcript_ok="$(awk -v begin="${begin_marker}" -v end="${end_marker}" \
      -v workload="${expected_workload}" -v dataset="${dataset}" \
      -v file_size="${file_size}" -v block_size="${block_size}" -v run_id="${run_id}" '
        BEGIN {
          if (workload == "prepare") {
            phase_count=10
            phase_name[1]="open"; phase_event[1]="begin"
            phase_name[2]="open"; phase_event[2]="end"
            phase_name[3]="data_loop"; phase_event[3]="begin"
            phase_name[4]="data_loop"; phase_event[4]="end"
            phase_name[5]="fsync"; phase_event[5]="begin"
            phase_name[6]="fsync"; phase_event[6]="end"
            phase_name[7]="close"; phase_event[7]="begin"
            phase_name[8]="close"; phase_event[8]="end"
            phase_name[9]="manifest"; phase_event[9]="begin"
            phase_name[10]="manifest"; phase_event[10]="end"
          } else {
            phase_count=8
            phase_name[1]="open"; phase_event[1]="begin"
            phase_name[2]="open"; phase_event[2]="end"
            phase_name[3]="data_loop"; phase_event[3]="begin"
            phase_name[4]="data_loop"; phase_event[4]="end"
            phase_name[5]="close"; phase_event[5]="begin"
            phase_name[6]="close"; phase_event[6]="end"
            phase_name[7]="verify"; phase_event[7]="begin"
            phase_name[8]="verify"; phase_event[8]="end"
          }
        }
        function token(line, key, value, parts, count, i) {
          count = split(line, parts, " ")
          for (i = 1; i <= count; i++) if (parts[i] == key "=" value) return 1
          return 0
        }
        function has_key(line, key, parts, count, i) {
          count = split(line, parts, " ")
          for (i = 1; i <= count; i++) if (index(parts[i], key "=") == 1) return 1
          return 0
        }
        { sub(/\r$/, "") }
        $0 == begin {
          if (inside || state != 0) invalid=1
          inside=1; state=1; begin_count++; next
        }
        $0 == end {
          if (!inside || state != 6) invalid=1
          inside=0; state=7; end_count++; next
        }
        !inside { next }
        substr($0, 1, 10) == "quiescence" && token($0, "stage", "before") {
          if (state != 1 || !token($0, "workload", workload) || !token($0, "status", "ok") ||
              !token($0, "run_id", run_id)) invalid=1
          state=2; qbefore++; next
        }
        substr($0, 1, 6) == "phase " {
          phase_seen++
          if (state != 2 || phase_seen > phase_count ||
              !token($0, "workload", workload) || !token($0, "dataset", dataset) ||
              !token($0, "phase", phase_name[phase_seen]) ||
              !token($0, "event", phase_event[phase_seen]) || !token($0, "run_id", run_id)) invalid=1
          if (phase_seen == phase_count) state=3
          next
        }
        substr($0, 1, 7) == "result " {
          result_total++
          if (state != 3 || !token($0, "workload", workload) || !token($0, "status", "ok") ||
              !token($0, "errno", "0") || !token($0, "dataset", dataset) ||
              !token($0, "file_size", file_size) || !token($0, "block_size", block_size) ||
              !token($0, "bytes", file_size) || !token($0, "run_id", run_id)) invalid=1
          state=4; next
        }
        substr($0, 1, 11) == "io_summary " {
          io_total++
          if (state != 4 || !token($0, "workload", workload) || !has_key($0, "checksum") ||
              !token($0, "run_id", run_id)) invalid=1
          state=5; next
        }
        substr($0, 1, 10) == "quiescence" && token($0, "stage", "after") {
          if (state != 5 || !token($0, "workload", workload) || !token($0, "status", "ok") ||
              !token($0, "run_id", run_id)) invalid=1
          state=6; qafter++; next
        }
        END {
          ok = !invalid && !inside && state == 7 && begin_count == 1 && end_count == 1 &&
               qbefore == 1 && qafter == 1 && phase_seen == phase_count &&
               result_total == 1 && io_total == 1
          print ok ? "yes" : "no"
        }
      ' "${serial_source}")"
    [[ "${transcript_ok}" == "yes" ]] || \
      die "completed case requires one ordered, exact helper transcript and quiescent completion"
    if [[ "${mode}" == "performance" ]]; then
      [[ "$(count_exact_line "${serial_source}" "P0_STATS_MODE:off:run_id=${run_id}")" == "1" ]] || \
        die "performance case lacks the off-mode serial assertion"
    else
      [[ "$(count_exact_line "${serial_source}" "P0_STATS_MODE:${mode}:run_id=${run_id}")" == "1" ]] || \
        die "${mode} case lacks its stats-mode serial assertion"
      [[ "${have_stats}" -eq 1 ]] || die "completed ${mode} case requires --artifact stats=PATH"
      local stats_header=""
      IFS= read -r stats_header <"${stats_source}" || true
      [[ "${stats_header}" == "P0_STATS_RUN:${run_id}:${case_id}" ]] || \
        die "stats artifact is not bound to this run and case"
      grep -Eq "^mode ${mode}\\r?$" "${stats_source}" || \
        die "stats artifact does not report mode ${mode}"

      if [[ "${phase}" == "read" ]]; then
        local direct_req direct_done direct_req_bytes direct_done_bytes
        local read_reqs read_bytes bridge_submitted bridge_completed bucket_sum=0 value
        direct_req="$(extract_stats_delta "${serial_source}" sequential_read \
          virtiofs.direct_read_requested_requests_total)" || \
          die "read evidence lacks one direct-read request delta"
        direct_done="$(extract_stats_delta "${serial_source}" sequential_read \
          virtiofs.direct_read_completed_requests_total)" || \
          die "read evidence lacks one direct-read completion delta"
        direct_req_bytes="$(extract_stats_delta "${serial_source}" sequential_read \
          virtiofs.direct_read_requested_bytes_total)" || \
          die "read evidence lacks one direct-read requested-bytes delta"
        direct_done_bytes="$(extract_stats_delta "${serial_source}" sequential_read \
          virtiofs.direct_read_completed_bytes_total)" || \
          die "read evidence lacks one direct-read completed-bytes delta"
        read_reqs="$(extract_stats_delta "${serial_source}" sequential_read \
          virtiofs.read_requested_requests_total)" || \
          die "read evidence lacks the light request-size delta"
        read_bytes="$(extract_stats_delta "${serial_source}" sequential_read \
          virtiofs.read_requested_bytes_total)" || \
          die "read evidence lacks the light requested-bytes delta"
        bridge_submitted="$(extract_stats_delta "${serial_source}" sequential_read \
          virtiofs.bridge_submitted_total)" || die "read evidence lacks bridge submissions"
        bridge_completed="$(extract_stats_delta "${serial_source}" sequential_read \
          virtiofs.bridge_completed_total)" || die "read evidence lacks bridge completions"
        [[ "${direct_req}" -gt 0 && "${direct_req}" -eq "${direct_done}" &&
           "${direct_req_bytes}" -eq "${file_size}" &&
           "${direct_done_bytes}" -eq "${file_size}" &&
           "${read_reqs}" -gt 0 ]] && \
          read_amplification_valid "${direct_done_bytes}" "${read_bytes}" && \
          [[ "${bridge_submitted}" -gt 0 && \
             "${bridge_submitted}" -eq "${bridge_completed}" ]] || \
          die "read request/DMA/bridge conservation checks failed"
        for value in read_requested_pages_1 read_requested_pages_2_4 \
          read_requested_pages_5_16 read_requested_pages_17_32 \
          read_requested_pages_33_64 read_requested_pages_65_plus; do
          value="$(extract_stats_delta_or_zero "${serial_source}" sequential_read \
            "virtiofs.${value}")" || die "read request-size bucket evidence is ambiguous"
          bucket_sum=$((bucket_sum + value))
        done
        [[ "${bucket_sum}" -eq "${read_reqs}" ]] || \
          die "read request-size buckets do not conserve request count"
        result_read_requests_json="${read_reqs}"
        result_requested_bytes_json="${read_bytes}"

        local gauge section key
        for gauge in \
          fuse:request_queue_current fuse:dispatch_current fuse:processing_current \
          fuse:background_inflight_current fuse:read_reservation_current \
          virtiofs:inflight_current virtiofs:hiprio_inflight_current \
          virtiofs:request_inflight_current virtiofs:queue_full_blocked_current \
          virtiofs:reply_retained_current; do
          section="${gauge%%:*}"; key="${gauge#*:}"
          value="$(stats_snapshot_value "${stats_source}" "${section}" "${key}")" || \
            die "stats snapshot lacks unique owner gauge ${gauge}"
          [[ "${value}" -eq 0 ]] || die "owner gauge ${gauge} is nonzero after completion"
        done
      fi
    fi

    local share_dir dataset_host dataset_manifest_host current_dataset_sha current_manifest_sha
    share_dir="$(jq -r '.dataset.share_dir' "${run_dir}/manifest.json")"
    dataset_host="${share_dir}/.virtiofs_bench_${dataset}/seq.dat"
    dataset_manifest_host="${share_dir}/.virtiofs_bench_${dataset}/manifest.v1"
    [[ -f "${dataset_host}" && ! -L "${dataset_host}" && -f "${dataset_manifest_host}" && ! -L "${dataset_manifest_host}" ]] || \
      die "completed case requires the prepared host dataset and manifest"
    current_dataset_sha="$(sha256_or_unavailable "${dataset_host}")"
    current_manifest_sha="$(sha256_or_unavailable "${dataset_manifest_host}")"
    if [[ "${phase}" == "read" ]]; then
      [[ "${current_dataset_sha}" == "$(jq -r '.dataset.sha256' "${run_dir}/manifest.json")" && \
         "${current_manifest_sha}" == "$(jq -r '.dataset.manifest_sha256' "${run_dir}/manifest.json")" ]] || \
        die "read dataset changed after the plan was created"
    fi
    if [[ "${verify_only}" -eq 1 ]]; then
      [[ "${current_dataset_sha}" == "$(jq -r '.dataset_sha256' "${case_dir}/status.json")" &&
         "${current_manifest_sha}" == "$(jq -r '.dataset_manifest_sha256' "${case_dir}/status.json")" ]] || \
        die "dataset endpoint hashes differ from the collected case"
    fi

    local expected_case_result
    expected_case_result="$(jq -cn \
      --arg schema "dragonos.virtiofs.non-dax-case-result.v1" \
      --arg runner_version "${RUNNER_VERSION}" --arg case_id "${case_id}" \
      --arg workload "${expected_workload}" --arg mode "${mode}" \
      --arg status completed --arg checksum "${result_checksum}" \
      --argjson elapsed_us "${result_elapsed_us}" --argjson bytes "${result_bytes}" \
      --argjson ops "${result_ops}" --argjson syscalls "${result_syscalls}" \
      --argjson short_io "${result_short_io}" --argjson eintr "${result_eintr}" \
      --argjson read_requests "${result_read_requests_json}" \
      --argjson requested_bytes "${result_requested_bytes_json}" \
      --argjson epoch "${config_epoch}" \
      --argjson max_read "${config_max_read}" \
      --argjson max_pages "${config_max_pages}" --argjson max_readahead "${config_max_readahead}" \
      --argjson async_read "${config_async_read}" --argjson sg_pages "${config_sg_pages}" \
      --argjson effective_bytes "${config_effective_bytes}" \
      '{schema:$schema,runner_version:$runner_version,status:$status,case_id:$case_id,
        workload:$workload,mode:$mode,
        result:{elapsed_us:$elapsed_us,bytes:$bytes,ops:$ops,syscalls:$syscalls,
          short_io:$short_io,eintr:$eintr,checksum:$checksum,
          read_requests:$read_requests,requested_bytes:$requested_bytes},
        config:{init_epoch:$epoch,negotiated_max_read_bytes:$max_read,negotiated_max_pages:$max_pages,
          negotiated_max_readahead_bytes:$max_readahead,negotiated_async_read:$async_read,
          sg_limit_pages_configured:$sg_pages,effective_read_payload_limit_bytes:$effective_bytes}}')"
    if [[ "${verify_only}" -eq 1 ]]; then
      [[ -n "${case_result_source}" && "$(jq -c . "${case_result_source}")" == "${expected_case_result}" ]] || \
        die "parsed case result differs from immutable transcript/config evidence"
    else
      printf '%s\n' "${expected_case_result}" >"${staging_dir}/case-result.json"
      captured_artifacts+=("case-result.json=${staging_dir}/case-result.json")
    fi
  fi

  if [[ "${verify_only}" -eq 1 ]]; then
    return 0
  fi

  : >"${staging_dir}/artifacts.tsv"
  for spec in "${captured_artifacts[@]}"; do
    name="${spec%%=*}"
    source="${spec#*=}"
    printf '%s\t%s\t%s\n' "${name}" "$(sha256_or_unavailable "${source}")" \
      "$(stat -c '%s' -- "${source}")" >>"${staging_dir}/artifacts.tsv"
  done
  local plan_seal_sha artifacts_tsv_sha
  plan_seal_sha="$(sha256_or_unavailable "${run_dir}/plan.sha256")"
  artifacts_tsv_sha="$(sha256_or_unavailable "${staging_dir}/artifacts.tsv")"
  jq -n --arg case_id "${case_id}" --arg status "${status}" \
    --arg schema "dragonos.virtiofs.non-dax-case.v4" \
    --arg runner_version "${RUNNER_VERSION}" \
    --arg plan_seal_sha256 "${plan_seal_sha}" \
    --arg artifacts_tsv_sha256 "${artifacts_tsv_sha}" \
    --arg collected_utc "$(date -u +'%Y-%m-%dT%H:%M:%SZ')" \
    --arg dataset_sha256 "${current_dataset_sha:-unavailable}" \
    --arg dataset_manifest_sha256 "${current_manifest_sha:-unavailable}" \
    '{schema:$schema,runner_version:$runner_version,case_id:$case_id,status:$status,
      plan_seal_sha256:$plan_seal_sha256,artifacts_tsv_sha256:$artifacts_tsv_sha256,
      collected_utc:$collected_utc,
      dataset_sha256:$dataset_sha256,dataset_manifest_sha256:$dataset_manifest_sha256}' \
    >"${staging_dir}/status.json"
  chmod 0400 "${staging_dir}/artifacts.tsv" "${staging_dir}/status.json" "${staging_dir}"/*
  chmod 0500 "${staging_dir}"
  # -T makes a concurrent collector fail instead of nesting this staging
  # directory inside an already-published case directory.
  mv -T -- "${staging_dir}" "${case_dir}"
  staging_dir=""
  trap - EXIT
  printf 'collected=%s\n' "${case_dir}"
}

finalize_run() {
  local run_dir=""
  while (($#)); do
    case "$1" in
      --run-dir) run_dir="${2:?missing --run-dir value}"; shift 2 ;;
      -h|--help) usage; exit 0 ;;
      *) die "unknown finalize option: $1" ;;
    esac
  done
  [[ -n "${run_dir}" ]] || die "finalize requires --run-dir"
  run_dir="$(canonical_existing_dir "${run_dir}")"
  [[ ! -e "${run_dir}/final.json" ]] || die "run is already finalized"
  verify_plan_seal "${run_dir}"
  local total=0 collected=0 failed=0 case_id status
  while IFS=$'\t' read -r case_id _; do
    [[ "${case_id}" != "case_id" ]] || continue
    ((total += 1))
    if [[ -f "${run_dir}/cases/${case_id}/status.json" ]]; then
      verify_collected_case "${run_dir}" "${case_id}"
      ((collected += 1))
      status="$(jq -r '.status' "${run_dir}/cases/${case_id}/status.json")"
      [[ "${status}" == "completed" ]] || ((failed += 1))
    fi
  done <"${run_dir}/case-matrix.tsv"
  ((collected == total)) || die "run is incomplete: collected ${collected}/${total}; evidence retained at ${run_dir}"
  local final_tmp
  final_tmp="$(mktemp -- "${run_dir}/.final.json.tmp.XXXXXX")"
  trap '[[ -z "${final_tmp:-}" ]] || rm -f -- "${final_tmp}"' EXIT
  jq -n --arg schema "dragonos.virtiofs.non-dax-final.v1" \
    --arg runner_version "${RUNNER_VERSION}" \
    --arg plan_seal_sha256 "$(sha256_or_unavailable "${run_dir}/plan.sha256")" \
    --arg finalized_utc "$(date -u +'%Y-%m-%dT%H:%M:%SZ')" \
    --argjson total "${total}" --argjson failed "${failed}" \
    '{schema:$schema,runner_version:$runner_version,plan_seal_sha256:$plan_seal_sha256,
      finalized_utc:$finalized_utc,total_cases:$total,non_completed_cases:$failed}' \
    >"${final_tmp}"
  chmod 0400 "${final_tmp}"
  ln -- "${final_tmp}" "${run_dir}/final.json" || die "run was finalized concurrently"
  rm -f -- "${final_tmp}"
  final_tmp=""
  trap - EXIT
  printf 'finalized=%s\n' "${run_dir}"
  ((failed == 0)) || exit 1
}

verify_finalized_run() {
  local run_dir=""
  while (($#)); do
    case "$1" in
      --run-dir) run_dir="${2:?missing --run-dir value}"; shift 2 ;;
      -h|--help) usage; exit 0 ;;
      *) die "unknown verify option: $1" ;;
    esac
  done
  [[ -n "${run_dir}" ]] || die "verify requires --run-dir"
  run_dir="$(canonical_existing_dir "${run_dir}")"
  [[ -f "${run_dir}/final.json" && ! -L "${run_dir}/final.json" ]] || \
    die "verify requires a finalized evidence directory"
  verify_plan_seal "${run_dir}"
  local total=0 failed=0 case_id status
  while IFS=$'\t' read -r case_id _; do
    [[ "${case_id}" != "case_id" ]] || continue
    ((total += 1))
    verify_collected_case "${run_dir}" "${case_id}"
    status="$(jq -r '.status' "${run_dir}/cases/${case_id}/status.json")"
    [[ "${status}" == "completed" ]] || ((failed += 1))
  done <"${run_dir}/case-matrix.tsv"
  jq -e --arg version "${RUNNER_VERSION}" \
    --arg seal "$(sha256_or_unavailable "${run_dir}/plan.sha256")" \
    --argjson total "${total}" --argjson failed "${failed}" '
      .schema == "dragonos.virtiofs.non-dax-final.v1" and
      .runner_version == $version and .plan_seal_sha256 == $seal and
      .total_cases == $total and .non_completed_cases == $failed and
      (.finalized_utc | type == "string")
    ' "${run_dir}/final.json" >/dev/null || die "final.json is incompatible with replayed evidence"
  printf 'verified=%s\n' "${run_dir}"
  ((failed == 0)) || exit 1
}

if [[ "${BASH_SOURCE[0]}" != "$0" ]]; then
  return 0
fi

require_command jq
require_command sha256sum
require_command realpath
require_command od
require_command find
require_command ss

command_name="${1:-plan}"
[[ $# -eq 0 ]] || shift
case "${command_name}" in
  plan) plan_run "$@" ;;
  collect) collect_case "$@" ;;
  finalize) finalize_run "$@" ;;
  verify) verify_finalized_run "$@" ;;
  -h|--help|help) usage ;;
  *) die "unknown command: ${command_name}" ;;
esac
