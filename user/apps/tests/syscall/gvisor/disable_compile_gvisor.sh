#!/bin/bash

CONFIG_FILE=config/app-blocklist.toml

sed -i \
  -e '/^\s*#\s*\[\[blocked_apps\]\]\s*$/ s/^\s*#\s*//' \
  -e '/^\s*#\s*name\s*=\s*"gvisor syscall tests"\s*$/ s/^\s*#\s*//' \
  -e '/^\s*#\s*reason\s*=\s*"由于文件较大，因此屏蔽。如果要允许系统调用测试，则把这几行取消注释即可"\s*$/ s/^\s*#\s*//' \
  $CONFIG_FILE