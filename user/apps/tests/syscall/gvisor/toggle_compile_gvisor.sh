#!/bin/bash

set -euo pipefail

usage() {
  cat >&2 <<'EOF'
用法: toggle_compile_gvisor.sh <enable|disable>
  enable  注释 gVisor 配置行，允许编译
  disable 取消注释 gVisor 配置行，继续屏蔽
EOF
  exit 1
}

ACTION="${1:-}"
shift || true

case "$ACTION" in
  enable|disable) ;;
  *) usage ;;
esac

SCRIPT_DIR="$(cd -- "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/../../../../.." && pwd)"
CONFIG_FILE="$REPO_ROOT/config/app-blocklist.toml"
TMP_FILE="${CONFIG_FILE}.tmp"
TARGET_PATTERN='name[[:space:]]*=[[:space:]]*"gvisor syscall tests"'

if [ ! -f "$CONFIG_FILE" ]; then
  echo "错误：配置文件不存在: $CONFIG_FILE" >&2
  exit 1
fi

if [ ! -w "$CONFIG_FILE" ]; then
  echo "错误：配置文件不可写: $CONFIG_FILE" >&2
  exit 1
fi

cleanup() {
  rm -f "$TMP_FILE"
}
trap cleanup EXIT

if ! awk -v action="$ACTION" -v pattern="$TARGET_PATTERN" '
  function comment_line(line, indent, rest) {
    if (line ~ /^[[:space:]]*$/) {
      return line
    }
    if (line ~ /^[[:space:]]*#/) {
      return line
    }
    match(line, /^[[:space:]]*/)
    indent = substr(line, 1, RLENGTH)
    rest = substr(line, RLENGTH + 1)
    if (rest == "") {
      return indent "#"
    }
    return indent "# " rest
  }

  function uncomment_line(line, indent, rest) {
    if (line ~ /^[[:space:]]*$/) {
      return line
    }
    match(line, /^[[:space:]]*/)
    indent = substr(line, 1, RLENGTH)
    rest = substr(line, RLENGTH + 1)
    while (rest ~ /^#/) {
      sub(/^# ?/, "", rest)
    }
    return indent rest
  }

  BEGIN {
    matched = 0
  }

  {
    line = $0
    tmp = line
    sub(/^[[:space:]]*# ?/, "", tmp)
    if (!matched && tmp ~ pattern) {
      if (action == "enable") {
        if (line ~ /^[[:space:]]*#/) {
          print line
        } else {
          print comment_line(line)
        }
      } else {
        if (line ~ /^[[:space:]]*#/) {
          print uncomment_line(line)
        } else {
          print line
        }
      }
      matched = 1
      next
    }
    print line
  }

  END {
    if (!matched) {
      exit 1
    }
  }
' "$CONFIG_FILE" > "$TMP_FILE"; then
  echo "错误：未找到 gVisor 配置行" >&2
  exit 1
fi

mv "$TMP_FILE" "$CONFIG_FILE"
trap - EXIT
cleanup

