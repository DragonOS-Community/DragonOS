#!/usr/bin/env bash
set -euo pipefail

[[ $# -eq 4 ]] || {
  echo "usage: $0 ATTESTATION SOCKET BINARY_SHA256 COMMAND_SHA256" >&2
  exit 2
}

ATTESTATION="$1"
SOCKET="$2"
EXPECTED_BINARY_SHA="$3"
EXPECTED_COMMAND_SHA="$4"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=common.sh
source "${SCRIPT_DIR}/common.sh"

[[ "${EUID}" -eq 0 ]] || {
  echo "backend attestation verification requires root" >&2
  exit 1
}
[[ -f "${ATTESTATION}" && ! -L "${ATTESTATION}" ]] || {
  echo "missing regular backend attestation: ${ATTESTATION}" >&2
  exit 1
}
mode="$(stat -c '%a' "${ATTESTATION}")"
owner="$(stat -c '%u' "${ATTESTATION}")"
[[ "${owner}" == "0" && "${mode}" == "600" ]] || {
  echo "backend attestation must be root-owned mode 0600" >&2
  exit 1
}

backend_pid="$(awk -F= '$1 == "PID" { print $2 }' "${ATTESTATION}")"
backend_starttime="$(awk -F= '$1 == "PROCESS_STARTTIME" { print $2 }' "${ATTESTATION}")"
backend_binary_sha="$(awk -F= '$1 == "BINARY_SHA256" { print $2 }' "${ATTESTATION}")"
backend_command_sha="$(awk -F= '$1 == "COMMAND_SHA256" { print $2 }' "${ATTESTATION}")"
backend_socket_inode="$(awk -F= '$1 == "SOCKET_KERNEL_INODE" { print $2 }' "${ATTESTATION}")"
[[ "${backend_pid}" =~ ^[0-9]+$ && "${backend_starttime}" =~ ^[0-9]+$ &&
   "${backend_socket_inode}" =~ ^[0-9]+$ && "${backend_binary_sha}" =~ ^[0-9a-f]{64}$ &&
   "${backend_command_sha}" =~ ^[0-9a-f]{64}$ ]] || {
  echo "malformed backend attestation" >&2
  exit 1
}
kill -0 "${backend_pid}" 2>/dev/null || {
  echo "attested virtiofsd process is not running" >&2
  exit 1
}

live_starttime="$(virtiofs_process_starttime "${backend_pid}")"
live_binary_sha="$(sha256sum "/proc/${backend_pid}/exe" | awk '{print $1}')"
live_command_sha="$(sha256sum "/proc/${backend_pid}/cmdline" | awk '{print $1}')"
live_socket_inode="$(virtiofs_socket_inode_for_process "${backend_pid}" "${SOCKET}" || true)"
[[ "${backend_starttime}" == "${live_starttime}" &&
   "${backend_binary_sha}" == "${live_binary_sha}" &&
   "${backend_binary_sha}" == "${EXPECTED_BINARY_SHA}" &&
   "${backend_command_sha}" == "${live_command_sha}" &&
   "${backend_command_sha}" == "${EXPECTED_COMMAND_SHA}" &&
   "${backend_socket_inode}" == "${live_socket_inode}" ]] || {
  echo "live virtiofsd identity does not match its attestation" >&2
  exit 1
}
virtiofs_process_holds_socket "${backend_pid}" "${backend_socket_inode}" || {
  echo "attested virtiofsd no longer holds the configured socket" >&2
  exit 1
}
