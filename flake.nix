{
  description = "DragonOS development environment, rootfs and QEMU runner";

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
          rust-components-from-toml = rust-toolchain-toml.toolchain.components or [ ];
          # rust-toolchain.toml 只列出额外组件，构建必需的组件在此补齐
          rust-required-components = [
            "cargo"
            "rustc"
            "rust-std"
          ];
          rust-components-selected = lib.unique (rust-components-from-toml ++ rust-required-components);
          rust-dist-root = "https://rsproxy.cn/dist";
          rust-toolchain-base = fenix.packages.${system}.toolchainOf {
            root = rust-dist-root;
            channel = rust-channel;
            date = rust-date;
            # Fixed-output hash for Rust channel metadata.
            # Update this when rust-toolchain channel/date changes:
            # 1) run nix develop / nix run
            # 2) copy the "got: sha256-..." from mismatch error to this field
            # 3) re-run nix develop and verify rustc/cargo come from rust-toolchain-wrapped/bin
            sha256 = "sha256-ggvRZZFjlAlrZVjqul/f/UpU5CEhDbdKZU0OCR8Uzbc=";
          };
          rust-toolchain-raw = rust-toolchain-base.withComponents rust-components-selected;
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

          # 与内核相同的 nightly 工具链，供需要 nightly 特性的宿主辅助程序使用
          rust-nightly-platform = pkgs.makeRustPlatform {
            cargo = rust-toolchain-raw;
            rustc = rust-toolchain-raw;
          };

          # 编译内核所需的工具，devShell 与 yolo 共用，避免依赖宿主机工具链
          kernelBuildInputs = with pkgs; [
            git
            llvm
            libclang
            gcc
            rust-toolchain
            zlib
            gnumake
            meson
            ninja
          ];
          kernelBuildEnv = {
            LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
            USING_DRAGONOS_NIX_ENV = "1";
          };

          testOpt = {
            # 自动测试项目，指定内核启动环境变量参数 AUTO_TEST
            autotest = "none"; # syscall / dunit
            syscall = {
              enable = true;
              testDir = "/opt/gvisor";
              version = "20251218";
            };
            # dunitest's nix package is broken on master since cad60340 (#2434):
            # its Makefile now generates ext4 fixtures from
            # kernel/crates/another_ext4 sources that are not part of the
            # derivation's sourceByRegex src, so `nix run .#rootfs-*` fails.
            # Disable until the dunitest nix package ships the ext4 fixture
            # generator (or its sources) properly.
            dunitest = {
              enable = false;
              testDir = "/opt/tests/dunitest";
            };
          };

          mkOutputs =
            target:
            let
              diskPath = "${buildDir}/disk-image-${target}.img";
              # vsock 固定配置（nix run .#start-* / .#yolo-* 生效）：
              # - guestCid 不能为 2（host CID）
              vsockConfig = {
                enable = true;
                guestCid = 3;
                deviceModel = "vhost-vsock-pci-non-transitional";
              };
              mkStart =
                extra:
                (import ./tools/qemu/default.nix (
                  {
                    inherit
                      lib
                      pkgs
                      testOpt
                      diskPath
                      ;
                    # 内核位置
                    kernel = "${buildDir}/kernel/kernel.elf"; # TODO: make it a drv 用nix构建内核，避免指定相对目录
                    enableVsock = vsockConfig.enable;
                    vsockGuestCid = vsockConfig.guestCid;
                    vsockDeviceModel = vsockConfig.deviceModel;
                    # 启用 VM 状态管理，与 make qemu 行为保持一致
                    vmstateDir = "${buildDir}/vmstate";
                  }
                  // extra
                )).${target};

              startPkg = mkStart { };
              # 优先使用系统 QEMU，避免 Nix 下载 QEMU 依赖
              startSystemPkg = mkStart { preferSystemQemu = true; };
              # 开启 GDB stub 与 QEMU trace，用于调试
              startDebugPkg = mkStart { debug = true; };
              rootfsPkg = pkgs.callPackage ./user/default.nix {
                inherit
                  lib
                  pkgs
                  nixpkgs
                  system
                  target
                  testOpt
                  rootfsType
                  buildDir
                  diskPath
                  ;
                nightlyRustPlatform = rust-nightly-platform;
              };

              gdbBin = if target == "x86_64" then "rust-gdb" else "gdb-multiarch";
              gdbScript = pkgs.writeScriptBin "dragonos-gdb" ''
                #!${pkgs.runtimeShell}
                GDB_PORT_FILE="${buildDir}/vmstate/gdb"
                if [ ! -f "$GDB_PORT_FILE" ]; then
                  echo "Error: VM not running or GDB port not allocated"
                  echo "Start VM first: nix run .#start-debug-''${target}"
                  exit 1
                fi
                GDB_PORT=$(cat "$GDB_PORT_FILE")
                GDB_INIT_TMP=$(mktemp)
                trap "rm -f $GDB_INIT_TMP" EXIT
                sed "s/{{GDB_PORT}}/$GDB_PORT/" ${./tools/.gdbinit} > "$GDB_INIT_TMP"
                echo "Connecting to GDB port: $GDB_PORT"
                exec ${gdbBin} -n -x "$GDB_INIT_TMP"
              '';

              # 一键化构建启动脚本 (yolo: You Only Live Once - 一条命令跑通全部)
              runApp = pkgs.writeShellApplication {
                name = "dragonos-yolo";
                # stdenv.initialPath 提供 mkShell 默认带有的基础工具（coreutils、bash 等）
                runtimeInputs = kernelBuildInputs ++ pkgs.stdenv.initialPath;
                text = ''
                  # 宿主的 LD_LIBRARY_PATH 可能与 nix 构建的工具不兼容
                  unset LD_LIBRARY_PATH
                  ${lib.concatStringsSep "\n" (lib.mapAttrsToList (k: v: "export ${k}=${v}") kernelBuildEnv)}

                  echo "==> Step 1: Building kernel with make kernel..."
                  make kernel

                  echo "==> Step 2: Preparing rootfs disk image..."
                  ${rootfsPkg}/bin/dragonos-rootfs

                  echo "==> Step 3: Starting DragonOS..."
                  exec ${startPkg}/bin/dragonos-run "$@"
                '';
              };
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
                "start-debug-${target}" = {
                  type = "app";
                  program = "${startDebugPkg}/bin/dragonos-run";
                  meta.description = "以调试模式启动DragonOS (开启GDB stub, ${target})";
                };
                # rootfs 中涉及到基于docker镜像的rootfs构建，修改了 user/ 下软件包相关内容后，
                # rootfs 的docker镜像会重复构建，并且由于nix特性，副本会全部保留
                # 因此可能会占很多空间，如果要清理空间请执行 nix store gc
                "rootfs-${target}" = {
                  type = "app";
                  program = "${rootfsPkg}/bin/dragonos-rootfs";
                  meta.description = "构建 ${target} rootfs 镜像";
                };
                "gdb-${target}" = {
                  type = "app";
                  program = "${gdbScript}/bin/dragonos-gdb";
                  meta.description = "Connect GDB to running DragonOS (${target})";
                };
              };
              packages = {
                "yolo-${target}" = runApp;
                "start-${target}" = startPkg;
                "start-system-${target}" = startSystemPkg;
                "start-debug-${target}" = startDebugPkg;
                "rootfs-${target}" = rootfsPkg;
                "gdb-${target}" = gdbScript;
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
            buildInputs = kernelBuildInputs ++ [ pkgs.qemu_kvm ];

            env = kernelBuildEnv;

            shellHook = ''
              echo "欢迎进入 DragonOS Nix 开发环境! ($(rustc -V))"
              echo ""
              echo "  一键运行:    nix run .#yolo-x86_64"
              echo "  构建内核:    make kernel"
              echo "  构建 rootfs: nix run .#rootfs-x86_64"
              echo "  QEMU 运行:   nix run .#start-x86_64"
              echo ""
              echo "  文档：       https://docs.dragonos.org.cn/"
            '';
          };

          checks.rootfs-image-x86_64 = pkgs.runCommand "check-rootfs-image-x86_64" {
            nativeBuildInputs = with pkgs; [
              util-linux
              e2fsprogs
            ];
            img = "${self'.packages.rootfs-x86_64.diskImage}/disk.img";
          } (builtins.readFile ./user/check-disk-image.sh);
        };
    };
}
