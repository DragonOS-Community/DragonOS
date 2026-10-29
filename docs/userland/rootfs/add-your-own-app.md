# Add Programs / Add Custom Programs!

Thanks to the new userland build system using Nix for management, adding programs has become extremely simple. Below, we'll explain from top to bottom how to add programs to run in DragonOS.

## Concepts

In Nix, a software package is a derivation. So, you just need to define your program as a derivation using Nix, and it becomes an installable package.  
`nixpkgs` also provides many native packages, and even includes a syntax to help us quickly specify statically compiled/cross-compiled versions of packages without manually specifying the toolchain.  
Below, let's first look at how to quickly add a `nixpkgs` package to DragonOS.

## Adding an nixpkgs Package

First, let's look at the part in `user/apps/default.nix` that defines and references `nixpkgs` packages.

<<< ../../../user/apps/default.nix{34-43 nix}


Treat `static` and `cross` as ready-made package prefixes (the equivalent of `pkgs` in other Nix tutorials). They handle dependencies, cross compilation, and static linking for you:

- `cross`: GNU dynamically linked packages; cross compilation is handled automatically
- `cross-musl`: musl dynamically linked packages; also auto-cross
- `static`: musl statically linked packages; also auto-cross

The packages injected here are statically linked, such as `busybox` and `dropbear`. Search for more packages with `nix search github:NixOS/nixpkgs/nixos-25.11 <package_name>` or https://search.nixos.org/packages?channel=25.11 .

```shell
~ ❯ nix search github:NixOS/nixpkgs/nixos-25.11 dropbear
evaluation warning: darwin.iproute2mac has been renamed to iproute2mac
* legacyPackages.x86_64-linux.dropbear (2025.88)
  Small footprint implementation of the SSH 2 protocol
evaluation warning: 'dockerfile-language-server-nodejs' has been renamed to 'dockerfile-language-server'
evaluation warning: beets-stable was aliased to beets, since upstream releases are frequent nowadays
evaluation warning: beets-unstable was aliased to beets, since upstream releases are frequent nowadays
evaluation warning: 'f3d' now build with egl support by default, so `f3d_egl` is deprecated, consider using 'f3d' instead.
evaluation warning: beets-stable was aliased to beets, since upstream releases are frequent nowadays
evaluation warning: beets-unstable was aliased to beets, since upstream releases are frequent nowadays
evaluation warning: 'f3d' now build with egl support by default, so `f3d_egl` is deprecated, consider using 'f3d' instead.
evaluation warning: 'hsa-amd-aqlprofile-bin' has been replaced by 'aqlprofile'.
evaluation warning: 'system' has been renamed to/replaced by 'stdenv.hostPlatform.system'
evaluation warning: 'ethersync' has been renamed to 'teamtype'
evaluation warning: Please replace 'pure-lua' with 'moonlight-nvim' as this name was an error
evaluation warning: windows.mingw_w64_pthreads is deprecated, windows.pthreads should be preferred

~ took 28s ❯
```

What is retrieved here is `legacyPackages.x86_64-linux.dropbear`, indicating that at least this package exists for x86_64. Directly referencing it with `cross.dropbear` means using this package. Using `static.dropbear` would rebuild it due to the lack of a remote build cache (but still saves the trouble of manual configuration).

## Adding a Custom Package

### C/C++
Above, you can also see `(static.callPackage ./about {})`, where the about package is a custom-built one. Let's see how Nix replaces its Makefile:

<<< ../../../user/apps/about/default.nix{nix}


You can also use Nix to package software that is not yet in NixOS (uncommon, especially for non-GUI programs).

More references:
- https://book.divnix.com/ch06-01-simple-c-program.html
- https://ryantm.github.io/nixpkgs/stdenv/stdenv/
- https://wiki.nixos.org/wiki/C

### Rust
For simple Rust programs, use `rustPlatform.buildRustPackage` provided by Nix. See `user/apps/tests/syscall/gvisor/default.nix`.

```nix
{ lib, pkgs, fenix, system, installDir }:

let
  fenixPkgs = fenix.packages.${system};
  toolchain = fenixPkgs.combine (with fenixPkgs; [
    minimal.rustc
    minimal.cargo
  ]);
  rustPlatform = pkgs.makeRustPlatform {
    cargo = toolchain;
    rustc = toolchain;
  };

  runner = rustPlatform.buildRustPackage {
    pname = "gvisor-test-runner-bin";
    version = "0.1.0";

    src = ./runner;
    cargoLock = {
      lockFile = ./runner/Cargo.lock;
    };

    # You can install the binary somewhere other than bin.
    postInstall = ''
      mkdir -p $out/${installDir}
      if [ -f "$out/bin/runner" ]; then
        mv "$out/bin/runner" "$out/${installDir}/gvisor-test-runner"
        # Clean up empty bin directory if it exists, to avoid clutter in symlinkJoin
        rmdir "$out/bin" || true
      fi
    '';
  };

  ...
```

For complex applications and cross-compilation, you can refer to a few examples from fenix:
- https://github.com/nix-community/fenix#examples

TODO: Multiplatform Rust Application
