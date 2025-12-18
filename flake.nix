{
  description = "RootFS";

  inputs = {
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
  };

  outputs = { self, nixpkgs, fenix }:
    let
      # targetSystems = [ "x86_64-linux" "riscv64-linux" ]; # TODO: Support multi-arch
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      target = "x86_64";
      syscallTestDir = "/usr/share/gvisor";
      qemuScripts = import ./tools/qemu/default.nix { 
        lib = pkgs.lib;
        inherit pkgs;
        rootfsDisk = "./bin/disk-image-${target}.img";
        kernel = "./bin/kernel";
        autotest = "none";
        syscallTestDir = syscallTestDir;
      };
    in {
      # packages.${system}.default = ;
      apps.${system} = {
        start.${target} = {
          type = "app";
          program = "${qemuScripts.${target}}/bin/run-dragonos-x86";
        };
        build-rootfs.${target} = {
          type = "app";
          program = "${pkgs.callPackage ./user/default.nix {
            inherit pkgs system fenix target syscallTestDir;
            buildDir = "./bin";
          }}/bin/build-rootfs-image";
        };
      };
    };
}