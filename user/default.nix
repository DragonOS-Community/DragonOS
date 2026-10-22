{
  lib,
  pkgs,
  nixpkgs,
  system,
  target,
  buildDir,
  testOpt,
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
    runtimeInputs = [ pkgs.coreutils ];
    text = ''
      SRC_IMG="${diskImage}/disk.img"
      DST_IMG="${diskPath}"
      STAMP="${buildDir}/.disk-image-${target}.src"

      FORCE=0
      if [ "''${1:-}" = "-f" ] || [ "''${1:-}" = "--force" ]; then
        FORCE=1
      fi

      if [ "$FORCE" = "0" ] && [ -f "$DST_IMG" ] && [ "$(cat "$STAMP" 2>/dev/null)" = "$SRC_IMG" ]; then
        echo "==> Disk image unchanged, skip: $DST_IMG"
        echo "    (use 'nix run .#rootfs-${target} -- --force' to discard changes made inside the guest)"
        exit 0
      fi

      echo "==> Copying disk image to $DST_IMG"
      mkdir -p "${buildDir}"
      cp --sparse=always --reflink=auto "$SRC_IMG" "$DST_IMG.tmp"
      chmod u+w "$DST_IMG.tmp"
      mv -f "$DST_IMG.tmp" "$DST_IMG"
      echo "$SRC_IMG" > "$STAMP"
      echo "==> Build complete!"
      echo "    Disk image: $DST_IMG ($(du -h "$DST_IMG" | cut -f1))"
    '';
    passthru = { inherit diskImage; };
  };

in
buildScript
