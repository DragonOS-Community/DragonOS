# Proc 挂载导出接口

## 1. 概述

DragonOS 当前已经实现了四个与挂载视图相关的 proc 导出入口，分属三类接口：

- `/proc/mounts`
- `/proc/[pid]/mounts`
- `/proc/[pid]/mountinfo`
- `/proc/[pid]/mountstats`

这些接口的共同目标，是把某个进程视角下的挂载命名空间信息导出给用户态程序。容器运行时、诊断工具、兼容性测试以及日常 shell 工具，都会依赖这些接口来理解当前进程能看到哪些挂载点，以及这些挂载点的类型、选项和统计信息。

其中：

- `mounts` 面向传统工具，格式最简单。
- `mountinfo` 面向现代用户态，信息最完整。
- `mountstats` 面向诊断和统计导出，重点不是拓扑，而是每个 mount 的描述及文件系统自定义统计。

## 2. 各接口的功能作用

### 2.1 `/proc/mounts`

`/proc/mounts` 是“当前读取进程视角”的挂载列表。它通常可以看作 `/proc/self/mounts` 的等价入口，主要用于兼容传统用户态程序。

每一行通常包含：

- 设备名
- 挂载点
- 文件系统类型
- 挂载选项
- 两个兼容字段 `0 0`

它解决的问题是：

- 当前进程能看到哪些挂载点
- 每个挂载点挂载到了哪里
- 每个挂载点的基础挂载参数是什么

### 2.2 `/proc/[pid]/mounts`

`/proc/[pid]/mounts` 与 `/proc/mounts` 的输出格式相同，但视角固定为目标 `pid`，不是读取者自己。

它解决的问题是：

- 调试另一个进程时，查看“目标进程自己的 mount namespace + root 视角”
- 为容器、沙箱、`chroot` 等场景提供正确的挂载视图

### 2.3 `/proc/[pid]/mountinfo`

`mountinfo` 是比 `mounts` 更完整的接口，现代用户态通常更依赖它。

它除了导出基础挂载信息外，还会额外导出：

- mount id
- parent mount id
- 设备号
- mount root
- propagation tagged fields
- filesystem type
- superblock 级选项

它解决的问题是：

- 精确恢复挂载拓扑
- 判断父子 mount 关系
- 判断 shared/slave/unbindable 等传播属性
- 为容器运行时、挂载命名空间工具、系统管理器提供稳定依据

### 2.4 `/proc/[pid]/mountstats`

`mountstats` 的定位不是“另一种 mountinfo”，而是“每个 mount 的统计导出入口”。

它至少为每个 mount 输出一行通用前缀：

```text
device <dev> mounted on <mountpoint> with fstype <type>
```

如果底层文件系统实现了额外的统计导出钩子，则还会在同一条记录后追加文件系统自定义统计内容。

它解决的问题是：

- 让用户态按 mount 粒度读取统计信息
- 为 NFS/CIFS 一类文件系统提供统一的挂载统计导出入口
- 为诊断工具提供每个挂载点的基础描述信息

## 3. DragonOS 当前实现原理

### 3.1 统一渲染入口

DragonOS 当前没有为 `mounts`、`mountinfo`、`mountstats` 分别实现三套独立逻辑，而是统一复用了 `kernel/src/filesystem/procfs/mount_view.rs` 中的挂载视图渲染逻辑。

整体流程如下：

1. `open()` 时根据当前文件类型选择 `Mounts`、`MountInfo` 或 `MountStats`
2. 按目标 `pid` 找到对应进程
3. 从目标进程的 `mnt_ns` 和 `fs root` 收集“该进程可见”的 mount
4. 将结果渲染成完整字节串
5. 把这份字节串缓存在当前已打开文件的 `FilePrivateData` 中
6. 后续 `read_at()` 仅从缓存中拷贝数据

这意味着 DragonOS 当前的 proc 挂载文件是“open 时生成一次快照，read 时读取缓存”的模型。

### 3.2 目标进程视角

DragonOS 当前实现已经区分了“当前进程视角”和“目标 pid 视角”：

- `/proc/mounts` 使用当前进程 pid
- `/proc/[pid]/mounts`
- `/proc/[pid]/mountinfo`
- `/proc/[pid]/mountstats`

后三者都按目标 `pid` 查找对应的 `ProcessControlBlock`，然后从目标进程的：

- `mnt_ns`
- `fs_struct.root()`

导出挂载视图。

因此，这些接口已经具备“按目标进程视角导出”的基本语义，而不是简单地把读取者自己的挂载表重复导出一遍。

### 3.3 可见性裁剪

当前实现会根据目标进程的 `fs root` 对挂载点做可见性裁剪。直观上说，就是：

- 如果目标进程的 root 是 `/`，则导出完整路径
- 如果目标进程处于 `chroot` 或类似受限根目录中，则仅导出该 root 之下可见的挂载点
- 对目标进程来说可见的根路径，会在导出时显示为 `/`

这使得导出结果更接近“目标进程真正能看到的世界”。

### 3.4 三种格式的渲染

统一收集到可见 mount 后，DragonOS 再按不同接口生成不同文本格式：

- `mounts`：渲染设备名、挂载点、文件系统类型、选项
- `mountinfo`：额外渲染 mount id、parent id、设备号、root、propagation tags
- `mountstats`：渲染通用前缀，并调用文件系统钩子输出 fs-specific stats

当前文件系统扩展点已经具备以下 proc 导出钩子：

- `proc_show_devname`
- `proc_show_mount_options`
- `proc_show_mountinfo_root`
- `proc_show_mount_stats`

这意味着 procfs 负责通用格式，具体文件系统负责“如何描述自己”，整体分层已经接近 Linux 的设计方向。

## 4. 当前接口的语义特点

### 4.1 `mounts`

`mounts` 的特点是兼容性强、字段少、解析简单，但信息不完整。它适合传统工具和人工查看，不适合恢复完整挂载拓扑。

### 4.2 `mountinfo`

`mountinfo` 是当前最重要的挂载导出接口。若用户态需要：

- 识别真实父子关系
- 识别 propagation 类型
- 理解 mount root 与 mountpoint 的区别

应优先使用 `mountinfo`，而不是 `mounts`。

### 4.3 `mountstats`

`mountstats` 的关键点有两个：

1. 它不是 mount 变更通知接口。
2. 它的值可能随着重新打开文件而变化。

当前 DragonOS 实现中，`mountstats` 与另外两个接口一样，都是在 `open()` 时生成一份缓存。因此：

- 同一次打开期间，内容是稳定的
- 重新打开后，会看到新的快照

这与 Linux 中“读时动态遍历并导出”的机制并不完全相同，但对大多数一次性读取型工具而言已经足够工作。

## 5. 与 Linux 的实现差异

### 5.1 总体差异概览

| 维度 | Linux | DragonOS 当前实现 |
|------|-------|-------------------|
| 打开方式 | `seq_file` + 迭代器 | `open()` 时一次性渲染并缓存 |
| 读取方式 | 读时按需生成 | 从缓存字节串读取 |
| 视角绑定 | 目标 task 的 `mnt_ns + fs root` | 已按目标进程的 `mnt_ns + fs root` 导出 |
| `mounts` / `mountinfo` poll | 支持 mount namespace 事件通知 | 当前未实现 Linux 风格的 poll 事件 |
| `mountstats` poll | 不提供专门的 mount 变更通知语义 | 当前也不应额外发明该语义 |
| 遍历基础 | namespace list + cursor | 基于当前挂载结构递归收集可见 mount |

### 5.2 Linux 的 `seq_file` 语义

Linux 的 `/proc/<pid>/{mounts,mountinfo,mountstats}` 使用统一的 `mounts_open_common()` 绑定上下文，并复用 `mounts_op` 迭代 mount namespace。

这套模型的特点是：

- 每次读取时由 `seq_file` 框架驱动迭代
- 遍历状态由 cursor 维护
- 挂载文件不是“提前拼好的一整段字符串”

DragonOS 当前则选择了更简单直接的实现：

- 在 `open()` 时渲染完整内容
- 使用 `FilePrivateData` 缓存结果
- `read_at()` 不再重新计算

这种做法的优点是实现简单、一次打开内结果稳定；缺点是与 Linux 的底层机制仍有差异。

### 5.3 `mounts` / `mountinfo` 的 `poll`

Linux 中：

- `mounts`
- `mountinfo`

都支持基于 mount namespace 事件的 `poll`。当 mount namespace 发生变化时，用户态可以通过 `poll`/`epoll` 感知“挂载视图已经变化”。

DragonOS 当前还没有实现这一机制。当前缺少的基础设施主要包括：

- namespace 级事件序号
- namespace 级等待队列
- 与 mount/umount/remount/propagation 变化联动的事件更新

因此，DragonOS 当前还不能像 Linux 那样把 `mounts` / `mountinfo` 作为 mount namespace 变更通知口来使用。

### 5.4 `mountstats` 的动态性与 `poll`

Linux 中 `mountstats` 的内容是动态生成的，并且文件系统统计可能随时间和 I/O 改变，例如 NFS 可以导出动态增长的 I/O 统计。

但是 Linux 并没有为 `mountstats` 定义专门的 `.poll` 事件语义。原因在于：

- `mountstats` 不仅会因 mount 拓扑变化而变化
- 还可能因为文件系统内部统计变化而变化
- 这些变化没有统一、低成本、通用的事件模型

DragonOS 当前也没有必要为 `mountstats` 实现额外的“更新通知机制”。比较合理的目标是：

- 重新打开时看到最新快照
- 不为 `mountstats` 发明 Linux 没有定义的 poll 语义

### 5.5 可见性裁剪语义

Linux 在导出挂载点路径时，会以目标 task 的 root 进行 path-root 语义判断。该机制本质上依赖内核路径对象和 chroot 视角，而不是简单字符串裁剪。

DragonOS 当前实现虽然也按目标进程 root 做了可见性过滤，但实现方式仍更接近“基于绝对路径字符串的裁剪”。这在大方向上满足需求，但与 Linux 的严格路径对象语义还存在差距。

### 5.6 遍历与权威数据源

Linux 有明确的 namespace 级 mount list，并通过 list + cursor 维持稳定迭代。

DragonOS 当前实现更偏向于：

- 从目标 namespace 的 root mount 出发
- 递归访问当前挂载树
- 再将结果渲染为三种 proc 文本格式

这种实现可以工作，但它仍不是 Linux 那种“以 namespace 权威记录表为中心”的导出模型。后续若要进一步对齐 Linux，比较自然的方向是为 `MntNamespace` 引入更明确的权威 mount record、稳定遍历序和事件序号。

## 6. 当前适用场景与建议

当前 DragonOS 的这套实现，已经足以支持以下场景：

- 传统工具读取 `/proc/mounts`
- 用户态读取目标进程的 `/proc/[pid]/mounts`
- 容器与命名空间工具读取 `/proc/[pid]/mountinfo`
- 诊断工具读取 `/proc/[pid]/mountstats`

但在以下方面仍需注意：

- 若用户态强依赖 Linux 风格的 `poll` 挂载变更通知，DragonOS 当前还不具备完整兼容性
- 若用户态依赖 Linux `seq_file` 的精细迭代行为，当前实现也不完全等价
- 若未来引入更复杂的网络文件系统统计，`mountstats` 可能需要从“open 时快照”进一步演进为更接近 Linux 的“按需生成”

## 7. 小结

DragonOS 当前已经建立了统一的 proc 挂载导出链路，并提供了：

- 面向兼容的 `mounts`
- 面向完整拓扑的 `mountinfo`
- 面向统计导出的 `mountstats`

从功能定位上看，这三者已经与 Linux 基本对齐；从底层机制上看，DragonOS 当前采用的是“open 时快照缓存”的简化实现，而不是 Linux 的 `seq_file + namespace event` 模型。

因此，理解这三类接口时，应区分两件事：

- **对外功能定位**：当前已经基本具备
- **底层机制与兼容性细节**：仍与 Linux 存在差异，尤其体现在 `poll`、迭代模型、可见性裁剪语义和 namespace 事件机制上
