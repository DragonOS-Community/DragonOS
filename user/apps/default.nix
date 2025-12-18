{ pkgs, system, fenix, syscallTestDir }:

# Return a list of app derivations to be copied into the rootfs.
let
  static = pkgs.pkgsStatic;
in [
  static.busybox
  static.curl
  static.dropbear
  pkgs.glibc
  
  # Simple C utility
  (static.callPackage ./about {})

  # gVisor syscall tests runner + assets
  (pkgs.callPackage ./tests/syscall/gvisor {
    fenix = fenix;
    system = system;
    installDir = syscallTestDir;
  })
]
