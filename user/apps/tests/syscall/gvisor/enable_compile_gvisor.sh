#!/bin/bash

CONFIG_FILE=config/app-blocklist.toml

sed -i -E \
  -e 's/^[[:space:]]*#*[[:space:]]*(\[\[blocked_apps\]\])[[:space:]]*$/# \1/' \
  -e 's/^[[:space:]]*#*[[:space:]]*(name[[:space:]]*=[[:space:]]*"gvisor syscall tests")[[:space:]]*$/# \1/' \
  -e 's/^[[:space:]]*#*[[:space:]]*(reason[[:space:]]*=[[:space:]]*"由于文件较大，因此屏蔽。如果要允许系统调用测试，则把这几行取消注释即可")[[:space:]]*$/# \1/' \
  $CONFIG_FILE  