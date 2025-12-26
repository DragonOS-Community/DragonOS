{
  description = "dragonos-nix-dev";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    fenix.url = "github:nix-community/fenix";
    fenix.inputs.nixpkgs.follows = "nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      fenix,
      flake-utils,
      ...
    }@inputs:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        rust-toolchain = fenix.packages.${system}.fromToolchainFile {
          file = ../../kernel/rust-toolchain.toml;
          sha256 = "sha256-3JA9u08FrvsLdi5dGIsUeQZq3Tpn9RvWdkLus2+5cHs=";
        };
      in
      {
        devShells.default = pkgs.mkShell {
          # 基础工具链
          buildInputs = with pkgs; [
            git
            llvm
            libclang
            rust-toolchain
            gcc
          ];

          env = {
            LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
            USING_DRAGONOS_NIX_ENV = 1;
          };

          # Shell启动脚本
          shellHook = '''';
        };

        # 兼容旧版nix-shell命令
        defaultPackage = self.devShells.${system}.default;
      }
    );
}
