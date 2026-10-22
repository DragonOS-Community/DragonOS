# 校验 disk-image.nix 产物：MBR 单分区从 2048 扇区开始、ext4 无错误、关键文件存在且属主为 root
# 由 flake checks 调用，$img 为待校验镜像
set -euo pipefail

sfdisk -d "$img" | tee table.txt
grep -q '^label: dos$' table.txt
[ "$(grep -c 'start=' table.txt)" = 1 ]
grep -Eq 'start= *2048,.*type=83' table.txt

start=$(sed -n 's/.*start= *\([0-9]*\),.*/\1/p' table.txt)
dd if="$img" of=part.img bs=512 skip="$start" status=none conv=sparse
e2fsck -fn part.img

for path in / /bin/busybox /bin/sh /etc/passwd /root /lib /opt/gvisor /opt/tests/dunitest; do
  info=$(debugfs -R "stat \"$path\"" part.img 2>&1)
  if ! grep -q 'User: *0 *Group: *0' <<<"$info"; then
    echo "missing or not owned by root: $path"
    echo "$info"
    exit 1
  fi
done

touch $out
