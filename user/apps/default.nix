{
  lib,
  pkgs,
  nixpkgs,
  system,
  target,
  fenix,
  testOpt,
}:

# Return a list of app derivations to be copied into the rootfs.
let
  cross =
    if system == "x86_64-linux" && target == "x86_64" then
      pkgs
    else if target == "riscv64" then
      pkgs.pkgsCross.riscv64
    else
      import nixpkgs {
        localSystem = system;
        crossSystem =
          if target == "x86_64" then "x86_64-unknown-linux-gnu" else abort "Unsupported target: ${target}}";
      };

  cross-musl =
    if system == "x86_64-linux" && target == "x86_64" then
      pkgs.pkgsMusl
    else if target == "riscv64" then
      pkgs.pkgsCross.riscv64-musl
    else
      import nixpkgs {
        localSystem = system;
        crossSystem =
          if target == "x86_64" then
            "x86_64-unknown-linux-musl"
          else
            abort "Unsupported target: ${target}-musl}";
      };

  static =
    if system == "x86_64-linux" && target == "x86_64" then
      pkgs.pkgsStatic
    else if target == "riscv64" then
      import nixpkgs {
        crossSystem = lib.systems.examples.riscv64-musl;
        isStatic = true;
      }
    else
      abort "Unsupported static target: ${target}";

  gvisor-syscall-tests = (
    pkgs.callPackage ./tests/syscall/gvisor {
      inherit fenix system;
      installDir = testOpt.syscall.testDir;
      version = testOpt.syscall.version;
    }
  );

  lmbench-benchmark-tests = (pkgs.callPackage ./tests/benchmark/lmbench { });
in
[
  static.busybox
  static.curl
  static.dropbear
  cross.glibc

  # Simple C utility
  (static.callPackage ./about { })
]
++ lib.optionals (target == "x86_64" && testOpt.syscall.enable) [
  # gvisor test case only included on x86_64
  gvisor-syscall-tests
  lmbench-benchmark-tests
  # TODO: Add debian libcxx deps or FHS
]
