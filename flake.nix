{
  description = "RootFS";

  nixConfig = {
    # 国内镜像优先，保留官方缓存作为回退
    extra-substituters = [
      "https://mirrors.tuna.tsinghua.edu.cn/nix-channels/store"
      "https://mirrors.ustc.edu.cn/nix-channels/store"
    ];
    extra-trusted-public-keys = [
      "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY="
    ];
  };

  inputs = {
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    flake-parts.url = "github:hercules-ci/flake-parts";
    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{
      self,
      nixpkgs,
      fenix,
      flake-parts,
      treefmt-nix,
    }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        inputs.treefmt-nix.flakeModule
      ];

      systems = [
        "x86_64-linux"
      ];
      perSystem =
        {
          self',
          inputs',
          system,
          ...
        }:
        let
          nixpkgs = inputs.nixpkgs;
          fenix = inputs.fenix;
          pkgs = nixpkgs.legacyPackages.${system};
          lib = pkgs.lib;
          rootfsType = "ext4";
          buildDir = "./bin"; # Specifying temp file location

          rust-toolchain-toml = builtins.fromTOML (builtins.readFile ./kernel/rust-toolchain.toml);
          rust-channel-raw = rust-toolchain-toml.toolchain.channel;
          rust-channel-match = builtins.match "^(stable|beta|nightly)(-([0-9]{4}-[0-9]{2}-[0-9]{2}))?$" rust-channel-raw;
          rust-channel =
            if rust-channel-match == null then rust-channel-raw else builtins.elemAt rust-channel-match 0;
          rust-date =
            if rust-channel-match == null || builtins.elemAt rust-channel-match 2 == null then
              null
            else
              builtins.elemAt rust-channel-match 2;
          rust-components = rust-toolchain-toml.toolchain.components or [ ];
          rust-dist-root = "https://rsproxy.cn/dist";
          rust-toolchain-base = fenix.packages.${system}.toolchainOf {
            root = rust-dist-root;
            channel = rust-channel;
            date = rust-date;
            sha256 = "sha256-3JA9u08FrvsLdi5dGIsUeQZq3Tpn9RvWdkLus2+5cHs=";
          };
          rust-toolchain-raw = rust-toolchain-base.withComponents rust-components;
          rust-toolchain = pkgs.symlinkJoin {
            name = "rust-toolchain-wrapped";
            paths = [ rust-toolchain-raw ];
            nativeBuildInputs = [ pkgs.makeWrapper ];
            postBuild = ''
              for bin in "$out/bin/"*; do
                if [ -x "$bin" ]; then
                  wrapProgram "$bin" --prefix LD_LIBRARY_PATH : ${pkgs.zlib}/lib
                fi
              done
            '';
          };

          testOpt = {
            # 自动测试项目，指定内核启动环境变量参数 AUTO_TEST
            autotest = "none";
            syscall = {
              enable = true;
              testDir = "/opt/gvisor";
              version = "20251218";
            };
          };

          mkOutputs =
            target:
            let
              diskPath = "${buildDir}/disk-image-${target}.img";
              qemuScripts = import ./tools/qemu/default.nix {
                inherit
                  lib
                  pkgs
                  testOpt
                  diskPath
                  ;
                # QEMU 相关参数：
                # 内核位置
                kernel = "${buildDir}/kernel/kernel.elf"; # TODO: make it a drv 用nix构建内核，避免指定相对目录
                # -s -S
                debug = false;
                # 启用 VM 状态管理，与 make qemu 行为保持一致
                vmstateDir = "${buildDir}/vmstate";
              };
              qemuScriptsSystem = import ./tools/qemu/default.nix {
                inherit
                  lib
                  pkgs
                  testOpt
                  diskPath
                  ;
                # QEMU 相关参数：
                # 内核位置
                kernel = "${buildDir}/kernel/kernel.elf"; # TODO: make it a drv 用nix构建内核，避免指定相对目录
                # -s -S
                debug = false;
                # 启用 VM 状态管理，与 make qemu 行为保持一致
                vmstateDir = "${buildDir}/vmstate";
                # 优先使用系统 QEMU，避免 Nix 下载 QEMU 依赖
                preferSystemQemu = true;
              };

              startPkg = qemuScripts.${target};
              startSystemPkg = qemuScriptsSystem.${target};
              rootfsPkg = pkgs.callPackage ./user/default.nix {
                inherit
                  lib
                  pkgs
                  nixpkgs
                  fenix
                  system
                  target
                  testOpt
                  rootfsType
                  buildDir
                  diskPath
                  ;
              };

              # 一键化构建启动脚本 (yolo: You Only Live Once - 一条命令跑通全部)
              runApp = pkgs.writeScriptBin "dragonos-yolo" ''
                #!${pkgs.runtimeShell}
                set -e

                echo "==> Step 1: Building kernel with make kernel..."
                ${pkgs.gnumake}/bin/make kernel

                echo "==> Step 2: Building rootfs (re-evaluating userland packages)..."
                ${pkgs.nix}/bin/nix run .#rootfs-${target}

                echo "==> Step 3: Starting DragonOS..."
                exec ${pkgs.nix}/bin/nix run .#start-${target} -- "$@"
              '';
            in
            {
              apps = {
                # yolo-${target}: 一键化构建启动命令 (make kernel + rootfs + start)
                "yolo-${target}" = {
                  type = "app";
                  program = "${runApp}/bin/dragonos-yolo";
                  meta.description = "一键化构建并启动DragonOS (${target})";
                };
                # start-${target} 的产物只是一个shell脚本，因此启动相关的参数，直接在上面修改即可，
                # 脚本不占什么空间所以重复eval也没关系，并且最终产出的脚本可读性更好.
                "start-${target}" = {
                  type = "app";
                  program = "${startPkg}/bin/dragonos-run";
                  meta.description = "以 ${target} 启动DragonOS";
                };
                "start-system-${target}" = {
                  type = "app";
                  program = "${startSystemPkg}/bin/dragonos-run";
                  meta.description = "以系统 QEMU 启动DragonOS (${target})";
                };
                # rootfs 中涉及到基于docker镜像的rootfs构建，修改了 user/ 下软件包相关内容后，
                # rootfs 的docker镜像会重复构建，并且由于nix特性，副本会全部保留
                # 因此可能会占很多空间，如果要清理空间请执行 nix store gc
                "rootfs-${target}" = {
                  type = "app";
                  program = "${rootfsPkg}/bin/dragonos-rootfs";
                  meta.description = "构建 ${target} rootfs 镜像";
                };
              };
              packages = {
                "yolo-${target}" = runApp;
                "start-${target}" = startPkg;
                "start-system-${target}" = startSystemPkg;
                "rootfs-${target}" = rootfsPkg;
              };
            };

          allOutputs = map mkOutputs [
            "x86_64"
            "riscv64"
          ];
          merged =
            lib.foldl'
              (acc: elem: {
                apps = acc.apps // elem.apps;
                packages = acc.packages // elem.packages;
              })
              {
                apps = { };
                packages = { };
              }
              allOutputs;

        in
        {
          inherit (merged) apps packages;

          # treefmt formatter配置 (使用nixfmt)
          treefmt = {
            projectRootFile = "flake.nix";

            programs = {
              nixfmt = {
                enable = true;
                package = pkgs.nixfmt-rfc-style;
              };
            };

            settings.formatter.nixfmt.excludes = [ ".direnv/**" ];
          };

          devShells.default = pkgs.mkShell {
            # 基础工具链
            buildInputs = with pkgs; [
              git
              llvm
              libclang
              gcc
              rust-toolchain
              zlib
              gnumake
              qemu_kvm
              meson
              ninja
            ];

            env = {
              LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
              USING_DRAGONOS_NIX_ENV = 1;
            };

            # Shell启动脚本
            shellHook = ''
              # 自动创建 GC root，避免 nix store gc 清理开发环境
              if [ -z "''${DRAGONOS_NIX_GC_ROOT:-}" ]; then
                if [ ! -e ".nix-gc-root" ]; then
                  echo "创建 Nix GC root: .nix-gc-root"
                  ${pkgs.nix}/bin/nix build .#devShells.${system}.default -o ./.nix-gc-root >/dev/null 2>&1 || true
                fi
              fi
              echo "欢迎进入 DragonOS Nix 开发环境!"
              echo ""
              echo "要运行 DragonOS，请构建内核、rootfs，再QEMU运行"
              echo ""
              echo "  一键运行:    nix run .#yolo-x86_64"
              echo "  构建内核:    make kernel"
              echo "  构建 rootfs: nix run .#rootfs-x86_64"
              echo "  QEMU 运行:   nix run .#start-x86_64"
              echo ""
              echo "  文档：       https://docs.dragonos.org.cn/"
            '';
          };
        };
    };
}
