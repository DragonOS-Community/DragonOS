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
        rusttoolchain = fenix.packages.${system}.fromToolchainFile{
          file = ../../kernel/rust-toolchain.toml;
        };
      in {
        devShells.default = pkgs.mkShell {
          # 基础工具链
          buildInputs = with pkgs; [
            git
            llvm
            libclang
            rusttoolchain
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