# DragonOS 关键命令

## 构建与运行

在仓库根目录执行 `make kernel` 编译内核；需要检查内核代码是否存在编译或语法问题时，也使用该命令。

推荐先执行 `nix develop` 进入 Nix 开发环境，再运行仓库构建命令。需要一键完成 x86_64 构建并启动 QEMU 时，执行 `nix run .#yolo-x86_64`。

Nix 安装、flake 配置、分步构建和 QEMU 操作方式以 [docs/introduction/develop_nix.md](docs/introduction/develop_nix.md) 为准。可用 flake 目标可能随仓库演进而变化，需要发现其他目标时在根目录执行 `nix flake show`，不要在本文复制完整目标清单。
