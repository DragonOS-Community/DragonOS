# RootFS 基础镜像

本文说明 `v0.6.0` 引入的 RootFS 新能力：`ext4` 文件系统、Docker base 导入、base 变更自动重建。

## 1. 能力概览

从 `v0.6.0` 开始，DADK 的 `rootfs` 支持：

- `fs_type = "ext4"`（原有 `fat32` 仍可用）
- 从 Docker 镜像导入基础 rootfs（如 `ubuntu:24.04`）
- 记录 base 元数据并在 base 变化时自动重建镜像

## 2. 配置方式

在 `rootfs.toml` 中配置：

```toml
[metadata]
fs_type = "ext4"
size = "4G"

[partition]
type = "mbr"

[base]
image = "ubuntu:24.04"          # 为空表示不使用 base
pull_policy = "if-not-present"  # always | if-not-present | never
```

说明：

- `metadata.fs_type`：可选 `fat32` / `ext4`
- `base.image`：Docker 镜像名；为空表示从空 rootfs 开始
- `base.pull_policy`：
  - `always`：每次都 `docker pull`
  - `if-not-present`：本地不存在时拉取
  - `never`：仅用本地缓存，不拉取

## 3. 设计说明

`dadk rootfs create` 的核心流程：

1. 创建 raw 磁盘镜像。
2. 按 `partition.type` 分区并格式化（`ext4` 时执行 `mkfs.ext4 -F`）。
3. 若配置了 `base.image`：
   - `docker create <image>`
   - `docker export <container> | tar -xpf - -C <mountpoint>`
4. 写入 base 元数据文件：`<disk-image>.base-meta.json`。

base 元数据包含：

- `image`：镜像名
- `image_id`：镜像 ID（`docker image inspect` 得到）

当磁盘镜像已存在时，DADK 会比对已有元数据与当前配置：

- 若 base 未变化：按原逻辑复用镜像
- 若 base 变化：打印提示并自动删除重建

## 4. 使用步骤

### 4.1 创建镜像

```bash
dadk rootfs create --skip-if-exists
```

### 4.2 挂载并查看

```bash
dadk rootfs mount
dadk rootfs show-mountpoint
```

### 4.3 卸载

```bash
dadk rootfs umount
```

### 4.4 强制重建（可选）

```bash
dadk rootfs delete
dadk rootfs create
```

## 5. 运行依赖

请确保环境中存在：

- `docker`
- `mkfs.ext4`（通常来自 `e2fsprogs`）
- `mount` / `umount` / `losetup`

## 6. 常见问题

### Q1: `pull_policy = "never"` 且本地无镜像

会报错并停止创建，请先手动拉取镜像或改为 `if-not-present`。

### Q2: 修改了 `base.image` 后镜像没变？

请确认使用的是同一个 manifest/rootfs 配置文件，并检查是否有 `Rootfs base changed, recreate disk image` 日志。

### Q3: 只想继续使用空 rootfs

将 `base.image` 设为空字符串即可：

```toml
[base]
image = ""
pull_policy = "if-not-present"
```
