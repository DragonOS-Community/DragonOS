{
  lib,
  pkgs,
  rootfsImage,
  rootfsType ? "ext4",
  partitionType ? "mbr",
}:

# 在 nix 沙箱内免特权生成带 MBR 分区表的磁盘镜像：
# 单个分区从 1MiB 开始，内核从 /dev/vda1 挂载 rootfs。
# 产物会被 nix 缓存，userland 不变时无需重新生成。
assert lib.assertMsg (
  partitionType == "mbr"
) "disk-image.nix: only mbr partition table is supported";
assert lib.assertMsg (lib.elem rootfsType [
  "ext4"
  "vfat"
]) "disk-image.nix: unsupported rootfsType ${rootfsType}";

let
  partOffsetSectors = 2048;
  partOffsetBytes = partOffsetSectors * 512;

  # 各文件系统的差异：内容大小统计方式、MBR 分区类型、写入命令
  # （$ROOT 为 rootfs 目录，$PART_KB 为分区大小）
  fs =
    if rootfsType == "ext4" then
      {
        prepare = "";
        # 写入时需要 fakeroot 让文件属主为 root
        wrap = "fakeroot -i fakeroot.state --";
        contentKb = ''$(( $(stat -c %s "$LAYER_TAR") / 1024 ))'';
        partType = "83";
        mkfs = ''
          mke2fs -q -t ext4 -F -d "$ROOT" -E offset=${toString partOffsetBytes} disk.img "''${PART_KB}k"
        '';
      }
    else
      {
        # vfat 不支持符号链接，mcopy 会解引用复制。先把绝对链接改写到 $ROOT 下，
        # 避免解引用到构建机自身的 /nix/store；镜像大小也按解引用后的大小计算
        prepare = ''
          chmod -R u+w "$ROOT"
          find "$ROOT" -type l -lname '/*' | while read -r link; do
            ln -sfn "$ROOT$(readlink "$link")" "$link"
          done
        '';
        contentKb = ''$(du -sklL "$ROOT" | cut -f1)'';
        # vfat 没有属主概念，不需要 fakeroot
        wrap = "";
        partType = "c";
        mkfs = ''
          mkfs.vfat --offset ${toString partOffsetSectors} disk.img "$PART_KB"
          mcopy -s -i disk.img@@${toString partOffsetBytes} "$ROOT"/* ::/
        '';
      };
in
pkgs.runCommand "dragonos-disk-image"
  {
    nativeBuildInputs = with pkgs; [
      gnutar
      fakeroot
      e2fsprogs
      util-linux
      dosfstools
      mtools
    ];
  }
  ''
    LAYER_TAR=$(tar -tf ${rootfsImage} | grep '/layer.tar$' | head -1)
    if [ -z "$LAYER_TAR" ]; then
      echo "Error: layer.tar not found in ${rootfsImage}"
      exit 1
    fi
    tar -xf ${rootfsImage} "$LAYER_TAR"

    # fakeroot 保证解包与写入文件系统时属主均为 root（-s/-i 在两次调用间保存属主信息）
    ROOT=$PWD/root
    mkdir "$ROOT"
    fakeroot -s fakeroot.state -- tar -xf "$LAYER_TAR" -C "$ROOT"
    ${fs.prepare}

    # 磁盘大小：rootfs 大小 + 1GiB 余量
    DISK_KB=$(( ${fs.contentKb} + 1024 * 1024 ))
    PART_KB=$(( DISK_KB - ${toString partOffsetBytes} / 1024 ))
    truncate -s "''${DISK_KB}K" disk.img
    echo 'start=${toString partOffsetSectors}, type=${fs.partType}' | sfdisk -q --label dos disk.img

    export ROOT PART_KB
    ${fs.wrap} sh -euc '
      chmod 0755 "$ROOT"
      ${fs.mkfs}
    '

    mkdir -p $out
    mv disk.img $out/disk.img
  ''
