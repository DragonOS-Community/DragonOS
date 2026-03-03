(_dadk_rootfs_manifest)=
# DADK RootFS Manifest 配置（非 Nix）

本文说明 DragonOS 当前基于 DADK 的 RootFS 配置方式（对应 `make run` / `make write_diskimage` 这条传统链路），并与仓库中的最新实现保持一致。

:::{note}
如果你使用的是 Nix RootFS 构建链路，请参考同目录下的 `diskgen.md`。
:::

## 1. 单一 RootFS Manifest 入口

当前使用单一入口文件：

- `config/rootfs-manifests/<name>.toml`

示例：

- `config/rootfs-manifests/default.toml`
- `config/rootfs-manifests/ubuntu2404.toml`

通过环境变量选择：

```bash
ROOTFS_MANIFEST=default make run-nographic
ROOTFS_MANIFEST=ubuntu2404 make run-nographic
```

未指定时默认 `default`。

## 2. Manifest 字段说明

示例（`config/rootfs-manifests/ubuntu2404.toml`）：

```toml
[metadata]
name = "ubuntu2404"
arch = "x86_64"

[rootfs]
fs_type = "ext4"
size = "2G"
partition = "mbr"

[base]
image = "ubuntu:24.04"
pull_policy = "if-not-present"

[user]
config_dir = "user/dadk/config/sets/ubuntu2404"
```

字段语义：

- `metadata.arch`：与当前 `ARCH` 一致（不一致会在构建前报错）。
- `rootfs.fs_type`：如 `fat32` / `ext4`。
- `rootfs.size`：磁盘镜像大小（如 `2G`）。
- `rootfs.partition`：分区方案（当前常用 `mbr`）。
- `base.image`：Docker 基础镜像；空字符串表示无 base。
- `base.pull_policy`：镜像拉取策略。
- `user.config_dir`：当前 manifest 对应的用户程序配置目录。

## 3. 包配置目录约定

当前目录结构：

- `user/dadk/config/all/`：存放所有应用 `.toml`。
- `user/dadk/config/sets/<set-name>/`：按场景组织包集合，通常以软链接引用 `../../all/*.toml`。

例如：

- `user/dadk/config/sets/default/`
- `user/dadk/config/sets/ubuntu2404/`

通过在 manifest 中切换 `user.config_dir` 来切换安装包集合。

## 4. 解析与生成文件

构建前会自动调用：

- `tools/rootfs_manifest_resolve.sh`

生成两个文件：

- `config/rootfs.generated.toml`
- `dadk-manifest.generated.toml`

后续 DADK 命令统一基于 `dadk-manifest.generated.toml` 执行。

你也可以手动执行一次检查：

```bash
ROOTFS_MANIFEST=ubuntu2404 ARCH=x86_64 make prepare_rootfs_manifest
```

## 5. 常用命令

### 5.1 完整构建并启动

```bash
ROOTFS_MANIFEST=default make run
ROOTFS_MANIFEST=ubuntu2404 make run-nographic
```

### 5.2 仅写盘镜像

```bash
ROOTFS_MANIFEST=ubuntu2404 SKIP_GRUB=1 make write_diskimage
```

### 5.3 仅构建用户态并安装到 `bin/sysroot`

```bash
ROOTFS_MANIFEST=ubuntu2404 make user
```

## 6. 变更 Manifest 后的重装行为

`user/Makefile` 已实现以下保护逻辑：

- 当 manifest 变化时，会清理 DADK 输出缓存并重装用户程序。
- 当 manifest 标记不一致时，会删除并重建 `bin/sysroot`，避免旧结果污染新配置。

因此通常不需要手动删 `sysroot`，直接按新 `ROOTFS_MANIFEST` 重新执行构建命令即可。
