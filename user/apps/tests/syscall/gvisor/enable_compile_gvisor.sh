#!/bin/bash

CONFIG_FILE=config/app-blocklist.toml

awk '
  BEGIN {block=0}
  /^\[\[blocked_apps\]\]/ {
    block++
  }
  block==1 {
    print "#" $0
    next
  }
  block==1 && /^$/ {
    # 第一个 block 结束
    block=2
  }
  {print}
' "$CONFIG_FILE" > "${CONFIG_FILE}.tmp" && mv "${CONFIG_FILE}.tmp" "$CONFIG_FILE"
