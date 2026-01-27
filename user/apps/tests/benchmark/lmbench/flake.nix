{
  description = "LMbench benchmark suite packaged for nix (x86_64-linux only)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};

      # Default installation directory
      defaultInstallDir = "tests/benchmark/lmbench";

      # Import the package definition
      lmbench = import ./default.nix {
        inherit (pkgs) lib pkgs;
        installDir = defaultInstallDir;
      };
    in
    {
      packages.${system} = {
        default = lmbench;
        lmbench = lmbench;
      };

      # Allow overriding installDir
      lib.${system}.mkLmbench =
        {
          installDir ? defaultInstallDir,
        }:
        import ./default.nix {
          inherit (pkgs) lib pkgs;
          inherit installDir;
        };

      apps.${system}.default = {
        type = "app";
        program = "${lmbench}/${defaultInstallDir}/run_tests.sh";
      };
    };
}
