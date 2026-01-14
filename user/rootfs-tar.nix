{
  lib,
  pkgs,
  nixpkgs,
  system,
  target,
  fenix,
  testOpt,
}:

# 产物是一个可以生成 rootfs.tar 的脚本
let
  apps = import ./apps {
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

  sys-config =
    pkgs.runCommand "sysconfig"
      {
        src = ./sysconfig;
      }
      ''
        mkdir -p $out
        cp -r $src/* $out/
      '';

  # 使用 buildImage 创建 Docker 镜像（单层）
  # 直接返回 dockerImage，解压逻辑在 default.nix 中处理
  dockerImage = pkgs.dockerTools.buildImage {
    name = "busybox-rootfs";
    copyToRoot = [
      sys-config
    ]
    ++ apps;
    keepContentsDirlinks = false;
  };

in
dockerImage
