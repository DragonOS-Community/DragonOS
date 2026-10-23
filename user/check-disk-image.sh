#!/usr/bin/env bash
# 校验 disk-image.nix 产物：MBR 单分区从 2048 扇区开始、ext4 无错误、关键文件存在且属主为 root
# 由 flake checks 调用，$img 为待校验镜像
set -euo pipefail
: "${img:?disk image is required}" "${out:?check output is required}"

sfdisk -d "$img" | tee table.txt
grep -q '^label: dos$' table.txt
[ "$(grep -c 'start=' table.txt)" = 1 ]
grep -Eq 'start= *2048,.*type=83' table.txt

start=$(sed -n 's/.*start= *\([0-9]*\),.*/\1/p' table.txt)
dd if="$img" of=part.img bs=512 skip="$start" status=none conv=sparse
e2fsck -fn part.img

for path in / /bin/busybox /bin/sh /etc/passwd /root /lib /var/tmp /opt/gvisor \
  /opt/tests/dunitest /opt/tests/dunitest/dunitest-runner \
  /opt/tests/dunitest/run_tests.sh /opt/tests/dunitest/whitelist.txt \
  /opt/tests/dunitest/no_skip.txt \
  /opt/tests/dunitest/fixtures/ext4_inode_identity.img \
  /opt/tests/dunitest/fixtures/ext4_orphan_recovery.img \
  /opt/tests/dunitest/fixtures/ext4_shared_xattr.img; do
  info=$(debugfs -R "stat \"$path\"" part.img 2>&1)
  if ! grep -q 'User: *0 *Group: *0' <<<"$info"; then
    echo "missing or not owned by root: $path"
    echo "$info"
    exit 1
  fi
done

info=$(debugfs -R 'stat /var/tmp' part.img 2>&1)
if ! grep -Eq 'Type: directory[[:space:]]+Mode:[[:space:]]+01777' <<<"$info"; then
  echo '/var/tmp must be a sticky world-writable directory'
  echo "$info"
  exit 1
fi

touch "$out"
