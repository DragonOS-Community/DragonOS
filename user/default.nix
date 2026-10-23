{
  lib,
  pkgs,
  nixpkgs,
  system,
  target,
  buildDir,
  testOpt,
  nightlyRustPlatform,
  rootfsType ? "vfat",
  diskPath,
  partitionType ? "mbr",
}:

let
  rootfsImage = import ./rootfs-tar.nix {
    inherit
      lib
      pkgs
      nixpkgs
      system
      target
      testOpt
      nightlyRustPlatform
      ;
  };

  diskImage = import ./disk-image.nix {
    inherit
      lib
      pkgs
      rootfsImage
      rootfsType
      partitionType
      ;
  };

  # 磁盘镜像在 nix store 中构建并缓存；这里只负责在其变化时拷贝出可写副本
  buildScript = pkgs.writeShellApplication {
    name = "dragonos-rootfs";
    runtimeInputs = [
      pkgs.coreutils
      pkgs.util-linux
    ];
    text = ''
      SRC_IMG="${diskImage}/disk.img"
      DST_IMG="${diskPath}"
      STAMP="${buildDir}/.disk-image-${target}.src"

      FORCE=0
      if [ "''${1:-}" = "-f" ] || [ "''${1:-}" = "--force" ]; then
        FORCE=1
      fi

      mkdir -p "${buildDir}" "$(dirname "$DST_IMG")"
      # Keep the lock file: unlinking it would allow callers to lock different
      # inodes. The image and its source stamp form a single transaction.
      exec {lock_fd}>"$DST_IMG.lock"
      flock -x "$lock_fd"

      if [ "$FORCE" = "0" ] && [ -f "$DST_IMG" ] && [ "$(cat "$STAMP" 2>/dev/null)" = "$SRC_IMG" ]; then
        echo "==> Disk image unchanged, skip: $DST_IMG"
        echo "    (use 'nix run .#rootfs-${target} -- --force' to discard changes made inside the guest)"
        exit 0
      fi

      echo "==> Copying disk image to $DST_IMG"
      TEMP_IMG=""
      TEMP_STAMP=""
      trap 'rm -f -- "$TEMP_IMG" "$TEMP_STAMP"' EXIT
      TEMP_IMG=$(mktemp "$DST_IMG.tmp.XXXXXX")
      TEMP_STAMP=$(mktemp "$STAMP.tmp.XXXXXX")
      cp --sparse=always --reflink=auto "$SRC_IMG" "$TEMP_IMG"
      chmod u+w "$TEMP_IMG"
      printf '%s\n' "$SRC_IMG" > "$TEMP_STAMP"
      # Invalidate the old source before replacing its image. A crash between
      # the two publications must force a rebuild, including for the old source.
      rm -f -- "$STAMP"
      mv -f -- "$TEMP_IMG" "$DST_IMG"
      # Publish the stamp last, so a failed copy/publication is retried.
      mv -f -- "$TEMP_STAMP" "$STAMP"
      echo "==> Build complete!"
      echo "    Disk image: $DST_IMG ($(du -h "$DST_IMG" | cut -f1))"
    '';
    passthru = { inherit diskImage; };
  };

in
buildScript
