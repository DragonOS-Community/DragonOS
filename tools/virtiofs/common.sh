#!/usr/bin/env bash

virtiofs_detect_daemon() {
  if [[ -n "${VIRTIOFSD_BIN:-}" ]]; then
    if [[ -x "${VIRTIOFSD_BIN}" ]]; then
      readlink -f "${VIRTIOFSD_BIN}"
      return 0
    fi
    return 1
  fi

  if command -v virtiofsd >/dev/null 2>&1; then
    readlink -f "$(command -v virtiofsd)"
    return 0
  fi

  local candidate
  for candidate in /usr/libexec/virtiofsd /usr/lib/qemu/virtiofsd; do
    if [[ -x "${candidate}" ]]; then
      readlink -f "${candidate}"
      return 0
    fi
  done
  return 1
}

virtiofs_build_daemon_args() {
  local binary="$1"
  local socket="$2"
  local share="$3"
  local cache="$4"
  local help_text
  help_text="$("${binary}" --help 2>&1 || true)"

  VIRTIOFSD_ARGS=("--socket-path=${socket}")
  if grep -q -- "--shared-dir" <<<"${help_text}"; then
    VIRTIOFSD_ARGS+=("--shared-dir=${share}")
    if grep -q -- "--cache" <<<"${help_text}"; then
      VIRTIOFSD_ARGS+=("--cache=${cache}")
    else
      VIRTIOFSD_ARGS+=("-o" "cache=${cache}")
    fi
  elif grep -q "source=PATH" <<<"${help_text}"; then
    VIRTIOFSD_ARGS+=("-o" "source=${share}" "-o" "cache=${cache}")
  else
    VIRTIOFSD_ARGS+=("-o" "cache=${cache}" "${share}")
  fi
}

virtiofs_build_daemon_command() {
  local binary="$1"
  local socket="$2"
  local share="$3"
  local cache="$4"
  local extra="$5"
  local -a extra_args=()

  virtiofs_build_daemon_args "${binary}" "${socket}" "${share}" "${cache}"
  if [[ -n "${extra}" ]]; then
    read -r -a extra_args <<<"${extra}"
  fi
  VIRTIOFSD_COMMAND=("${binary}" "${VIRTIOFSD_ARGS[@]}" "${extra_args[@]}")
}

virtiofs_command_sha256() {
  printf '%s\0' "${VIRTIOFSD_COMMAND[@]}" | sha256sum | awk '{print $1}'
}

virtiofs_process_holds_socket() {
  local pid="$1"
  local inode="$2"
  local fd target
  for fd in "/proc/${pid}/fd/"*; do
    target="$(readlink "${fd}" 2>/dev/null || true)"
    if [[ "${target}" == "socket:[${inode}]" ]]; then
      return 0
    fi
  done
  return 1
}

virtiofs_socket_inode_for_process() {
  local pid="$1"
  local expected_path="$2"
  local num ref_count protocol flags type state inode path match=""
  while read -r num ref_count protocol flags type state inode path; do
    [[ "${path:-}" == "${expected_path}" ]] || continue
    virtiofs_process_holds_socket "${pid}" "${inode}" || continue
    [[ -z "${match}" ]] || return 1
    match="${inode}"
  done </proc/net/unix
  [[ -n "${match}" ]] || return 1
  printf '%s\n' "${match}"
}

virtiofs_process_starttime() {
  local pid="$1"
  local stat rest
  [[ -r "/proc/${pid}/stat" ]] || return 1
  stat="$(<"/proc/${pid}/stat")"
  rest="${stat##*) }"
  set -- ${rest}
  [[ $# -ge 20 ]] || return 1
  printf '%s\n' "${20}"
}
