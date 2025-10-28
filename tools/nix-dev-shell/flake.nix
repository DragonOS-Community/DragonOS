{
  description = "dragonos-nix-dev";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    fenix.url = "github:nix-community/fenix";
    fenix.inputs.nixpkgs.follows = "nixpkgs";
    utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, fenix, utils, ... } @ inputs:
    utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        rustVer = fenix.packages.${system}.fromToolchainName {
          name = "nightly-2024-11-15";
          sha256 = "sha256-muM2tfsMpo2gsFsofhwHutPWgiPjjuwfBUvYCrwomAY=";
        };
        # 组合工具链并提取二进制路径
        rustToolChain = rustVer.withComponents [
          "cargo"
          "clippy"
          "rust-src"
          "rustfmt"
          "rust-analyzer"
        ];
      in {
        devShells.default = pkgs.mkShell {
          # 基础工具链
          buildInputs = with pkgs; [
            git
            llvm
            libclang
            rustToolChain
          ];

          env = {
              LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
            };


          # Shell启动脚本
          shellHook = ''
          '';
        };

        # 兼容旧版nix-shell命令
        defaultPackage = self.devShells.${system}.default;
      }
    );
}