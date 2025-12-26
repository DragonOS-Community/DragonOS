{
  lib,
  pkgs,
  nixpkgs,
  system,
  target,
  fenix,
  buildDir,
  testOpt,
  rootfsType ? "vfat",
  diskPath,
  partitionType ? "mbr",
}:

let
  image = import ./rootfs-tar.nix {
    inherit
      lib
      pkgs
      nixpkgs
      system
      target
      fenix
      testOpt
      ;
  };

  # 构建脚本 - 在bin/目录下构建
  buildScript = pkgs.writeShellApplication {
    name = "dragonos-rootfs";
    runtimeInputs = [
      pkgs.coreutils
      pkgs.gnutar
      pkgs.libguestfs-with-appliance
      pkgs.findutils
      pkgs.parted
      pkgs.dosfstools
      pkgs.e2fsprogs
      pkgs.util-linux
    ];
    text = ''
      set -euo pipefail

      # Ensure build directory exists
      mkdir -p "${buildDir}"

      OUTPUT_TAR="${buildDir}/rootfs.tar"

      echo "==> Generating rootfs"

      # 创建临时目录
      TEMP_DIR=$(mktemp -d)
      EXTRACT_DIR=""  # 初始化为空，vfat 处理时会赋值
      trap 'chmod +w -R "$TEMP_DIR" 2>/dev/null && rm -rf "$TEMP_DIR"; [ -n "$EXTRACT_DIR" ] && chmod +w -R "$EXTRACT_DIR" 2>/dev/null && rm -rf "$EXTRACT_DIR"' EXIT

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

      echo "==> Building disk image at ${diskPath}"

      # 创建磁盘镜像并初始化文件系统
      echo "  Creating disk image..."
      TEMP_IMG="${diskPath}.tmp"

      # 计算所需磁盘大小：tar包大小 + 1G 缓冲空间
      TAR_SIZE_KB=$(du -k "$FINAL_TAR" | cut -f1)
      DISK_SIZE_KB=$(( TAR_SIZE_KB + 1024 * 1024 ))
      truncate -s "''${DISK_SIZE_KB}K" "$TEMP_IMG"

      # 检查是否使用非特权构建模式（guestfish）
      # shellcheck disable=SC2050
      if [ "''${DRAGONOS_UNPRIVILEGED_BUILD:-0}" = "1" ]; then
        echo "  Using guestfish (unprivileged mode)..."
        export LIBGUESTFS_CACHEDIR=/tmp
        export LIBGUESTFS_BACKEND=direct

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
      else
        echo "  Using loop device (privileged mode, faster)..."

        # 使用 parted 创建分区表和分区
        echo "    Creating partition table..."
        # parted 使用 msdos 而不是 mbr
        PARTED_LABEL="${partitionType}"
        if [ "$PARTED_LABEL" = "mbr" ]; then
          PARTED_LABEL="msdos"
        fi
        parted -s "$TEMP_IMG" mklabel "$PARTED_LABEL"
        parted -s "$TEMP_IMG" mkpart primary ${rootfsType} 1MiB 100%

        # 设置 loop 设备
        echo "    Setting up loop device..."
        LOOP_DEV=$(sudo losetup --find --show --partscan "$TEMP_IMG")
        echo "    Loop device: $LOOP_DEV"

        # 确保清理 loop 设备
        # shellcheck disable=SC2317,SC2329
        cleanup_loop() {
          echo "    Cleaning up loop device..."
          sudo umount "''${LOOP_DEV}p1" 2>/dev/null || true
          sudo losetup -d "$LOOP_DEV" 2>/dev/null || true
        }
        trap 'cleanup_loop; chmod +w -R "$TEMP_DIR" 2>/dev/null && rm -rf "$TEMP_DIR"; [ -n "$EXTRACT_DIR" ] && chmod +w -R "$EXTRACT_DIR" 2>/dev/null && rm -rf "$EXTRACT_DIR"' EXIT

        # 等待分区设备出现
        echo "    Waiting for partition device..."
        for _ in $(seq 1 10); do
          if [ -b "''${LOOP_DEV}p1" ]; then
            break
          fi
          sleep 0.1
        done

        if [ ! -b "''${LOOP_DEV}p1" ]; then
          echo "Error: Partition device ''${LOOP_DEV}p1 not found"
          exit 1
        fi

        # 格式化分区
        echo "    Formatting partition as ${rootfsType}..."
        # shellcheck disable=SC2050
        if [ "${rootfsType}" = "vfat" ]; then
          sudo mkfs.vfat "''${LOOP_DEV}p1"
        elif [ "${rootfsType}" = "ext4" ]; then
          sudo mkfs.ext4 -F "''${LOOP_DEV}p1"
        else
          sudo mkfs -t ${rootfsType} "''${LOOP_DEV}p1"
        fi

        # 挂载分区
        MOUNT_DIR=$(mktemp -d)
        echo "    Mounting partition to $MOUNT_DIR..."
        sudo mount "''${LOOP_DEV}p1" "$MOUNT_DIR"

        # 更新 trap 以包含 MOUNT_DIR
        # shellcheck disable=SC2317,SC2329
        cleanup_loop() {
          echo "    Cleaning up..."
          sudo umount "$MOUNT_DIR" 2>/dev/null || true
          sudo losetup -d "$LOOP_DEV" 2>/dev/null || true
          rm -rf "$MOUNT_DIR" 2>/dev/null || true
        }
        trap 'cleanup_loop; chmod +w -R "$TEMP_DIR" 2>/dev/null && rm -rf "$TEMP_DIR"; [ -n "$EXTRACT_DIR" ] && chmod +w -R "$EXTRACT_DIR" 2>/dev/null && rm -rf "$EXTRACT_DIR"' EXIT

        # 解压 tar 到分区
        echo "    Extracting rootfs to partition..."
        sudo tar -xf "$FINAL_TAR" -C "$MOUNT_DIR"
        sudo chmod 0755 "$MOUNT_DIR"

        # 同步并卸载
        echo "    Syncing and unmounting..."
        sync
        sudo umount "$MOUNT_DIR"
        sudo losetup -d "$LOOP_DEV"
        rm -rf "$MOUNT_DIR"

        # 重置 trap
        trap 'chmod +w -R "$TEMP_DIR" 2>/dev/null && rm -rf "$TEMP_DIR"; [ -n "$EXTRACT_DIR" ] && chmod +w -R "$EXTRACT_DIR" 2>/dev/null && rm -rf "$EXTRACT_DIR"' EXIT
      fi

      mv "$TEMP_IMG" "${diskPath}"

      IMG_SIZE=$(du -h "${diskPath}" | cut -f1)
      echo "  ✓ disk image created ($IMG_SIZE)"

      echo "==> Build complete!"
      echo "    Rootfs tar: $OUTPUT_TAR"
      echo "    Disk image: ${diskPath}"
    '';
  };

in
buildScript
