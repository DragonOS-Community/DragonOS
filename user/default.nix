{ pkgs, system, fenix, target, buildDir ? "./bin" }:

let
  imageStream = import ./rootfs-tar.nix { inherit pkgs system fenix; };
  diskName = "disk-image-${target}.img";

  # 构建脚本 - 在bin/目录下构建
  buildScript = pkgs.writeShellApplication {
    name = "build-rootfs-image";
    runtimeInputs = [ pkgs.coreutils pkgs.gnutar pkgs.libguestfs ];
    text = ''
      set -euo pipefail
      
      # Ensure build directory exists
      mkdir -p "${buildDir}"
      
      OUTPUT_TAR="${buildDir}/rootfs.tar"
      DISK_IMAGE="${buildDir}/${diskName}"
      
      echo "==> Generating rootfs from docker image stream"
      
      # 创建临时目录
      TEMP_DIR=$(mktemp -d)
      trap 'rm -rf "$TEMP_DIR"' EXIT
      
      # 执行 streamLayeredImage 脚本生成 docker tar
      echo "  Running image stream script..."
      ${imageStream} > "$TEMP_DIR/image.tar"
      
      # 提取 layer.tar (rootfs)
      echo "  Extracting rootfs layer..."
      cd "$TEMP_DIR"
      tar -xf image.tar
      
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
      
      echo "==> Building disk image at $DISK_IMAGE"
      
      TEMP_IMG="$DISK_IMAGE.tmp"
      truncate -s 2G "$TEMP_IMG"
      
      export LIBGUESTFS_CACHEDIR=/tmp
      
      echo "  Partitioning and formatting..."
      guestfish -a "$TEMP_IMG" <<'EOF'
        run
        part-init /dev/sda gpt
        part-add /dev/sda primary 2048 -2048
        mkfs ext4 /dev/sda1
      EOF
      
      echo "  Injecting rootfs..."
      guestfish -a "$TEMP_IMG" -m /dev/sda1 tar-in "$OUTPUT_TAR" / compress:none
      guestfish -a "$TEMP_IMG" -m /dev/sda1 chmod 0755 /
      
      mv "$TEMP_IMG" "$DISK_IMAGE"
      
      IMG_SIZE=$(du -h "$DISK_IMAGE" | cut -f1)
      echo "  ✓ disk image created ($IMG_SIZE)"
      
      echo "==> Build complete!"
      echo "    Rootfs tar: $OUTPUT_TAR"
      echo "    Disk image: $DISK_IMAGE"
    '';
  };

in buildScript 