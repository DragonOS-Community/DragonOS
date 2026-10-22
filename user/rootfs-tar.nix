{
  lib,
  pkgs,
  nixpkgs,
  system,
  target,
  testOpt,
}:

# 产物是一个未压缩的 docker 镜像 tar，其中的 layer.tar 即 rootfs 内容
let
  apps = import ./apps {
    inherit
      lib
      pkgs
      nixpkgs
      system
      target
      testOpt
      ;
  };

  sys-config =
    pkgs.runCommand "sysconfig"
      {
        src = ./sysconfig;
      }
      ''
        mkdir -p $out $out/root
        cp -r $src/* $out/
      '';

  # 使用 buildImage 创建 Docker 镜像（单层），copyToRoot 会连同 /nix/store 闭包一起打包
  dockerImage = pkgs.dockerTools.buildImage {
    name = "busybox-rootfs";
    copyToRoot = [
      sys-config
    ]
    ++ apps;
    keepContentsDirlinks = false;
    # 仅作为 disk-image.nix 的中间产物，不需要压缩
    compressor = "none";
  };

in
dockerImage
