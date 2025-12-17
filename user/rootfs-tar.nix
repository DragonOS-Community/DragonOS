{ pkgs, system, fenix }:

# 产物是一个可以生成 rootfs.tar 的脚本
let
  apps = import ./apps { inherit pkgs system fenix; };

  sys-config = pkgs.runCommand "sysconfig" {
    src = ./sysconfig;
  } ''
    mkdir -p $out
    cp -r $src/* $out/
  '';

  # streamLayeredImage 返回一个脚本，执行后生成 docker tar，不会在 store 中存储完整镜像
  imageStream = pkgs.dockerTools.streamLayeredImage {
    name = "busybox-rootfs";
    tag = "latest";

    contents = [
      sys-config
    ] ++ apps;
  };
  
in imageStream