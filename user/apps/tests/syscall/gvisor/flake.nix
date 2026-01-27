{
  description = "gVisor syscall test runner and scripts";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-25.11";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      fenix,
      flake-utils,
    }:
    let
      pkgs = import nixpkgs { inherit system; };
      installDir = "share/gvisor-tests";
      system = "x86_64-linux";
    in
    {
      packages.${system}.default = pkgs.callPackage ./default.nix {
        inherit fenix system installDir;
      };
    };
}
