#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT_PATH="${SCRIPT_DIR}/$(basename -- "${BASH_SOURCE[0]}")"
ENV_FILE="${SCRIPT_DIR}/env.sh"

if [[ "${EUID}" -ne 0 ]]; then
  echo "virtiofsd 需要以 sudo 权限启动，正在尝试提权..."
  exec sudo HOME="${HOME}" bash "${SCRIPT_PATH}" "$@"
fi

if [[ ! -f "${ENV_FILE}" ]]; then
  echo "未找到 ${ENV_FILE}"
  echo "请先执行：cp \"${SCRIPT_DIR}/env.sh.example\" \"${ENV_FILE}\" 并按需修改"
  exit 1
fi

# shellcheck source=/dev/null
source "${ENV_FILE}"

detect_virtiofsd_bin() {
  if [[ -n "${VIRTIOFSD_BIN:-}" ]]; then
    echo "${VIRTIOFSD_BIN}"
    return 0
  fi

  if command -v virtiofsd >/dev/null 2>&1; then
    command -v virtiofsd
    return 0
  fi

  local candidates=(
    "/usr/libexec/virtiofsd"
    "/usr/lib/qemu/virtiofsd"
  )
  local p
  for p in "${candidates[@]}"; do
    if [[ -x "${p}" ]]; then
      echo "${p}"
      return 0
    fi
  done

  return 1
}

VIRTIOFSD_PATH="$(detect_virtiofsd_bin || true)"
if [[ -z "${VIRTIOFSD_PATH}" ]]; then
  echo "找不到 virtiofsd，请安装 qemu/virtiofsd 或在 env.sh 中设置 VIRTIOFSD_BIN"
  exit 1
fi

mkdir -p "${HOST_SHARE_DIR}"
mkdir -p "${RUNTIME_DIR}"
rm -f "${SOCKET_PATH}"

echo "启动 virtiofsd:"
echo "  binary : ${VIRTIOFSD_PATH}"
echo "  shared : ${HOST_SHARE_DIR}"
echo "  socket : ${SOCKET_PATH}"
echo "  cache  : ${VIRTIOFSD_CACHE:-auto}"
echo
echo "保持此终端运行，不要关闭。"

build_virtiofsd_args() {
  local help_text
  help_text="$("${VIRTIOFSD_PATH}" --help 2>&1 || true)"

  VIRTIOFSD_ARGS=("--socket-path=${SOCKET_PATH}")

  if grep -q -- "--shared-dir" <<<"${help_text}"; then
    VIRTIOFSD_ARGS+=("--shared-dir=${HOST_SHARE_DIR}")
    if grep -q -- "--cache" <<<"${help_text}"; then
      VIRTIOFSD_ARGS+=("--cache=${VIRTIOFSD_CACHE:-auto}")
    else
      VIRTIOFSD_ARGS+=("-o" "cache=${VIRTIOFSD_CACHE:-auto}")
    fi
    return 0
  fi

  if grep -q "source=PATH" <<<"${help_text}"; then
    VIRTIOFSD_ARGS+=(
      "-o" "source=${HOST_SHARE_DIR}"
      "-o" "cache=${VIRTIOFSD_CACHE:-auto}"
    )
    return 0
  fi

  VIRTIOFSD_ARGS+=(
    "-o" "cache=${VIRTIOFSD_CACHE:-auto}"
    "${HOST_SHARE_DIR}"
  )
}

build_virtiofsd_args

if [[ -n "${VIRTIOFSD_EXTRA_ARGS:-}" ]]; then
  # shellcheck disable=SC2086
  exec "${VIRTIOFSD_PATH}" "${VIRTIOFSD_ARGS[@]}" ${VIRTIOFSD_EXTRA_ARGS}
else
  exec "${VIRTIOFSD_PATH}" "${VIRTIOFSD_ARGS[@]}"
fi
