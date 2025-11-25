#!/bin/bash

set -euo pipefail

CONFIG_FILE="config/app-blocklist.toml"
TMP_FILE="${CONFIG_FILE}.tmp"
BACKUP_FILE="${CONFIG_FILE}.bak"

if [ ! -f "$CONFIG_FILE" ]; then
  echo "错误：配置文件不存在: $CONFIG_FILE" >&2
  exit 1
fi

if [ ! -w "$CONFIG_FILE" ]; then
  echo "错误：配置文件不可写: $CONFIG_FILE" >&2
  exit 1
fi

cp "$CONFIG_FILE" "$BACKUP_FILE"

trap 'rm -f "$TMP_FILE"' EXIT

if ! awk '
  # 用于将给出的已注释的 `line` 取消注释, 并保持缩进
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

  # 用于输出一个块，根据 should_uncomment 决定是否取消注释该块
  function emit_block(should_uncomment, i, line) {
    for (i = 1; i <= block_len; i++) {
      line = block[i]
      if (should_uncomment) {
        print uncomment_line(line)
      } else {
        print line
      }
    }
    delete block
    block_len = 0
    in_block = 0
    if (should_uncomment && block_has_target) {
      matched = 1
    }
    block_has_target = 0
  }

  BEGIN {
    in_block = 0
    block_len = 0
    block_has_target = 0
    matched = 0
  }

  # 匹配块开始。如果已在块内，则输出前一个块；然后开始新块。
  /^[[:space:]]*#?[[:space:]]*\[\[blocked_apps\]\]/ {
    if (in_block) {
      emit_block(block_has_target)
    }
    in_block = 1
  }

  {
    if (in_block) {
      block[++block_len] = $0
      tmp = $0
      sub(/^[[:space:]]*# ?/, "", tmp)
      if (tmp ~ /name[[:space:]]*=[[:space:]]*"gvisor syscall tests"/) {
        block_has_target = 1
      }
      next
    }
    print
  }

  END {
    if (in_block) {
      emit_block(block_has_target)
    }
    if (!matched) {
      exit 1
    }
  }
' "$CONFIG_FILE" > "$TMP_FILE"; then
  echo "错误：未找到 gVisor 配置块" >&2
  exit 1
fi

mv "$TMP_FILE" "$CONFIG_FILE"

trap - EXIT
rm -f "$TMP_FILE"
