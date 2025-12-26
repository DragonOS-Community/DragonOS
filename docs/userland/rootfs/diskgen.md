# 从软件包到RootFS镜像

定义完软件包后，无需做更多修改，直接 `nix run .#rootfs-${target}` 即可构建 RootFS。

## 生成 Docker 镜像

我们利用 `pkgs.dockerTools.buildImage` 的展平和拷贝属性，将 `user/apps` 下的软件包，以及 `user/sysconfig`
中的配置文件复制到单层 overlayfs 中（Docker原理），`buildImage` 的终产物是一个可以被 `docker import` 的一个 
`tar.gz`。见 `rootfs-tar.nix`

```{literalinclude} ../../../user/rootfs-tar.nix
:language: nix
```

## 解压 Docker tar 并生成文件系统镜像

原理：解压 docker 镜像，拿出第一层中的 `layer.tar`，然后使用 `guestfish` 导入到虚拟镜像文件中。见 `user/default.nix`

```{literalinclude} ../../../user/default.nix
:language: shell
:lines: 23-112
```

## nix script 的背后

生成 Docker 镜像时，Docker Image 是作为二进制产物缓存在 /nix/store 中的，因此多次构建可能会快速占用硬盘。

hint: 要想释放磁盘空间，执行 `nix store gc`

但第二步：解压 Docker tar 到文件系统磁盘镜像文件，是以生成一个 shell script 并运行来实现的，生成的 `disk-image-${target}.img` 只会存在一个在 `bin` 目录下；同时从 `.tar.gz` 中解压出来的 `rootfs.tar` 也会存在于 `bin` 目录下，你可以自己解压看看里面都存了些啥。

TODO: 后续脚本允许分两步进行，中间允许注入自定义文件（除了直接在 sysconfig 中注入外）

如果你想看看这个脚本长什么样，你可以在项目根目录执行 `nix build .#rootfs-${target} -o bin/result` 。 nix 会把 script 对应的 derivation 链接到 result 目录下，只需 `cat bin/result/bin/build-rootfs-image` 就能看到全部命令。
