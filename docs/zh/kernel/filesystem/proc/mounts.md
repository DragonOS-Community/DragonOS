# Proc 挂载导出接口

## 1. 概述

DragonOS 通过 procfs 向用户态导出挂载命名空间视图，主要入口如下：

| 路径 | 类型 | 视角 |
|------|------|------|
| `/proc/mounts` | 符号链接 → `self/mounts` | 当前读取进程 |
| `/proc/self/mounts` | 普通文件 | 当前读取进程 |
| `/proc/[pid]/mounts` | 普通文件 | 目标 `pid` |
| `/proc/[pid]/mountinfo` | 普通文件 | 目标 `pid` |
| `/proc/[pid]/mountstats` | 普通文件 | 目标 `pid` |

其中：

- **`mounts`**：传统格式，字段少，兼容 `mount(8)`、shell 脚本等。
- **`mountinfo`**：现代格式，含 mount id、父子关系、传播标签、superblock 选项等。
- **`mountstats`**：每个 mount 一行描述前缀，并可追加文件系统自定义统计（`proc_show_mount_stats`）。

传播类型（`shared` / `master` / `propagate_from` / `unbindable`）**仅出现在 `mountinfo` 的 optional 字段**，不会写入 `/proc/*/mounts` 的普通 option 列。

## 2. 各接口的功能作用

### 2.1 `/proc/mounts` 与 `/proc/self/mounts`

`/proc/mounts` 在实现上是 **指向 `self/mounts` 的符号链接**（`readlink` 结果为 `self/mounts`），解析后等价于读取 `/proc/self/mounts`，即 **当前读取进程** 在其 mount namespace 与 `fs root` 下的挂载列表。

每一行通常包含：

- 设备名（或文件系统名）
- 挂载点
- 文件系统类型
- 挂载选项（`rw` 及 `nosuid,nodev,...` 等 per-mount 选项；不含传播标签）
- 两个兼容字段 `0 0`

### 2.2 `/proc/[pid]/mounts`

格式与 `/proc/self/mounts` 相同，但 **open 时绑定目标线程组 leader** 的 `mnt_ns` 与 `fs_struct.root()`，导出的是目标进程视角下的可见挂载。

### 2.3 `/proc/[pid]/mountinfo`

在 `mounts` 基础上增加：

- mount id、parent mount id
- 主设备号（`major:minor`）
- mount root（`proc_show_mountinfo_root`）
- per-mount options 与 superblock 选项（以 `-` 分隔的两段 optional 字段）
- propagation tagged fields（`MountPropagation::proc_mountinfo_tags()`）
- 文件系统类型名

### 2.4 `/proc/[pid]/mountstats`

每个可见 mount 至少一行：

```text
device <dev> mounted on <mountpoint> with fstype <type>
```

若底层文件系统通过 `proc_show_mount_stats` 返回额外内容，则追加在同一行末尾。权限为 **0400**（仅 owner 可读），与 `mounts` / `mountinfo` 的 0444 不同。

## 3. 内核源码布局

挂载导出逻辑集中在 **`kernel/src/filesystem/procfs/mount/`**，不再使用历史上的 `mount_view.rs` 单文件或 `procfs/mounts.rs`、`pid/mountinfo.rs` 等分散实现。

```
kernel/src/filesystem/procfs/mount/
├── mod.rs                 # 模块入口；导出 render API
├── collect.rs             # ProcMountEntry、collect_visible_mounts()
├── fields.rs              # MountProcFields：open 前预计算各导出字段
├── escape.rs              # proc 字段转义（空格、制表符、反斜杠等）
├── render.rs              # ProcMountRenderKind；open 渲染 + read 读缓存
├── format/
│   ├── mounts_line.rs     # /proc/*/mounts 行格式
│   ├── mountinfo_line.rs  # /proc/*/mountinfo 行格式
│   └── mountstats_line.rs # /proc/*/mountstats 行格式
└── inode/
    ├── mounts_symlink.rs  # /proc/mounts → self/mounts（MountsSymOps）
    └── pid_mount.rs       # /proc/[pid]/{mounts,mountinfo,mountstats}（MountProcFileOps）
```

**注册位置：**

- `kernel/src/filesystem/procfs/root.rs`：根目录项 `("mounts", MountsSymOps::new_inode)`
- `kernel/src/filesystem/procfs/pid/mod.rs`：`PidDirOps::STATIC_ENTRIES` 中为 `mountinfo` / `mounts` / `mountstats` 各注册一个 `MountProcFileOps`（通过 `ProcMountRenderKind` 区分格式）

**相关但不在 `procfs/mount/` 内的依赖：**

- `kernel/src/filesystem/vfs/mount/mod.rs`：`MountFlags::proc_rw_token()`、`proc_per_mount_options()`、`proc_super_block_options()`、`options_string()`
- `kernel/src/filesystem/vfs/mod.rs`：文件系统 trait 钩子 `proc_show_devname`、`proc_show_mount_options`、`proc_show_mountinfo_root`、`proc_show_mount_stats`
- `kernel/src/process/namespace/propagation.rs`：`MountPropagation::proc_mountinfo_tags()`

用户态测例：`user/apps/tests/dunitest/suites/normal/proc_mount_exports.cc`（whitelist：`normal/proc_mount_exports`）。

## 4. DragonOS 实现原理

### 4.1 统一渲染流水线

三种 proc 文件共用一条流水线（`render.rs`）：

1. **`open()`**（`MountProcFileOps::open` 或经 symlink 打开 `/proc/self/mounts`）
   - 解析目标：`ProcPidTarget` → 线程组 leader `ProcessControlBlock`
2. **`collect_visible_mounts()`**（`collect.rs`）
   - 遍历目标 `mnt_ns.mount_list()`，按 mount id 排序
   - 用目标 `fs_struct.root()` 做 **可见性裁剪**（`visible_mountpoint`）
3. **`MountProcFields::from_entry()`**（`fields.rs`）
   - 为每个 `ProcMountEntry` 快照 devname、fstype、各类 options、mountinfo root/tags 等
4. **按 `ProcMountRenderKind` 调用 `format::*_line::render`**
5. 将完整文本写入 `FilePrivateData::Procfs.data`
6. **`read_at()`** 仅通过 `read_cached_mount_file()` → `proc_read()` 从缓存拷贝

因此当前模型是：**open 时生成整文件快照，同一次打开期间 read 不再重新遍历挂载树**。

### 4.2 目标进程视角

- `/proc/mounts` → `self/mounts` → 当前进程的 pid 目录下的 `mounts`
- `/proc/[pid]/mounts|mountinfo|mountstats` 在 open 时固定绑定该 `pid` 对应线程组的 namespace 与 root

导出内容反映的是 **目标进程的 `mnt_ns` + `fs root`**，不是读取者自己的挂载表（除非读取的就是自己的 `/proc/self/...`）。

### 4.3 可见性裁剪

`collect.rs` 中 `visible_mountpoint(mount_path, root_path)`：

- 目标 root 为 `/` 时，挂载点路径原样导出
- 目标处于 chroot 等受限 root 时，只保留该 root 子树内的 mount，并将显示路径归一化到以 `/` 为根的视图

### 4.4 选项与传播字段的拆分

| 字段来源 | 用于 | 说明 |
|----------|------|------|
| `MountFlags::proc_rw_token()` | mounts / mountinfo per-mount | `ro` 或 `rw` |
| `MountFlags::proc_per_mount_options()` | mountinfo per-mount | `nosuid,nodev,...`，不含 `rw` |
| `MountFlags::proc_super_block_options()` + sb 只读状态 | mountinfo superblock 段 | `sync,mand,...` |
| `FileSystem::proc_show_mount_options()` | mounts 行、mountinfo superblock 段 | 文件系统私有选项 |
| `MountPropagation::proc_mountinfo_tags()` | 仅 mountinfo 尾部 | `shared:N` 等，**不进入 mounts** |

`mounts_line` 使用预合并的 `mounts_options`；`mountinfo_line` 将 per-mount 与 superblock 选项用 `-` 分隔，再追加 propagation tags。

### 4.5 三种格式的职责划分

- **`format/mounts_line.rs`**：设备、挂载点、类型、选项、`0 0`
- **`format/mountinfo_line.rs`**：id、parent、major:minor、root、挂载点、选项段、`-`、fstype、super 选项、tags
- **`format/mountstats_line.rs`**：通用 `device ... mounted on ...` 前缀 + 可选 fs stats

文件系统差异通过 VFS trait 钩子注入，procfs 只负责通用行结构与转义。

## 5. 当前接口的语义特点

### 5.1 `mounts`

兼容性强、字段少；**不包含** propagation 标签。与 Linux 一样，应通过 `/proc/mounts` symlink 访问当前进程视图。

### 5.2 `mountinfo`

恢复挂载拓扑与传播属性的首选接口；per-mount 与 superblock 选项、传播标签分列展示。

### 5.3 `mountstats`

- 不是 mount 变更通知接口
- 同一次 `open()` 内内容为快照；重新 `open()` 可看到更新后的挂载集合与统计
- 行格式允许 `device` 或 `no device` 前缀（由 devname 是否为空决定，测例见 `proc_mount_exports.cc`）

## 6. 与 Linux 的实现差异

### 6.1 总体差异概览

| 维度 | Linux | DragonOS 当前实现 |
|------|-------|-------------------|
| 打开方式 | `seq_file` + 迭代器 | `open()` 时一次性渲染并缓存 |
| 读取方式 | 读时按需生成 | 从 `FilePrivateData` 缓存读取 |
| `/proc/mounts` | symlink → `self/mounts` | 已实现（`MountsSymOps`） |
| 视角绑定 | 目标 task 的 `mnt_ns + fs root` | 同左（`collect_visible_mounts`） |
| `mounts` / `mountinfo` poll | 支持 mount namespace 事件 | 未实现 |
| 遍历基础 | namespace list + cursor | `mnt_ns.mount_list()` 排序后迭代 |
| 代码组织 | `fs/proc_namespace.c` 等 | `procfs/mount/{collect,fields,format,render,inode}` |

### 6.2 Linux 的 `seq_file` 语义

Linux 使用 `mounts_open_common()` + `seq_file` 在读取过程中迭代 mount 列表。DragonOS 选择在 open 时拼完整字符串并缓存，实现更简单，同一次 fd 内结果稳定，但与 Linux 的迭代模型不完全等价。

### 6.3 `mounts` / `mountinfo` 的 `poll`

Linux 可通过 mount namespace 事件对 `mounts` / `mountinfo` 做 `poll`/`epoll`。DragonOS 尚未实现 namespace 事件序号与等待队列，不能作为挂载变更通知源。

### 6.4 `mountstats` 的动态性与 `poll`

Linux 无专门的 `mountstats` poll 语义；DragonOS 同样不为 `mountstats` 发明额外 poll。统计与拓扑变化通过重新打开文件观察。

### 6.5 可见性裁剪语义

Linux 使用 `seq_path_root` 等基于路径对象的 root 裁剪。DragonOS 当前基于 **绝对路径字符串** 与目标 `fs root` 比较，大方向一致，细节上与 Linux 路径对象语义仍有差距。

### 6.6 遍历与权威数据源

Linux 以 namespace 级 mount 链表为权威数据源。DragonOS 从 `MntNamespace::mount_list()` 取表并排序，而非从单棵 mount 树 DFS；后续若要对齐 Linux 迭代顺序与事件模型，需要在 `MntNamespace` 侧继续演进。

## 7. 当前适用场景与建议

已支持：

- 通过 `/proc/mounts`（symlink）或 `/proc/self/mounts` 读取当前进程挂载表
- 调试时读取 `/proc/[pid]/mounts`、`mountinfo`、`mountstats`
- 容器/命名空间工具解析 `mountinfo` 中的 propagation 字段

需注意：

- 依赖 **mount namespace `poll` 通知** 的用户态工具尚未兼容
- 强依赖 Linux `seq_file` 逐行迭代语义的程序可能观察到行为差异
- 修改导出逻辑时，应同时更新 `procfs/mount/` 与 `proc_mount_exports` 测例

## 8. 小结

DragonOS 将 proc 挂载导出收拢到 **`kernel/src/filesystem/procfs/mount/`**：

- **inode 层**：`/proc/mounts` symlink + `/proc/[pid]/*` 统一 `MountProcFileOps`
- **数据层**：`collect` → `fields` 快照 → `format` 三种行渲染
- **选项语义**：传播标签仅在 `mountinfo`；`MountFlags` 与 VFS 钩子分工导出

对外功能定位已接近 Linux；底层仍为 **open 快照 + 字符串裁剪**，在 `poll`、迭代模型与路径语义上继续演进。
