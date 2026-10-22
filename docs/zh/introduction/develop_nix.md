# 使用 nix 开发 DragonOS

nix 的引入使得 DragonOS 的开发环境不再依赖手动维护的 `bootstrap.sh` 。现在任意发行版都可通过安装 nix 环境快速构建运行 DragonOS！

## 安装 nix 并启用 flake 功能

参考 https://nixos.org/download/ 安装 Nix: The Nix package manager. （不是 NixOS ！）

参考 https://wiki.nixos.org/wiki/Flakes#Setup 启用 flakes 功能。

- 如果你想体验 nix 带来的声明式管理，又不想更改发行版，尝试 home-manager 并在其上配置启用 flakes、direnv
- 否则可以直接以 nix standalone 的方式安装 flakes，或者每次输入命令时添加 `--experimental-features 'nix-command flakes'`

## 允许当前用户使用 KVM

nix 启动脚本不使用 `sudo`，QEMU 以当前用户身份运行，因此当前用户需要能访问 `/dev/kvm`（以及可选的 `/dev/vhost-vsock`）。通常加入 `kvm` 用户组并重新登录即可：

```shell
sudo usermod -aG kvm $USER
```

如需 vsock 设备，还需加载 `vhost_vsock` 内核模块。`/dev/kvm` 不可用时，启动脚本会给出提示并回退到 TCG 软件模拟（较慢）。

## 国内镜像加速（推荐）

如果你在国内且没有全局代理，首次拉取依赖可能很慢甚至失败。建议在用户级配置中追加以下内容（不会覆盖你已有配置）：

```shell
mkdir -p ~/.config/nix
cat >> ~/.config/nix/nix.conf <<'EOF'
# DragonOS Nix mirror (CN)
extra-substituters = https://mirrors.tuna.tsinghua.edu.cn/nix-channels/store https://mirrors.ustc.edu.cn/nix-channels/store
EOF
```

::: info
使用多用户安装（nix-daemon）时，非 trusted-user 设置的 `extra-substituters` 会被忽略。此时请在 `/etc/nix/nix.conf`（NixOS 上为 `nix.settings.substituters`）中配置，或把当前用户加入 `trusted-users`。
:::

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
nix develop
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
nix run .#rootfs-x86_64
```

磁盘镜像在 nix store 中构建并缓存，该命令只在镜像发生变化时把它拷贝为 `./bin/disk-image-x86_64.img`。

`user/` 下的软件包没有变化时会直接跳过，从而保留你在 DragonOS 中对磁盘所做的修改；如需丢弃这些修改、恢复为干净镜像，执行 `nix run .#rootfs-x86_64 -- --force`。

## 启动内核

```shell
nix run .#start-x86_64
```

现在你能看到你的终端载入 DragonOS 了

::: info
需要退出 DragonOS （QEMU）环境，请输入 `ctrl + a`，然后 `x`
:::

启动时可以通过环境变量调整行为：

- `QEMU_ACCEL=kvm|tcg`：强制指定加速方式
- `AUTO_TEST=syscall|dunit`：开机后自动运行对应测试
- `QEMU_GDB_WAIT=1`：配合 `nix run .#start-debug-x86_64` 使用，启动后等待 GDB 连接（`nix run .#gdb-x86_64`）

## 一键构建并运行

```shell
nix run .#yolo-x86_64
```

依次执行 `make kernel`、准备 rootfs 镜像、启动 QEMU。编译内核使用的工具链与 `nix develop` 完全一致，不需要事先进入 dev shell。

## 更多 nix 命令用法及 nix script 维护

- `cd docs && nix run` 构建文档并启动一个 http 服务器
- 如果存储空间告急，`nix store gc` 清理悬空的历史构建副本
- 防止开发环境被 `nix store gc` 清理：使用 direnv（`use flake` 会自动保留），或使用 `nix develop --profile .nix-gc-root`
- 项目根目录下 `nix flake show` 查看可供构建的目标
- 更多 nix 相关的用户空间构建详见 Userland 部分

## 冻结新版本文档

历史版的唯一来源是仓库里的 `docs/.vitepress/archives/html/<tag>/`。CI 只把这些目录叠进站点，不再从 secret 或现网拉取。

发布冻结版（例如 `V0.5.0`）时：

1. 在 `docs/` 下执行 `npm run docs:build`。
2. 把 `.vitepress/dist/` 里**不属于** `V*`、`master` 的最新站拷到 `.vitepress/archives/html/V0.5.0/`（`docs:build` 叠进去的历史版不要拷）。
3. 在 [`.vitepress/legacy-tags.json`](../../.vitepress/legacy-tags.json) 最前面加上 `"V0.5.0"`。
4. 把新目录和 JSON 一并提交。下次文档 CI 就会叠上该版本。

`V0.4.0` 及更早是 Sphinx 冻结页：中文 `/V0.4.0/...`，英文 `/V0.4.0/locales/en/...`。从 VitePress 冻结的版本路径是 `/V0.5.0/zh/...` 与 `/V0.5.0/...`，顶栏版本切换到那时需要按新旧布局分别映射。
