{
  description = "RootFS";

  inputs = {
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    flake-parts.url = "github:hercules-ci/flake-parts";
  };

  outputs = inputs@{ self, nixpkgs, fenix, flake-parts }: flake-parts.lib.mkFlake { inherit inputs; } {
    systems = [
      "x86_64-linux"
    ];
    perSystem = { self', inputs', system, ... }:
      let
        nixpkgs = inputs.nixpkgs;
        fenix = inputs.fenix;
        pkgs = nixpkgs.legacyPackages.${system};
        lib = pkgs.lib;
        syscallTestDir = "/usr/share/gvisor";
        rootfsType = "ext4";
        buildDir = "./bin"; # Specifying temp file location

        mkOutputs = target:
          let
            diskPath = "${buildDir}/disk-image-${target}.img";
            qemuScripts = import ./tools/qemu/default.nix {
              inherit lib pkgs syscallTestDir diskPath;
              # 启动相关参数：
              # 内核位置
              kernel = "${buildDir}/kernel/kernel.elf"; # TODO: make it a drv 用nix构建内核，避免指定相对目录
              # 自动测试项目，指定内核启动环境变量参数 AUTO_TEST
              autotest = "none";
              debug = true;
            };

            startPkg = qemuScripts.${target};
            rootfsPkg = pkgs.callPackage ./user/default.nix {
              inherit lib pkgs nixpkgs fenix system target syscallTestDir rootfsType buildDir diskPath;
            };
          in
          {
            apps = {
              # start-${target} 的产物只是一个shell脚本，因此启动相关的参数，直接在上面修改即可，
              # 脚本不占什么空间所以重复eval也没关系，并且最终产出的脚本可读性更好.
              "start-${target}" = {
                type = "app";
                program = "${startPkg}/bin/run-dragonos";
                meta.description = "以 ${target} 启动DragonOS";
              };
              # rootfs 中涉及到基于docker镜像的rootfs构建，修改了 user/ 下软件包相关内容后，
              # rootfs 的docker镜像会重复构建，并且由于nix特性，副本会全部保留
              # 因此可能会占很多空间，如果要清理空间请执行 nix store gc
              "rootfs-${target}" = {
                type = "app";
                program = "${rootfsPkg}/bin/build-rootfs-image";
                meta.description = "构建 ${target} rootfs 镜像";
              };
            };
            packages = {
              "start-${target}" = startPkg;
              "rootfs-${target}" = rootfsPkg;
            };
          };

        allOutputs = map mkOutputs [ "x86_64" "riscv64" ];
        merged = lib.foldl' (acc: elem: {
          apps = acc.apps // elem.apps;
          packages = acc.packages // elem.packages;
        }) { apps = {}; packages = {}; } allOutputs;

      in {
        inherit (merged) apps packages;
      };
  };
}
