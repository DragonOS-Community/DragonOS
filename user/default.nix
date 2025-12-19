{
  pkgs,
  system,
  fenix,
  target,
  buildDir,
  syscallTestDir,
  rootfsType ? "vfat",
  partitionType ? "mbr"
}:

let
  image = import ./rootfs-tar.nix { inherit pkgs system fenix syscallTestDir; };
  diskName = "disk-image-${target}.img";

  # 构建脚本 - 在bin/目录下构建
  buildScript = pkgs.writeShellApplication {
    name = "build-rootfs-image";
    runtimeInputs = [ pkgs.coreutils pkgs.gnutar pkgs.libguestfs-with-appliance pkgs.findutils ];
    text = ''
      set -euo pipefail

      # Ensure build directory exists
      mkdir -p "${buildDir}"

      OUTPUT_TAR="${buildDir}/rootfs.tar"
      DISK_IMAGE="${buildDir}/${diskName}"

      echo "==> Generating rootfs"

      # 创建临时目录
      TEMP_DIR=$(mktemp -d)
      trap 'chmod +w -R "$TEMP_DIR" && rm -rf "$TEMP_DIR"' EXIT

      # 提取 layer.tar (rootfs)
      echo "  Extracting rootfs layer..."
      cd "$TEMP_DIR"
      tar -xzf ${image}

      # 找到 layer.tar 并复制到 bin/
      LAYER_TAR=$(find . -name "layer.tar" | head -1)
      if [ -z "$LAYER_TAR" ]; then
        echo "Error: layer.tar not found in docker image"
        exit 1
      fi

      cp "$LAYER_TAR" "$OLDPWD/$OUTPUT_TAR"
      cd "$OLDPWD"

      TAR_SIZE=$(du -h "$OUTPUT_TAR" | cut -f1)
      echo "  ✓ rootfs.tar created ($TAR_SIZE)"

      # 如果是 vfat 文件系统，需要特殊处理：排除 /nix/store，解引用符号链接
      FINAL_TAR="$OUTPUT_TAR"
      # shellcheck disable=SC2050
      if [ "${rootfsType}" = "vfat" ]; then
        echo "  Processing rootfs for vfat (excluding /nix/store, dereferencing symlinks)..."

        EXTRACT_DIR=$(mktemp -d)
        FILTERED_TAR="${buildDir}/rootfs-filtered.tar"

        # 添加到清理列表
        trap 'chmod +w -R "$TEMP_DIR" "$EXTRACT_DIR" && rm -rf "$TEMP_DIR" "$EXTRACT_DIR"' EXIT

        # 解压原始 tar，排除 /nix/store
        echo "    Extracting and not filtering..."
        # tar --exclude='nix' -xf "$OUTPUT_TAR" -C "$EXTRACT_DIR"
        chmod +w -R "$TEMP_DIR" "$EXTRACT_DIR"
        fakeroot tar --owner=0 --group=0 --numeric-owner --exclude='proc' --exclude='dev' \
            --exclude='sys' -xf "$OUTPUT_TAR" -C "$EXTRACT_DIR" # 当 RootFS 里包含这几个文件夹时会报错

        # 重新打包，解引用符号链接和硬链接
        echo "    Re-packing with dereferenced links..."
        fakeroot tar --owner=0 --group=0 --numeric-owner --dereference --hard-dereference -cf "$FILTERED_TAR" -C "$EXTRACT_DIR" .

        FILTERED_SIZE=$(du -h "$FILTERED_TAR" | cut -f1)
        echo "  ✓ Re-packed rootfs.tar created ($FILTERED_SIZE)"

        FINAL_TAR="$FILTERED_TAR"
      fi

      echo "==> Building disk image at $DISK_IMAGE"

      export LIBGUESTFS_CACHEDIR=/tmp
      export LIBGUESTFS_BACKEND=direct

      # 创建磁盘镜像并初始化文件系统
      echo "  Creating disk image..."
      TEMP_IMG="$DISK_IMAGE.tmp"

      # 计算所需磁盘大小：tar包大小 + 1G 缓冲空间
      TAR_SIZE_KB=$(du -k "$FINAL_TAR" | cut -f1)
      DISK_SIZE_KB=$(( TAR_SIZE_KB + 1024 * 1024 ))
      truncate -s "''${DISK_SIZE_KB}K" "$TEMP_IMG"

      # 使用 guestfish 创建分区并注入 tar
      echo "  Initializing disk and copying rootfs..."
      guestfish -a "$TEMP_IMG" <<EOF
        run
        part-init /dev/sda ${partitionType}
        part-add /dev/sda primary 2048 -2048
        mkfs ${rootfsType} /dev/sda1
        mount /dev/sda1 /
        tar-in $FINAL_TAR /
        chmod 0755 /
        umount /
        sync
        shutdown
      EOF

      mv "$TEMP_IMG" "$DISK_IMAGE"

      IMG_SIZE=$(du -h "$DISK_IMAGE" | cut -f1)
      echo "  ✓ disk image created ($IMG_SIZE)"

      echo "==> Build complete!"
      echo "    Rootfs tar: $OUTPUT_TAR"
      echo "    Disk image: $DISK_IMAGE"
    '';
  };

in buildScript
