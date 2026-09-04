# 使用 nix 开发 DragonOS

nix 的引入使得 DragonOS 的开发环境不再依赖手动维护的 `bootstrap.sh` 。现在任意发行版都可通过安装 nix 环境快速构建运行 DragonOS！

## 安装 nix 并启用 flake 功能

参考 https://nixos.org/download/ 安装 Nix: The Nix package manager. （不是 NixOS ！）

参考 https://wiki.nixos.org/wiki/Flakes#Setup 启用 flakes 功能。

- 如果你想体验 nix 带来的声明式管理，又不想更改发行版，尝试 home-manager 并在其上配置启用 flakes、direnv
- 否则可以直接以 nix standalone 的方式安装 flakes，或者每次输入命令时添加 `--experimental-features 'nix-command flakes'`

## 国内镜像加速（推荐）

如果你在国内且没有全局代理，首次拉取依赖可能很慢甚至失败。本仓库已在 `flake.nix` 内置国内镜像配置，使用 `nix develop / nix run` 时会自动生效。

若仍然不生效，建议在用户级配置中追加以下内容（不会覆盖你已有配置）：

```shell
mkdir -p ~/.config/nix
cat >> ~/.config/nix/nix.conf <<'EOF'
# DragonOS Nix mirror (CN)
extra-substituters = https://mirrors.tuna.tsinghua.edu.cn/nix-channels/store https://mirrors.ustc.edu.cn/nix-channels/store
extra-trusted-public-keys = cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=
EOF
```

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

这会生成 `./bin/qemu-system-x86_64.img`

## 启动内核

```shell
nix run .#start-x86_64
```

现在你能看到你的终端载入 DragonOS 了

::: info
需要退出 DragonOS （QEMU）环境，请输入 `ctrl + a`，然后 `x`
:::


## 更多 nix 命令用法及 nix script 维护

- `cd docs && nix run` 构建文档并启动一个 http 服务器
- 如果存储空间告急，`nix store gc` 清理悬空的历史构建副本
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
