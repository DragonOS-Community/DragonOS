:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: userland/rootfs/diskgen.md

- Translation time: 2025-12-26 10:51:55

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# From Software Packages to RootFS Image

After defining the software packages, no further modifications are needed. Simply `nix run .#rootfs-${target}` to build the RootFS.

## Generating a Docker Image

We leverage the flattening and copying properties of `pkgs.dockerTools.buildImage` to copy the packages under `user/apps` and the configuration files in `user/sysconfig` into a single-layer overlayfs (following the Docker principle). The final output of `buildImage` is a `tar.gz` that can be used by `docker import`. See `rootfs-tar.nix`.

```{literalinclude} ../../../user/rootfs-tar.nix
:language: nix
```

## 解压 Docker tar 并生成文件系统镜像

原理：解压 docker 镜像，拿出第一层中的 `layer.tar`，然后使用 `guestfish` 导入到虚拟镜像文件中。见 `user/default.nix`

```{literalinclude} ../../../user/default.nix
:language: shell
:lines: 23-112
```

## Behind the Nix Script

When generating a Docker image, the Docker Image is cached as a binary artifact in /nix/store, so multiple builds may quickly consume disk space.

hint: To free up disk space, run `nix store gc`

However, the second step—extracting the Docker tar to a filesystem disk image file—is implemented by generating and running a shell script. The generated `disk-image-${target}.img` will only exist in the `bin` directory. Meanwhile, the `rootfs.tar` extracted from `.tar.gz` will also reside in the `bin` directory. You can inspect it yourself to see what's inside.

TODO: Future scripts will allow a two-step process, with the option to inject custom files in between (in addition to direct injection via sysconfig).

If you want to see what this script looks like, you can run `nix build .#rootfs-${target} -o bin/result` in the project root directory. Nix will link the script's corresponding derivation to the result directory. Simply `cat bin/result/bin/build-rootfs-image` to view all the commands.
