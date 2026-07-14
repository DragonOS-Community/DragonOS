#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=non_dax_bench_runner.sh
source "${SCRIPT_DIR}/non_dax_bench_runner.sh"

tmp_dir="$(mktemp -d)"
trap 'rm -rf -- "${tmp_dir}"' EXIT

expect_failure() {
  if ("$@") >/dev/null 2>&1; then
    printf 'expected failure: %q' "$1" >&2
    printf ' %q' "${@:2}" >&2
    printf '\n' >&2
    exit 1
  fi
}

cat >"${tmp_dir}/result.ok" <<'EOF'
BEGIN
result workload=sequential_read status=ok errno=0 elapsed_us=123 bytes=1048576 ops=256 syscalls=256 short_io=0 eintr=0 checksum=0123456789abcdef mount=/tmp/host dataset=fixture seed=1 files=1 file_size=1048576 block_size=4096 iterations=1 workers=1 run_id=run cache_mode=cold mount_options=none expect_dax=false sysname=DragonOS release=test
END
EOF
[[ "$(parse_completed_result "${tmp_dir}/result.ok" sequential_read fixture 1048576 4096 BEGIN END)" == \
   $'123\t1048576\t256\t256\t0\t0\t0123456789abcdef' ]]

sed 's/ ops=256//' "${tmp_dir}/result.ok" >"${tmp_dir}/result.missing"
expect_failure parse_completed_result "${tmp_dir}/result.missing" sequential_read fixture 1048576 4096 BEGIN END
sed 's/ syscalls=256//' "${tmp_dir}/result.ok" >"${tmp_dir}/result.missing-syscalls"
expect_failure parse_completed_result "${tmp_dir}/result.missing-syscalls" sequential_read fixture 1048576 4096 BEGIN END
sed 's/ elapsed_us=123/ elapsed_us=123 elapsed_us=124/' \
  "${tmp_dir}/result.ok" >"${tmp_dir}/result.duplicate"
expect_failure parse_completed_result "${tmp_dir}/result.duplicate" sequential_read fixture 1048576 4096 BEGIN END
sed 's/ ops=256/ ops=2x6/' "${tmp_dir}/result.ok" >"${tmp_dir}/result.nonnumeric"
expect_failure parse_completed_result "${tmp_dir}/result.nonnumeric" sequential_read fixture 1048576 4096 BEGIN END

cat >"${tmp_dir}/config.ok" <<'EOF'
P0_CONFIG_RUN:run:case
[fuse]
init_epoch 1
negotiated_max_read_bytes 1048576
negotiated_max_pages 256
negotiated_max_readahead_bytes 1048576
negotiated_async_read 1
effective_read_payload_limit_bytes 131072
[virtiofs]
sg_limit_pages_configured 32
EOF
[[ "$(parse_negotiated_config "${tmp_dir}/config.ok" run case)" == \
   $'1\t1048576\t256\t1048576\t1\t32\t131072' ]]
sed 's/negotiated_async_read 1/negotiated_async_read 2/' \
  "${tmp_dir}/config.ok" >"${tmp_dir}/config.bad-async"
expect_failure parse_negotiated_config "${tmp_dir}/config.bad-async" run case
sed '/sg_limit_pages_configured/d' "${tmp_dir}/config.ok" >"${tmp_dir}/config.missing"
expect_failure parse_negotiated_config "${tmp_dir}/config.missing" run case
sed 's/negotiated_max_pages 256/negotiated_max_pages 0/' \
  "${tmp_dir}/config.ok" >"${tmp_dir}/config.zero"
expect_failure parse_negotiated_config "${tmp_dir}/config.zero" run case
sed 's/effective_read_payload_limit_bytes 131072/effective_read_payload_limit_bytes 262144/' \
  "${tmp_dir}/config.ok" >"${tmp_dir}/config.bad-effective"
expect_failure parse_negotiated_config "${tmp_dir}/config.bad-effective" run case
sed 's/init_epoch 1/init_epoch 2/' "${tmp_dir}/config.ok" >"${tmp_dir}/config.reused"
expect_failure parse_negotiated_config "${tmp_dir}/config.reused" run case

read_amplification_valid 4 4
read_amplification_valid 4 5
read_amplification_valid 1 2
expect_failure read_amplification_valid 4 6
expect_failure read_amplification_valid 5 4

validate_csv_numbers "1,16777216" "block size" 16777216
validate_csv_numbers "1,1073741824" "file size" 1073741824
expect_failure validate_csv_numbers "16777217" "block size" 16777216
expect_failure validate_csv_numbers "1073741825" "file size" 1073741824
expect_failure validate_csv_numbers "999999999999999999999999" "file size" 1073741824

printf 'artifact-v1\n' >"${tmp_dir}/stable-artifact"
stable_sha="$(sha256_or_unavailable "${tmp_dir}/stable-artifact")"
[[ "$(stable_manifest_artifact_sha256 "${tmp_dir}/stable-artifact" "${stable_sha}" fixture)" == \
   "${stable_sha}" ]]
printf 'artifact-v2\n' >"${tmp_dir}/stable-artifact"
expect_failure stable_manifest_artifact_sha256 "${tmp_dir}/stable-artifact" "${stable_sha}" fixture
ln -s "${tmp_dir}/stable-artifact" "${tmp_dir}/stable-artifact-link"
expect_failure stable_manifest_artifact_sha256 "${tmp_dir}/stable-artifact-link" \
  "$(sha256_or_unavailable "${tmp_dir}/stable-artifact")" fixture

plan_dir="${tmp_dir}/plan"
mkdir -p "${plan_dir}"
kernel_sha="$(printf kernel | sha256sum | awk '{print $1}')"
disk_sha="$(printf disk | sha256sum | awk '{print $1}')"
helper_sha="$(printf helper | sha256sum | awk '{print $1}')"
jq -n --arg kernel "${kernel_sha}" --arg disk "${disk_sha}" --arg helper "${helper_sha}" \
  '{schema:"dragonos.virtiofs.build-manifest.v1",artifacts:{kernel:{sha256:$kernel},
    disk_image:{sha256:$disk},guest_helper:{sha256:$helper}}}' >"${plan_dir}/build-manifest.json"
jq -n --arg version "${RUNNER_VERSION}" \
  --arg build_sha "$(sha256_or_unavailable "${plan_dir}/build-manifest.json")" \
  --arg kernel "${kernel_sha}" --arg disk "${disk_sha}" --arg helper "${helper_sha}" \
  '{schema:"dragonos.virtiofs.non-dax-run.v2",runner_version:$version,
    repo:{build_manifest_sha256:$build_sha},artifacts:{kernel_sha256:$kernel,
      disk_image_sha256:$disk,guest_helper_sha256:$helper}}' >"${plan_dir}/manifest.json"
printf 'case_id\tmode\tphase\tfile_size\tblock_size\tguest_cache\thost_cache\ncase\tlight\tread\t4\t4\tcold\twarm\n' \
  >"${plan_dir}/case-matrix.tsv"
for member in guest-commands.sh host-facts.txt git-status.txt MANUAL-STAGE.txt runner.sh common.sh; do
  printf '%s\n' "${member}" >"${plan_dir}/${member}"
done
(cd "${plan_dir}" && sha256sum -- manifest.json build-manifest.json case-matrix.tsv guest-commands.sh \
  host-facts.txt git-status.txt MANUAL-STAGE.txt runner.sh common.sh >plan.sha256)
verify_plan_seal "${plan_dir}"
cp "${plan_dir}/case-matrix.tsv" "${tmp_dir}/case-matrix.valid"
printf 'case\tlight\tread\t1073741825\t4\tcold\twarm\n' >>"${plan_dir}/case-matrix.tsv"
(cd "${plan_dir}" && sha256sum -- manifest.json build-manifest.json case-matrix.tsv guest-commands.sh \
  host-facts.txt git-status.txt MANUAL-STAGE.txt runner.sh common.sh >plan.sha256)
expect_failure verify_plan_seal "${plan_dir}"
mv "${tmp_dir}/case-matrix.valid" "${plan_dir}/case-matrix.tsv"
(cd "${plan_dir}" && sha256sum -- manifest.json build-manifest.json case-matrix.tsv guest-commands.sh \
  host-facts.txt git-status.txt MANUAL-STAGE.txt runner.sh common.sh >plan.sha256)
mkdir -p "${plan_dir}/cases/case"
printf '{"status":"completed"}\n' >"${plan_dir}/cases/case/status.json"
jq -n --arg version "${RUNNER_VERSION}" \
  --arg seal "$(sha256_or_unavailable "${plan_dir}/plan.sha256")" \
  '{schema:"dragonos.virtiofs.non-dax-final.v1",runner_version:$version,
    plan_seal_sha256:$seal,finalized_utc:"test",total_cases:1,non_completed_cases:0}' \
  >"${plan_dir}/final.json"
# The finalized-run test isolates final metadata/replay orchestration; detailed
# immutable case replay is exercised by verify_collected_case in production.
verify_collected_case() { :; }
verify_finalized_run --run-dir "${plan_dir}" >/dev/null
sed -i 's/[0-9a-f]\{64\}/bad/' "${plan_dir}/final.json"
expect_failure verify_finalized_run --run-dir "${plan_dir}"
printf 'tampered\n' >>"${plan_dir}/runner.sh"
expect_failure verify_plan_seal "${plan_dir}"

# Exercise the attester in a clean synthetic repository so two concurrent
# publishers race for exactly one immutable output name.
manifest_repo="${tmp_dir}/manifest-repo"
mkdir -p "${manifest_repo}/tools/virtiofs" "${manifest_repo}/bin/kernel" \
  "${manifest_repo}/bin/sysroot/bin" "${manifest_repo}/bin"
cp "${SCRIPT_DIR}/create_build_manifest.sh" "${manifest_repo}/tools/virtiofs/"
printf 'kernel\n' >"${manifest_repo}/bin/kernel/kernel.elf"
printf 'disk\n' >"${manifest_repo}/bin/disk-image-x86_64.img"
printf 'helper\n' >"${manifest_repo}/bin/sysroot/bin/virtiofs_bench"
git -C "${manifest_repo}" init -q
git -C "${manifest_repo}" config user.name test
git -C "${manifest_repo}" config user.email test@example.invalid
git -C "${manifest_repo}" add .
git -C "${manifest_repo}" commit -qm fixture
manifest_output="${manifest_repo}/manifest.json"
set +e
DRAGONOS_BUILD_MANIFEST_ALLOW_SKIP_BUILD=1 \
  "${manifest_repo}/tools/virtiofs/create_build_manifest.sh" --skip-build \
  --output "${manifest_output}" >"${tmp_dir}/manifest-1.out" 2>"${tmp_dir}/manifest-1.err" &
manifest_pid_1=$!
DRAGONOS_BUILD_MANIFEST_ALLOW_SKIP_BUILD=1 \
  "${manifest_repo}/tools/virtiofs/create_build_manifest.sh" --skip-build \
  --output "${manifest_output}" >"${tmp_dir}/manifest-2.out" 2>"${tmp_dir}/manifest-2.err" &
manifest_pid_2=$!
wait "${manifest_pid_1}"; manifest_status_1=$?
wait "${manifest_pid_2}"; manifest_status_2=$?
set -e
(( (manifest_status_1 == 0) + (manifest_status_2 == 0) == 1 ))
[[ -f "${manifest_output}" && ! -L "${manifest_output}" ]]
[[ -z "$(find "${manifest_repo}" -maxdepth 1 -name '.build-manifest.*' -print -quit)" ]]

printf 'non_dax_bench_runner tests: PASS\n'
