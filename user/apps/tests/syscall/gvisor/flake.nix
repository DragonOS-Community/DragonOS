{
  description = "gVisor syscall test runner and scripts";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-25.11";
  };

  outputs =
    {
      self,
      nixpkgs,
    }:
    let
      pkgs = import nixpkgs { inherit system; };
      installDir = "share/gvisor-tests";
      system = "x86_64-linux";
    in
    {
      packages.${system}.default = pkgs.callPackage ./default.nix {
        inherit installDir;
      };
    };
}
