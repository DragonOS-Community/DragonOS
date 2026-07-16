#!/usr/bin/env bash
# Build DragonOS from a clean tree and atomically attest the exact benchmark artifacts.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd)"

usage() {
  cat <<'EOF'
Usage: create_build_manifest.sh --output FILE [--skip-build]

The normal path requires a clean Git tree, runs the fixed kernel/user/disk-image
build, verifies the tree stayed clean, then seals kernel, disk image and the guest
virtiofs_bench helper. --skip-build is test-only and requires
DRAGONOS_BUILD_MANIFEST_ALLOW_SKIP_BUILD=1.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 2
}

sha256_file() {
  sha256sum -- "$1" | awk '{print $1}'
}

require_clean_tree() {
  local status
  status="$(git -C "${REPO_ROOT}" status --porcelain=v1 --untracked-files=all)"
  [[ -z "${status}" ]] || die "formal build attestation requires a clean Git tree"
}

output=""
skip_build=0
while (($#)); do
  case "$1" in
    --output) output="${2:?missing --output value}"; shift 2 ;;
    --skip-build) skip_build=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done

[[ -n "${output}" ]] || die "--output is required"
[[ "${output}" == /* ]] || die "--output must be absolute"
[[ ! -e "${output}" ]] || die "refusing to replace existing output: ${output}"
command -v git >/dev/null || die "git is required"
command -v jq >/dev/null || die "jq is required"
command -v sha256sum >/dev/null || die "sha256sum is required"

require_clean_tree
commit="$(git -C "${REPO_ROOT}" rev-parse HEAD)"
tree="$(git -C "${REPO_ROOT}" rev-parse 'HEAD^{tree}')"

if ((skip_build)); then
  [[ "${DRAGONOS_BUILD_MANIFEST_ALLOW_SKIP_BUILD:-}" == "1" ]] || \
    die "--skip-build is restricted to explicit host tests"
  build_commands_json='["test-only: skip build"]'
else
  make -C "${REPO_ROOT}" kernel
  make -C "${REPO_ROOT}" user
  SKIP_GRUB=1 make -C "${REPO_ROOT}" write_diskimage
  build_commands_json='["make kernel","make user","SKIP_GRUB=1 make write_diskimage"]'
fi

require_clean_tree
[[ "$(git -C "${REPO_ROOT}" rev-parse HEAD)" == "${commit}" &&
   "$(git -C "${REPO_ROOT}" rev-parse 'HEAD^{tree}')" == "${tree}" ]] || \
  die "repository identity changed during build"

kernel="${REPO_ROOT}/bin/kernel/kernel.elf"
disk_image="${REPO_ROOT}/bin/disk-image-x86_64.img"
guest_helper_host="${REPO_ROOT}/bin/sysroot/bin/virtiofs_bench"
for artifact in "${kernel}" "${disk_image}" "${guest_helper_host}"; do
  [[ -f "${artifact}" && ! -L "${artifact}" ]] || die "missing regular artifact: ${artifact}"
done

toolchain_file=""
manifest_tmp=""
cleanup() {
  [[ -z "${toolchain_file}" ]] || rm -f -- "${toolchain_file}"
  [[ -z "${manifest_tmp}" ]] || rm -f -- "${manifest_tmp}"
}
trap cleanup EXIT
toolchain_file="$(mktemp)"
manifest_tmp="$(mktemp "$(dirname -- "${output}")/.build-manifest.XXXXXX")"
{
  rustc --version 2>&1 || true
  cargo --version 2>&1 || true
  x86_64-linux-gnu-gcc --version 2>&1 | head -n 1 || true
  dadk --version 2>&1 || true
  nix --version 2>&1 || true
} >"${toolchain_file}"

jq -n \
  --arg schema "dragonos.virtiofs.build-manifest.v1" \
  --arg created_utc "$(date -u +'%Y-%m-%dT%H:%M:%SZ')" \
  --arg commit "${commit}" --arg tree "${tree}" \
  --arg toolchain_sha256 "$(sha256_file "${toolchain_file}")" \
  --argjson commands "${build_commands_json}" \
  --arg kernel_sha256 "$(sha256_file "${kernel}")" \
  --argjson kernel_size "$(stat -c '%s' -- "${kernel}")" \
  --arg disk_sha256 "$(sha256_file "${disk_image}")" \
  --argjson disk_size "$(stat -c '%s' -- "${disk_image}")" \
  --arg helper_sha256 "$(sha256_file "${guest_helper_host}")" \
  --argjson helper_size "$(stat -c '%s' -- "${guest_helper_host}")" \
  '{schema:$schema,created_utc:$created_utc,
    repo:{commit:$commit,tree:$tree,clean:true},
    build:{commands:$commands,toolchain_fingerprint_sha256:$toolchain_sha256},
    artifacts:{
      kernel:{path:"bin/kernel/kernel.elf",sha256:$kernel_sha256,size:$kernel_size},
      disk_image:{path:"bin/disk-image-x86_64.img",sha256:$disk_sha256,size:$disk_size},
      guest_helper:{host_path:"bin/sysroot/bin/virtiofs_bench",guest_path:"/bin/virtiofs_bench",
                    sha256:$helper_sha256,size:$helper_size}}}' >"${manifest_tmp}"
chmod 0444 "${manifest_tmp}"
# manifest_tmp is deliberately created in the output directory.  link(2)
# therefore publishes it atomically on the same filesystem and, unlike mv,
# fails if another concurrent attester has already claimed the output name.
ln -- "${manifest_tmp}" "${output}" || \
  die "refusing to replace concurrently-created output: ${output}"
printf 'build_manifest=%s\nsha256=%s\n' "${output}" "$(sha256_file "${output}")"
