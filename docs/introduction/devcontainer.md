# 使用 devcontainer 开发 DragonOS

本教程以 VSCode 为例，需要装有 Docker 的 Linux。

## 克隆仓库

```shell
git clone https://github.com/DragonOS-Community/DragonOS.git
code DragonOS
```

## 进入 devcontainer 环境

在 VSCode 右下角会有弹窗，选择 `Reopen in Container`。如果不可见，请根据下列步骤来进入：
- 下载 devcontainer 插件
- `ctrl+shift+p` 打开 VSCode 命令面板
- 输入 `devcontainer` 字样，会有 `Reopen in Container` 的选项，点击即会构建 devcontainer 环境

构建可能需要一些时间，尤其 msr 的插件在网络环境不好的情况下容易安装失败。

## 构建 DragonOS！

直接输入

```shell
make run-nographic
```

等待构建，最后会自动进入 DragonOS qemu 环境。

需要退出qemu环境，请输入 `ctrl+a` 然后按 `x`。
