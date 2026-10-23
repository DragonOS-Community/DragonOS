# 从软件包到RootFS镜像

定义完软件包后，无需做更多修改，直接 `nix run .#rootfs-${target}` 即可构建 RootFS。

## 生成 Docker 镜像

我们利用 `pkgs.dockerTools.buildImage` 的展平和拷贝属性，将 `user/apps` 下的软件包，以及 `user/sysconfig`
中的配置文件（连同它们在 `/nix/store` 中的依赖闭包）复制到单层镜像中（Docker原理）。
`buildImage` 的产物是一个可以被 `docker load` 的未压缩 tar。见 `rootfs-tar.nix`

<<< ../../../../user/rootfs-tar.nix{nix}


## 生成磁盘镜像

原理：在 nix 沙箱中取出镜像第一层的 `layer.tar`，在 `fakeroot` 下解包，再用 `mke2fs -d` 直接把目录写入
磁盘镜像中偏移 1MiB 处的分区，并用 `sfdisk` 写入 MBR 分区表（内核从 `/dev/vda1` 挂载 rootfs）。
整个过程不需要 root 权限，也不需要 loop 设备。见 `user/disk-image.nix`

<<< ../../../../user/disk-image.nix{nix}

`nix build .#checks.x86_64-linux.rootfs-image-x86_64` 会对生成的镜像进行校验（分区表、`e2fsck`、关键文件属主），见 `user/check-disk-image.sh`。


## nix script 的背后

磁盘镜像是一个普通的 nix derivation，会作为构建产物缓存在 /nix/store 中：`user/` 没有变化时不会重新生成。
多次修改 `user/` 后旧的镜像副本仍会保留，可能占用较多硬盘。 镜像准备选择 `AUTO_TEST=dunit` 或 `AUTO_TEST=syscall` 时（包括 `make test-dunit` 和 `make test-syscall`），始终恢复干净镜像，并丢弃 guest 内的磁盘修改。

hint: 要想释放磁盘空间，执行 `nix store gc`

`nix run .#rootfs-${target}` 运行的脚本（`user/default.nix`）只做一件事：当 store 中的镜像发生变化时，
把它拷贝为可写的 `bin/disk-image-${target}.img`，并在 `bin/.disk-image-${target}.src` 中记录来源。
交互模式下，镜像没有变化时会跳过拷贝，以保留在 DragonOS 中对磁盘所做的修改；`-- --force` 可强制恢复为干净镜像。

<<< ../../../../user/default.nix{nix}

如果你想看看最终的镜像里都有些什么，可以在项目根目录执行
`nix build .#packages.x86_64-linux.rootfs-x86_64.diskImage -o bin/result`，镜像位于 `bin/result/disk.img`。
