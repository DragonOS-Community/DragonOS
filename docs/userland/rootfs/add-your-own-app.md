# 添加程序/添加自定义程序！

由于新的 userland 构建采用了 nix 来管理，添加程序变得非常的简单！下面我们自顶向下地讲解如何添加程序进 DragonOS 里跑

## 概念

nix 中，一个软件包是一个 derivation，所以只需要用 nix 将你的程序定义为一个 derivation，那么他就是一个可供“安装”的软件包。
`nixpkgs` 也提供了许多原生的软件包，甚至有一套语法来帮助我们快速地指定静态编译/交叉编译版本的软件包，而不需要手动指定工具链。
下面，我们先来看看如何快速添加一个 `nixpkgs` 软件包到 DragonOS 中

## 添加一个 nixpkgs 软件包

首先我们来看到 `user/apps/default.nix` 中定义引用 `nixpkgs` 软件包的部分

```{literalinclude} ../../../user/apps/default.nix
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

此处检索出来的是 `legacyPackages.x86_64-linux.dropbear`，说明至少这个软件包存在于 x86_64 上，直接 `cross.dropbear` 引用的就是这个软件包。而使用 `static.dropbear` 则会因为没有远程构建缓存而重新构建（但仍然省去自己手动配置的麻烦）

## 添加一个自定义软件包

### C/C++
上面还可以看到有 `(static.callPackage ./about {})` ，这个 about 软件包即是自定义构建的。我们来看看如何用 nix 取代了它的 Makefile：

```{literalinclude} ../../../user/apps/about/default.nix
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

对于复杂的应用程序，以及交叉编译，可以参考 fenix 的几个例子：
- https://github.com/nix-community/fenix#examples

TODO: Multiplatform Rust Application
