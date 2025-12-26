# 使用 nix 开发 DragonOS

nix 的引入使得 DragonOS 的开发环境不再依赖手动维护的 `bootstrap.sh` 。现在任意发行版都可通过安装 nix 环境快速构建运行 DragonOS！

## 安装 nix 并启用 flake 功能

参考 https://nixos.org/download/ 安装 Nix: The Nix package manager. （不是 NixOS ！）

参考 https://wiki.nixos.org/wiki/Flakes#Setup 启用 flakes 功能。

- 如果你想体验 nix 带来的声明式管理，又不想更改发行版，尝试 home-manager 并在其上配置启用 flakes、direnv
- 否则可以直接以 nix standalone 的方式安装 flakes，或者每次输入命令时添加 `--experimental-features 'nix-command flakes'`

## 克隆仓库

DragonOS 现在在多个托管平台上都有仓库镜像
- `https://github.com/DragonOS-Community/DragonOS.git`
- `https://atomgit.com/DragonOS-Community/DragonOS.git`
- `https://cnb.cool/DragonOS-Community/DragonOS.git`

```shell
git clone https://atomgit.com/DragonOS-Community/DragonOS.git
cd DragonOS
```

## 激活内核编译环境

```shell
nix develop ./tools/nix-dev-shell 
```

如果你配置了 `direnv`，首次进入仓库目录会提示需要执行 `direnv allow`，相当于自动进入了 `nix develop` 环境。

## 编译内核

执行编译

```shell
make kernel
```

默认状态下，这会将内核 elf 编译到 `./bin/kernel/kernel.elf`

## 构建 rootfs

```shell
nix run .#rootfs.x86_64
```

这会生成 `./bin/qemu-system-x86_64.img`

## 启动内核

```shell
nix run .#start.x86_64
```

现在你能看到你的终端载入 DragonOS 了

:::{note}
需要退出 DragonOS （QEMU）环境，请输入 `ctrl + a`，然后 `x`
:::

## 更多 nix 命令用法及 nix script 维护

- `cd docs && nix run` 构建文档并启动一个 http 服务器
- 如果存储空间告急，`nix store gc` 清理悬空的历史构建副本
- 项目根目录下 `nix flake show` 查看可供构建的目标
- 更多 nix 相关的用户空间构建详见 Userland 部分
