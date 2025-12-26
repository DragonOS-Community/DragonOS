:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: userland/rootfs/add-your-own-app.md

- Translation time: 2025-12-26 10:37:00

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Add Programs / Add Custom Programs!

Thanks to the new userland build system using Nix for management, adding programs has become extremely simple. Below, we'll explain from top to bottom how to add programs to run in DragonOS.

## Concepts

In Nix, a software package is a derivation. So, you just need to define your program as a derivation using Nix, and it becomes an installable package.  
`nixpkgs` also provides many native packages, and even includes syntax to help quickly specify statically compiled/cross-compiled versions of packages without manually specifying the toolchain.  
Below, let's first look at how to quickly add an `nixpkgs` package to DragonOS.

## Adding an nixpkgs Package

First, let's look at the part in `user/apps/default.nix` that defines and references `nixpkgs` packages.

```_translated_label__{literalinclude} ../../../user/apps/default.nix
:language: nix
:lines: 34-43
```

这里我们先简单将 `static` 和 `cross` 等理解为提供好的现成的软件包调用前缀（即，你在其他nix教程中见到的引用软件包时的 `pkgs` 前缀），他们会帮助我们处理好依赖、交叉编译和静态编译的麻烦事~
- `cross` : 使用 GNU 动态链接的软件，需要交叉编译时自动处理
- `cross-musl` : 使用 musl 动态链接的软件，同样自动处理交叉
- `static` : 使用 musl 静态编译的软件，自动处理交叉

显然，在这里，我们注入的都是静态链接的软件包，如 `busybox` 与 `dropbear`。更多的软件包，可以通过 `nix search github:NixOS/nixpkgs/nixos-25.11 <package_name>` 或者 https://search.nixos.org/packages?channel=25.11 快速检索。

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

This retrieves `legacyPackages.x86_64-linux.dropbear`, indicating that at least this package exists for x86_64. Directly referencing it via `cross.dropbear` uses this package. Using `static.dropbear`, however, would rebuild it due to the absence of a remote build cache (but still saves the trouble of manual configuration).

## Adding a Custom Package

### C/C++
Above, you can also see `(static.callPackage ./about {})`, where the about package is a custom build. Let's see how Nix replaces its Makefile:

_en```{literalinclude} ../../../user/apps/about/default.nix
:language: nix
```

你还可以用 nix 工具来迁移目前 NixOS 上没有收录的软件包进来！（当然，这很少见，尤其是非 GUI 的软件）

更多可以参考：
- https://book.divnix.com/ch06-01-simple-c-program.html
- https://ryantm.github.io/nixpkgs/stdenv/stdenv/
- https://wiki.nixos.org/wiki/C

### Rust
对于简单的 Rust 程序，直接使用 nix 提供的 rustPlatform.buildRustPackage 构建即可，参考 `user/apps/tests/syscall/gvisor/default.nix`

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

    # 你可以在这里选择不把binary装在bin目录下
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

For complex applications and cross-compilation, you can refer to several examples from fenix:
- https://github.com/nix-community/fenix#examples

TODO: Multiplatform Rust Application
