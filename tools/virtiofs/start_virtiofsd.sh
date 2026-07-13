#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT_PATH="${SCRIPT_DIR}/$(basename -- "${BASH_SOURCE[0]}")"
ENV_FILE="${DRAGONOS_VIRTIOFS_ENV_FILE:-${SCRIPT_DIR}/env.sh}"
DAX_CACHE_OVERRIDE="${DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE:-}"
ATTESTATION_OVERRIDE="${DRAGONOS_VIRTIOFS_BACKEND_ATTESTATION:-}"
VIRTIOFSD_BIN_OVERRIDE="${VIRTIOFSD_BIN:-}"
VIRTIOFSD_CACHE_OVERRIDE="${VIRTIOFSD_CACHE:-}"
VIRTIOFSD_EXTRA_OVERRIDE="${VIRTIOFSD_EXTRA_ARGS:-}"

if [[ "${EUID}" -ne 0 ]]; then
  echo "virtiofsd 需要以 sudo 权限启动，正在尝试提权..."
  exec sudo HOME="${HOME}" \
    DRAGONOS_VIRTIOFS_ENV_FILE="${ENV_FILE}" \
    DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE="${DAX_CACHE_OVERRIDE}" \
    DRAGONOS_VIRTIOFS_BACKEND_ATTESTATION="${ATTESTATION_OVERRIDE}" \
    VIRTIOFSD_BIN="${VIRTIOFSD_BIN_OVERRIDE}" \
    VIRTIOFSD_CACHE="${VIRTIOFSD_CACHE_OVERRIDE}" \
    VIRTIOFSD_EXTRA_ARGS="${VIRTIOFSD_EXTRA_OVERRIDE}" \
    bash "${SCRIPT_PATH}" "$@"
fi

if [[ ! -f "${ENV_FILE}" ]]; then
  echo "未找到 ${ENV_FILE}"
  echo "请先执行：cp \"${SCRIPT_DIR}/env.sh.example\" \"${ENV_FILE}\" 并按需修改"
  exit 1
fi

# shellcheck source=/dev/null
source "${ENV_FILE}"
[[ -z "${DAX_CACHE_OVERRIDE}" ]] || DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE="${DAX_CACHE_OVERRIDE}"
[[ -z "${ATTESTATION_OVERRIDE}" ]] || \
  DRAGONOS_VIRTIOFS_BACKEND_ATTESTATION="${ATTESTATION_OVERRIDE}"
[[ -z "${VIRTIOFSD_BIN_OVERRIDE}" ]] || VIRTIOFSD_BIN="${VIRTIOFSD_BIN_OVERRIDE}"
[[ -z "${VIRTIOFSD_CACHE_OVERRIDE}" ]] || VIRTIOFSD_CACHE="${VIRTIOFSD_CACHE_OVERRIDE}"
[[ -z "${VIRTIOFSD_EXTRA_OVERRIDE}" ]] || VIRTIOFSD_EXTRA_ARGS="${VIRTIOFSD_EXTRA_OVERRIDE}"
# shellcheck source=common.sh
source "${SCRIPT_DIR}/common.sh"

VIRTIOFSD_PATH="$(virtiofs_detect_daemon || true)"
if [[ -z "${VIRTIOFSD_PATH}" ]]; then
  echo "找不到 virtiofsd，请安装 qemu/virtiofsd 或在 env.sh 中设置 VIRTIOFSD_BIN"
  exit 1
fi

mkdir -p "${HOST_SHARE_DIR}"
mkdir -p "${RUNTIME_DIR}"

echo "启动 virtiofsd:"
echo "  binary : ${VIRTIOFSD_PATH}"
echo "  shared : ${HOST_SHARE_DIR}"
echo "  socket : ${SOCKET_PATH}"
echo "  cache  : ${VIRTIOFSD_CACHE:-auto}"
echo
echo "保持此终端运行，不要关闭。"

virtiofs_build_daemon_command "${VIRTIOFSD_PATH}" "${SOCKET_PATH}" "${HOST_SHARE_DIR}" \
  "${VIRTIOFSD_CACHE:-auto}" "${VIRTIOFSD_EXTRA_ARGS:-}"

if [[ -z "${DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE:-}" ]]; then
  rm -f "${SOCKET_PATH}"
  exec "${VIRTIOFSD_COMMAND[@]}"
fi

command -v flock >/dev/null || {
  echo "缺少 flock，无法安全管理 DAX virtiofsd 单实例" >&2
  exit 1
}
command -v sha256sum >/dev/null || {
  echo "缺少 sha256sum，无法验证 DAX virtiofsd 身份" >&2
  exit 1
}
exec 9>"${RUNTIME_DIR}/virtiofsd.lock"
flock -n 9 || {
  echo "已有 DAX virtiofsd wrapper 使用 ${RUNTIME_DIR}" >&2
  exit 1
}
SOCKET_LOCK_ID="$(printf '%s' "${SOCKET_PATH}" | sha256sum | awk '{print $1}')"
exec 8>"/run/lock/dragonos-virtiofsd-${SOCKET_LOCK_ID}.lock"
flock -n 8 || {
  echo "已有 virtiofsd wrapper 使用 socket ${SOCKET_PATH}" >&2
  exit 1
}
rm -f "${SOCKET_PATH}"
ATTESTATION="${DRAGONOS_VIRTIOFS_BACKEND_ATTESTATION:-${RUNTIME_DIR}/virtiofsd.attestation}"
mkdir -p "$(dirname -- "${ATTESTATION}")"
rm -f "${ATTESTATION}"

"${VIRTIOFSD_COMMAND[@]}" &
VIRTIOFSD_PID=$!

cleanup() {
  rm -f "${ATTESTATION}"
  [[ -z "${ATTESTATION_TMP:-}" ]] || rm -f "${ATTESTATION_TMP}"
  if kill -0 "${VIRTIOFSD_PID}" 2>/dev/null; then
    kill "${VIRTIOFSD_PID}" 2>/dev/null || true
    for _ in $(seq 1 50); do
      kill -0 "${VIRTIOFSD_PID}" 2>/dev/null || break
      sleep 0.02
    done
    if kill -0 "${VIRTIOFSD_PID}" 2>/dev/null; then
      kill -KILL "${VIRTIOFSD_PID}" 2>/dev/null || true
    fi
    wait "${VIRTIOFSD_PID}" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

for _ in $(seq 1 100); do
  [[ -S "${SOCKET_PATH}" ]] && break
  kill -0 "${VIRTIOFSD_PID}" 2>/dev/null || {
    wait "${VIRTIOFSD_PID}" || true
    echo "virtiofsd 在创建 socket 前退出" >&2
    exit 1
  }
  sleep 0.05
done
[[ -S "${SOCKET_PATH}" ]] || {
  echo "virtiofsd 未能创建 socket: ${SOCKET_PATH}" >&2
  exit 1
}

SOCKET_KERNEL_INODE="$(virtiofs_socket_inode_for_process "${VIRTIOFSD_PID}" "${SOCKET_PATH}" || true)"
[[ -n "${SOCKET_KERNEL_INODE}" ]] || {
  echo "无法确认 virtiofsd socket inode" >&2
  exit 1
}
BINARY_SHA256="$(sha256sum "/proc/${VIRTIOFSD_PID}/exe" | awk '{print $1}')"
COMMAND_SHA256="$(sha256sum "/proc/${VIRTIOFSD_PID}/cmdline" | awk '{print $1}')"
PROCESS_STARTTIME="$(virtiofs_process_starttime "${VIRTIOFSD_PID}")"
ATTESTATION_TMP="$(mktemp "$(dirname -- "${ATTESTATION}")/.virtiofsd.attestation.XXXXXX")"
chmod 0600 "${ATTESTATION_TMP}"
{
  printf 'PID=%q\n' "${VIRTIOFSD_PID}"
  printf 'PROCESS_STARTTIME=%q\n' "${PROCESS_STARTTIME}"
  printf 'BINARY_SHA256=%q\n' "${BINARY_SHA256}"
  printf 'COMMAND_SHA256=%q\n' "${COMMAND_SHA256}"
  printf 'SOCKET_KERNEL_INODE=%q\n' "${SOCKET_KERNEL_INODE}"
} >"${ATTESTATION_TMP}"
mv "${ATTESTATION_TMP}" "${ATTESTATION}"
echo "  attestation: ${ATTESTATION}"

set +e
wait "${VIRTIOFSD_PID}"
STATUS=$?
set -e
exit "${STATUS}"
