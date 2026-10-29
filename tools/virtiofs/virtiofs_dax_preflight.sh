#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
ENV_FILE="${DRAGONOS_VIRTIOFS_ENV_FILE:-${SCRIPT_DIR}/env.sh}"
DAX_QEMU_OVERRIDE="${DRAGONOS_VIRTIOFS_QEMU_BIN:-}"
DAX_CACHE_OVERRIDE="${DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE:-}"
DAX_STAMP_OVERRIDE="${DRAGONOS_VIRTIOFS_DAX_PREFLIGHT_STAMP:-}"
DAX_REQUIRED_OVERRIDE="${DRAGONOS_VIRTIOFS_DAX_REQUIRED:-}"
VIRTIOFSD_BIN_OVERRIDE="${VIRTIOFSD_BIN:-}"
VIRTIOFSD_CACHE_OVERRIDE="${VIRTIOFSD_CACHE:-}"
VIRTIOFSD_EXTRA_OVERRIDE="${VIRTIOFSD_EXTRA_ARGS:-}"

fail() {
  echo "DAX preflight failed: $*" >&2
  exit 1
}

[[ -f "${ENV_FILE}" ]] || fail "missing configuration ${ENV_FILE}"
# shellcheck source=/dev/null
source "${ENV_FILE}"
[[ -z "${DAX_QEMU_OVERRIDE}" ]] || DRAGONOS_VIRTIOFS_QEMU_BIN="${DAX_QEMU_OVERRIDE}"
[[ -z "${DAX_CACHE_OVERRIDE}" ]] || DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE="${DAX_CACHE_OVERRIDE}"
[[ -z "${DAX_STAMP_OVERRIDE}" ]] || \
  DRAGONOS_VIRTIOFS_DAX_PREFLIGHT_STAMP="${DAX_STAMP_OVERRIDE}"
[[ -z "${DAX_REQUIRED_OVERRIDE}" ]] || DRAGONOS_VIRTIOFS_DAX_REQUIRED="${DAX_REQUIRED_OVERRIDE}"
[[ -z "${VIRTIOFSD_BIN_OVERRIDE}" ]] || VIRTIOFSD_BIN="${VIRTIOFSD_BIN_OVERRIDE}"
[[ -z "${VIRTIOFSD_CACHE_OVERRIDE}" ]] || VIRTIOFSD_CACHE="${VIRTIOFSD_CACHE_OVERRIDE}"
[[ -z "${VIRTIOFSD_EXTRA_OVERRIDE}" ]] || VIRTIOFSD_EXTRA_ARGS="${VIRTIOFSD_EXTRA_OVERRIDE}"
# shellcheck source=common.sh
source "${SCRIPT_DIR}/common.sh"

QEMU_BIN="${DRAGONOS_VIRTIOFS_QEMU_BIN:-$(command -v qemu-system-x86_64 || true)}"
VIRTIOFSD_PATH="$(virtiofs_detect_daemon || true)"
CACHE_SIZE="${DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE:-}"
RUNTIME_DIR="${RUNTIME_DIR:-${SCRIPT_DIR}/../../bin/virtiofs-runtime}"
STAMP="${DRAGONOS_VIRTIOFS_DAX_PREFLIGHT_STAMP:-${RUNTIME_DIR}/dax-preflight.stamp}"

[[ -x "${QEMU_BIN}" ]] || fail "QEMU binary is not executable: ${QEMU_BIN:-unset}"
[[ "${CACHE_SIZE}" =~ ^[0-9]+$ ]] || fail "DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE must be bytes"
(( CACHE_SIZE >= 2 * 1024 * 1024 )) || fail "cache size must be at least 2 MiB"
(( (CACHE_SIZE & (CACHE_SIZE - 1)) == 0 )) || fail "cache size must be a power of two"
CACHE_RANGES=$((CACHE_SIZE / 2097152))
MAX_CORRECTNESS_RANGES=4096
if (( CACHE_RANGES <= MAX_CORRECTNESS_RANGES )); then
  CORRECTNESS_PROFILE=1
else
  CORRECTNESS_PROFILE=0
fi
case "${DRAGONOS_VIRTIOFS_DAX_REQUIRED:-0}" in
  1|y|Y|yes|YES|true|TRUE|on|ON)
    [[ "${CORRECTNESS_PROFILE}" == "1" ]] ||
      fail "required correctness runs support at most ${MAX_CORRECTNESS_RANGES} cache ranges (8 GiB)"
    ;;
esac
[[ "${VIRTIOFSD_CACHE:-}" == "always" ]] || fail "VIRTIOFSD_CACHE must be always"
command -v sha256sum >/dev/null || fail "sha256sum is required"
command -v timeout >/dev/null || fail "timeout is required"

DEVICE_HELP="$("${QEMU_BIN}" -device vhost-user-fs-pci,help 2>&1 || true)"
grep -q "cache-size" <<<"${DEVICE_HELP}" ||
  fail "QEMU does not expose the experimental virtiofs cache-size property"
grep -q "modern-pio-notify" <<<"${DEVICE_HELP}" ||
  fail "QEMU does not expose modern-pio-notify; the DAX-compatible device cannot be proven"
[[ -x "${VIRTIOFSD_PATH}" ]] || fail "virtiofsd binary was not found"

TMPDIR_PATH="$(mktemp -d "${TMPDIR:-/tmp}/dragonos-dax-preflight.XXXXXX")"
BACKEND_PID=""
cleanup() {
  if [[ -n "${BACKEND_PID}" ]] && kill -0 "${BACKEND_PID}" 2>/dev/null; then
    kill "${BACKEND_PID}" 2>/dev/null || true
    wait "${BACKEND_PID}" 2>/dev/null || true
  fi
  rm -rf "${TMPDIR_PATH}"
  [[ -z "${STAMP_TMP:-}" ]] || rm -f "${STAMP_TMP}"
}
trap cleanup EXIT

SHARE="${TMPDIR_PATH}/share"
SOCKET="${TMPDIR_PATH}/virtiofsd.sock"
mkdir -p "${SHARE}"
virtiofs_build_daemon_command "${VIRTIOFSD_PATH}" "${SOCKET}" "${SHARE}" "always" \
  "${VIRTIOFSD_EXTRA_ARGS:-}"
"${VIRTIOFSD_COMMAND[@]}" >"${TMPDIR_PATH}/virtiofsd.log" 2>&1 &
BACKEND_PID=$!

for _ in $(seq 1 100); do
  [[ -S "${SOCKET}" ]] && break
  kill -0 "${BACKEND_PID}" 2>/dev/null || {
    sed -n '1,120p' "${TMPDIR_PATH}/virtiofsd.log" >&2
    fail "virtiofsd exited before creating its socket"
  }
  sleep 0.05
done
[[ -S "${SOCKET}" ]] || fail "virtiofsd did not create its socket"

QMP_OUTPUT="${TMPDIR_PATH}/qmp.out"
printf '%s\n' '{"execute":"qmp_capabilities"}' '{"execute":"query-pci"}' \
  '{"execute":"quit"}' | timeout 15 "${QEMU_BIN}" \
    -nodefaults -display none -machine q35 -accel tcg -m 128M -S -qmp stdio \
    -object memory-backend-memfd,id=mem,size=128M,share=on \
    -numa node,memdev=mem \
    -chardev "socket,id=char_virtiofs,path=${SOCKET}" \
    -device "vhost-user-fs-pci,id=fs0,chardev=char_virtiofs,tag=preflight,cache-size=${CACHE_SIZE},modern-pio-notify=off" \
    >"${QMP_OUTPUT}" 2>"${TMPDIR_PATH}/qemu.log" || {
      sed -n '1,120p' "${TMPDIR_PATH}/qemu.log" >&2
      fail "QEMU could not realize a connected DAX-capable virtiofs device"
    }
grep -Eq '"qdev_id"[[:space:]]*:[[:space:]]*"fs0"' "${QMP_OUTPUT}" ||
  fail "QMP query-pci did not report the realized virtiofs device"

QEMU_SHA256="$(sha256sum "${QEMU_BIN}" | awk '{print $1}')"
VIRTIOFSD_SHA256="$(sha256sum "${VIRTIOFSD_PATH}" | awk '{print $1}')"
CONFIG_SHA256="$(printf '%s\0%s\0%s\0' "always" "${VIRTIOFSD_EXTRA_ARGS:-}" \
  "cache-size=${CACHE_SIZE},modern-pio-notify=off" | sha256sum | awk '{print $1}')"
QEMU_PATH="$(readlink -f "${QEMU_BIN}")"
VIRTIOFSD_PATH="$(readlink -f "${VIRTIOFSD_PATH}")"
QEMU_VERSION="$("${QEMU_BIN}" --version 2>&1 | head -n 1)"
VIRTIOFSD_VERSION="$("${VIRTIOFSD_PATH}" --version 2>&1 | head -n 1 || true)"
DEVICE_OPTIONS="cache-size=${CACHE_SIZE},modern-pio-notify=off"
mkdir -p "$(dirname -- "${STAMP}")"
STAMP_TMP="$(mktemp "$(dirname -- "${STAMP}")/.dax-preflight.XXXXXX")"
chmod 0600 "${STAMP_TMP}"
{
  printf 'PREFLIGHT_UTC=%q\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf 'QEMU_PATH=%q\n' "${QEMU_PATH}"
  printf 'QEMU_VERSION=%q\n' "${QEMU_VERSION}"
  printf 'QEMU_SHA256=%q\n' "${QEMU_SHA256}"
  printf 'VIRTIOFSD_PATH=%q\n' "${VIRTIOFSD_PATH}"
  printf 'VIRTIOFSD_VERSION=%q\n' "${VIRTIOFSD_VERSION}"
  printf 'VIRTIOFSD_SHA256=%q\n' "${VIRTIOFSD_SHA256}"
  printf 'CONFIG_SHA256=%q\n' "${CONFIG_SHA256}"
  printf 'CACHE_SIZE=%q\n' "${CACHE_SIZE}"
  printf 'CACHE_RANGES=%q\n' "${CACHE_RANGES}"
  printf 'CORRECTNESS_PROFILE=%q\n' "${CORRECTNESS_PROFILE}"
  printf 'DEVICE_OPTIONS=%q\n' "${DEVICE_OPTIONS}"
  printf 'VIRTIOFSD_CACHE=%q\n' "always"
  printf 'VIRTIOFSD_EXTRA_ARGS=%q\n' "${VIRTIOFSD_EXTRA_ARGS:-}"
} >"${STAMP_TMP}"
mv "${STAMP_TMP}" "${STAMP}"
echo "DAX preflight passed; cache ranges: ${CACHE_RANGES}; stamp: ${STAMP}"
if [[ "${CORRECTNESS_PROFILE}" == "0" ]]; then
  echo "warning: windows above ${MAX_CORRECTNESS_RANGES} ranges are not supported by the bounded correctness pressure profile" >&2
fi
