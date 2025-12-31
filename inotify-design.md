# DragonOS inotify 机制设计方案（对齐 Linux 6.6 语义）

> 目标：在 DragonOS 内核中实现与 Linux 6.6 行为一致的 inotify（及其底层 fsnotify 风格机制），并遵循 Rust 的所有权/并发范式，实现高内聚、低耦合、可扩展。
>
> Linux代码在： ~/code/linux-6.6.21/

## 1. 设计约束与目标

### 1.1 约束

- **Linux 兼容语义**：系统调用 ABI、阻塞/非阻塞、poll/epoll 可用性、事件格式必须尽量对齐 Linux 6.6。
- **高内聚、低耦合**：
  - inotify 逻辑不侵入具体文件系统（ext4/fat/procfs/…）。
  - VFS 层仅暴露“事件上报 API”，不依赖 inotify 内部数据结构。
- **Rust 编程范式**：
  - 明确所有权/生命周期，避免引用环。
  - 采用 `Arc`/`Weak` + 自旋锁/互斥锁的组合，确保可在中断上下文/可睡眠上下文正确使用。
- **渐进实现**：先实现 inotify（用户态常用），再扩展 fanotify/dnotify（可选）。

### 1.2 目标（MVP 必达）

- syscalls：`inotify_init` / `inotify_init1` / `inotify_add_watch` / `inotify_rm_watch`
- inotify fd 行为：`read`、`poll/epoll` 可用、`FIONREAD`（查询可读字节数）
- 关键事件：
  - 文件：`IN_OPEN`、`IN_CLOSE_WRITE`、`IN_CLOSE_NOWRITE`、`IN_MODIFY`、`IN_ATTRIB`、`IN_DELETE_SELF`、`IN_MOVE_SELF`
  - 目录及子项：`IN_CREATE`、`IN_DELETE`、`IN_MOVED_FROM`、`IN_MOVED_TO`
  - 特殊：`IN_IGNORED`、`IN_Q_OVERFLOW`、`IN_UNMOUNT`、`IN_ISDIR`
- 目录 watch 的 **event-on-child** 行为：对目录 inode 的 watch 自动接收子项事件（与 Linux 的 `FS_EVENT_ON_CHILD` 对齐）。

### 1.3 非目标（可以后续迭代）

- `INOTIFY_IOC_SETNEXTWD`（checkpoint/restore）
- 完整的 per-user 资源统计（max_user_watches / max_user_instances / memcg accounting）
- 与 LSM（security hooks）完全等价的安全检查（DragonOS 可逐步完善）

## 2. Linux 6.6.21 参考实现要点（语义基线）

Linux 源码位置（本机）：`~/code/linux-6.6.21/`

### 2.1 UAPI：事件结构与标志位

- `include/uapi/linux/inotify.h`
  - `struct inotify_event { wd, mask, cookie, len, name[] }`
  - `IN_CLOEXEC == O_CLOEXEC`、`IN_NONBLOCK == O_NONBLOCK`

关键语义：
- `len` 是 **name 字段长度（含 NUL 与 padding）**，Linux 会对齐到 `sizeof(struct inotify_event)` 的倍数。
- `name` 对于目录 watch 的子项事件为相对文件名；对纯 inode 事件可能为空。

### 2.2 inotify fd 的 file_operations（read/poll/ioctl）

- `fs/notify/inotify/inotify_user.c`
  - `inotify_poll()`：等待 `group->notification_waitq`，队列非空返回 `EPOLLIN|EPOLLRDNORM`
  - `inotify_read()`：
    - 队列为空：非阻塞返回 `-EAGAIN`；阻塞睡眠；若有信号返回 `-ERESTARTSYS`
    - **缓冲区不足以容纳“下一个事件”**：返回 `-EINVAL`（注意不是短读）
    - 若已拷贝出部分事件，再遇到错误（非 `EFAULT`）会返回已读字节数
  - `FIONREAD`：遍历队列，计算可读字节数（事件头 + name padding）

### 2.3 watch 的生命周期与 wd 分配

- Linux 将“watch”实现为 fsnotify mark：`inotify_inode_mark`（挂到 inode 上）。
- wd 通过 `idr_alloc_cyclic(..., start=1)` 分配，0 不使用。
- `inotify_add_watch`：
  - mask 必须仅包含 `ALL_INOTIFY_BITS`，且必须至少一个 bit
  - `IN_MASK_ADD` 与 `IN_MASK_CREATE` 互斥
  - 解析路径：默认跟随 symlink（除非 `IN_DONT_FOLLOW`），`IN_ONLYDIR` 要求目标为目录
  - 已存在 watch：根据 `IN_MASK_ADD` 选择“替换/叠加”mask；`IN_MASK_CREATE` 要求不存在否则 `-EEXIST`
- `inotify_rm_watch`：wd 不存在返回 `-EINVAL`；存在则销毁 mark，并最终触发 `IN_IGNORED` 事件。

### 2.4 队列溢出与 group 销毁语义

- `fs/notify/group.c`
  - `fsnotify_group_stop_queueing()`：设置 `group->shutdown=true`，保证之后不再入队
  - `fsnotify_destroy_group()`：停止入队 → 清 mark → 等待引用释放 → flush 通知队列 → 释放 overflow_event → put group

**结论**：Linux 的关键抽象是“group + mark + event queue”，inotify 只是 fsnotify 的一个 backend。

## 3. DragonOS 现状盘点（可复用基础设施）

### 3.1 现有基础设施

- `WaitQueue`：`kernel/src/libs/wait_queue.rs`
  - 提供可中断等待、避免丢失唤醒的 waiter/waker 机制。
- `PollableInode` + `EPollItem`：
  - `kernel/src/filesystem/vfs/mod.rs` 定义 `PollableInode`
  - `eventfd`（`kernel/src/filesystem/eventfd.rs`）给出了“伪文件 inode + WaitQueue + epoll items + 非阻塞语义”的成熟模式
- `ioctl` 分发：`kernel/src/filesystem/vfs/syscall/sys_ioctl.rs`
  - 未识别的 `cmd` 会下发到 inode 的 `IndexNode::ioctl()`

### 3.2 inotify 的系统调用号

- DragonOS 已预留 syscall nr（不同架构号不同），但目前未找到具体实现函数。

**启示**：DragonOS 的 eventfd/epoll 代码路径，几乎可以直接作为 inotify fd 的实现模板。

## 4. 总体架构：fsnotify-core + inotify-backend

为避免把 inotify 逻辑“揉进 VFS/各 FS 实现”，建议新增一个内核子系统：

- `kernel/src/filesystem/notify/`（建议新模块）
  - `fsnotify_core`：通用的 watch 注册、inode 事件分发、cookie 分配、生命周期管理
  - `inotify`：面向用户态的 backend（fd、队列、UAPI 编码、syscalls）

### 4.1 模块边界（低耦合）

1) **VFS 层（事件生产者）**
- 只调用一个窄接口：`notify::report(event)`，不持有任何 inotify 内部锁。

2) **notify 子系统（事件路由器）**
- 维护 `(dev_id, inode_id) -> watchers` 的 registry
- 根据事件类型、watch mask、是否目录、是否 child event，决定投递到哪些 inotify 实例

3) **inotify backend（事件消费者队列）**
- 每个 inotify 实例是一种“可 poll/read 的伪文件 inode”，类似 eventfd。
- 负责：队列、溢出、read/poll/ioctl、wd 管理。

### 4.2 为何采用“全局 registry”而不是修改每个 inode

- DragonOS 的 `IndexNode` 是 trait object；各 FS inode 类型众多，给每个 inode 增加“watch 链表字段”会造成巨大侵入与耦合。
- 以 `(dev_id, inode_id)` 为 key 的 registry：
  - 能在不修改具体 inode 的前提下维护 watch 列表
  - 与 DragonOS `Metadata { dev_id, inode_id }` 自然匹配

## 5. 核心数据结构设计（Rust 范式）

### 5.1 基础标识

```text
InodeKey := (dev_id: usize, inode_id: InodeId)
```

### 5.2 inotify 实例（对应 Linux fsnotify_group）

- `InotifyInstance`（Arc 管理生命周期）
  - `queue: SpinLock<VecDeque<InotifyQueuedEvent>>`
  - `waitq: WaitQueue`
  - `epitems: LockedEPItemLinkedList`（复用 eventfd 模式）
  - `flags: FileFlags`（至少 `O_NONBLOCK`）
  - `max_events: usize`
  - `overflow_queued: AtomicBool`
  - `shutdown: AtomicBool`（对应 Linux `group->shutdown`）
  - `wd_alloc: IdAllocator`（从 1 开始）
  - `wd_map: SpinLock<BTreeMap<i32, Watch>>`（wd->Watch）

### 5.3 Watch / Mark

- `Watch`
  - `wd: i32`
  - `key: InodeKey`
  - `mask: u32`（inotify mask，含 `IN_ALL_EVENTS` 与特殊 flag）
  - `flags: WatchFlags`（内部扩展：excl_unlink、oneshot、is_dir_hint…）

- `RegistryEntry`（按 inode 聚合）
  - `watchers: Vec<Weak<InotifyInstance>> + wd/mask 信息`
  - 或保存为 `Vec<WatcherRef>`：其中 `WatcherRef { instance: Weak<InotifyInstance>, wd: i32, mask: u32, flags... }`

注意：registry 使用 `Weak` 指向 instance，避免“inode->watch->instance->inode”引用环。

### 5.4 Event（队列项）

- `InotifyQueuedEvent`
  - `wd: i32`（溢出事件为 -1）
  - `mask: u32`（最终给用户的 inotify mask）
  - `cookie: u32`（rename/move 配对）
  - `name: Option<Vec<u8>>`（UTF-8 不强制；按字节处理，末尾补 0）
  - `isdir: bool`（最终 OR `IN_ISDIR`）

事件编码时严格按 Linux UAPI：
- 头部 `struct inotify_event` 固定 16 字节（Linux uapi 定义），
- `len` 为 name padding 后长度（包含 NUL）。

## 6. 事件分发模型（对齐 Linux）

### 6.1 统一内部事件类型

定义内部事件枚举（示意）：

- `FsEvent::Inode { key, kind, is_dir }`
- `FsEvent::DirChild { parent_key, name, kind, child_is_dir, cookie? }`
- `FsEvent::Unmount { key }`

其中 `kind` 可映射到 inotify bits：

| VFS 操作/场景 | 目标 watch | 对应 inotify bits | name | cookie |
|---|---|---|---|---|
| `open` | inode | `IN_OPEN` | 无 | 0 |
| `close`（写打开） | inode | `IN_CLOSE_WRITE` | 无 | 0 |
| `close`（非写） | inode | `IN_CLOSE_NOWRITE` | 无 | 0 |
| `write`/`truncate`/`resize` | inode | `IN_MODIFY` | 无 | 0 |
| `chmod/chown/utimens/xattr` | inode | `IN_ATTRIB` | 无 | 0 |
| `mkdir` | 父目录 watch | `IN_CREATE` + `IN_ISDIR` | 子名 | 0 |
| `create`/`mknod`/`symlink` | 父目录 watch | `IN_CREATE` | 子名 | 0 |
| `unlink` | 父目录 watch | `IN_DELETE` | 子名 | 0 |
| `rmdir` | 父目录 watch | `IN_DELETE` + `IN_ISDIR` | 子名 | 0 |
| `unlink` 导致目标 inode 消亡 | 目标 inode watch | `IN_DELETE_SELF` | 无 | 0 |
| `rename`/`move_to` | old 父目录 watch | `IN_MOVED_FROM` (+ `IN_ISDIR`?) | 旧名 | cookie |
| `rename`/`move_to` | new 父目录 watch | `IN_MOVED_TO` (+ `IN_ISDIR`?) | 新名 | cookie |
| `rename`/`move_to`（目标自身） | 目标 inode watch | `IN_MOVE_SELF` | 无 | cookie(可 0 或同 cookie) |
| watch 被移除/对象不可达 | 该 watch | `IN_IGNORED` | 无 | 0 |
| 队列溢出 | instance | `IN_Q_OVERFLOW` | 无 | 0 |
| unmount | inode/dir watch | `IN_UNMOUNT` | 可选 | 0 |

> 备注：Linux 对 `IN_MOVE_SELF` 的 cookie 行为在不同场景下不一定暴露给用户；实现上允许 cookie=0。

### 6.2 cookie 生成

- 维护 `static MOVE_COOKIE: AtomicU32`，从 1 开始自增。
- 对 `rename/move_to`：统一生成一个 cookie 用于 `MOVED_FROM` 与 `MOVED_TO` 配对。

### 6.3 目录子项事件（event-on-child）

Linux 目录 watch 默认包含 `FS_EVENT_ON_CHILD` 行为。

实现策略：
- 在 `inotify_add_watch` 时，如果目标 inode 是目录，则 internal mask 自动带上 `EVENT_ON_CHILD` 语义。
- VFS 在做 `mkdir/unlink/rename/...` 这种“父目录发生变化”的操作时，必须调用 `report_dir_child(parent_key, name, ...)`。

## 7. inotify fd 行为设计（read/poll/ioctl）

### 7.1 read(2) 语义（严格对齐 Linux）

- 取队首事件，先计算它编码后的总大小 `event_size`。
- 若 `event_size > user_count`：
  - 若当前 read 尚未拷贝任何事件：返回 `EINVAL`（Linux 行为）
  - 否则返回已拷贝字节数（除 `EFAULT` 例外）
- 队列为空：
  - `O_NONBLOCK`：返回 `EAGAIN`
  - 否则 `WaitQueue::wait_event_interruptible`，被信号打断返回 `ERESTARTSYS`

### 7.2 poll/epoll

- 复用 `PollableInode`：
  - 队列非空 → `EPOLLIN|EPOLLRDNORM`
  - 队列空 → 0
- 当新事件入队：
  - `waitq.wake_one()/wake_all()`（按需）
  - `EventPoll::wakeup_epoll(&epitems, EPOLLIN|EPOLLRDNORM)`

### 7.3 ioctl: FIONREAD

- inotify inode 实现 `IndexNode::ioctl(cmd, data, ...)`：
  - `FIONREAD`（Linux: 0x541B）：计算队列内所有事件编码后总字节数，写回用户 `int`。

### 7.4 队列溢出策略（IN_Q_OVERFLOW）

- `max_events`：默认 16384（对齐 Linux 默认）
- 入队流程：
  1) 若 `shutdown==true`：丢弃（对齐 `fsnotify_group_stop_queueing`）
  2) 若 `queue.len() >= max_events`：
     - 若 `overflow_queued==false`：推入一个 overflow event（wd=-1, mask=IN_Q_OVERFLOW）并置位
     - 丢弃当前事件
  3) 否则正常入队

- 当用户读走 overflow 事件后：
  - 清 `overflow_queued=false`（允许未来再次投递 overflow）

## 8. syscalls 设计（DragonOS 实现落点）

建议新增 syscall 实现文件：

- `kernel/src/filesystem/inotify/`（或 `kernel/src/filesystem/notify/inotify/`）
  - `sys_inotify_init.rs`
  - `sys_inotify_add_watch.rs`
  - `sys_inotify_rm_watch.rs`

并通过 syscall table 注册（仿照 `sys_write`, `sys_ioctl` 等）。

### 8.1 inotify_init1(flags)

- 校验 `flags & ~(IN_CLOEXEC|IN_NONBLOCK) == 0`，否则 `EINVAL`
- 创建 `InotifyInode`（伪文件 inode）并 `File::new(inode, O_RDONLY|flags)`
- 分配 fd（复用 eventfd 的 fd 分配模式）

### 8.2 inotify_add_watch(fd, pathname, mask)

对齐 Linux 的关键检查：
- `mask` 必须：
  - 仅包含合法位（`IN_ALL_EVENTS` + 若干 flag 位）
  - 且至少有一个事件位（不能全是 flag）
- `IN_MASK_ADD` 与 `IN_MASK_CREATE` 互斥，否则 `EINVAL`
- 校验 `fd` 指向的 file 是 inotify inode（否则 `EINVAL`）
- 路径解析：
  - 默认跟随符号链接；若 `IN_DONT_FOLLOW` 则不跟随最后一跳
  - 若 `IN_ONLYDIR` 则目标必须是目录，否则 `ENOTDIR`（或 `EINVAL`，建议按 Linux: `ENOTDIR`）
- 权限检查：对目标 inode 需要 `MAY_READ`（Linux 要求可读）
- 若 inode 上已有 watch：
  - `IN_MASK_CREATE` → `EEXIST`
  - 否则按 `IN_MASK_ADD` 决定：
    - replace：替换 mask
    - add：按位 OR
- 返回 wd（>=1）

### 8.3 inotify_rm_watch(fd, wd)

- 校验 `fd` 类型
- `wd` 不存在：返回 `EINVAL`
- 存在：
  - 从 instance 的 `wd_map` 移除
  - 从 registry 中移除该 watcher
  - 向该 instance 队列投递 `IN_IGNORED`（wd=原 wd）

## 9. VFS 事件上报点（最小侵入改造方案）

### 9.1 设计原则

- **只在 VFS 公共路径上报**，避免在每个文件系统里重复实现。
- 优先选择已经集中化的函数：
  - `kernel/src/filesystem/vfs/vcore.rs`（`do_mkdir_at`, `do_unlink_at`, `do_remove_dir`, `vfs_truncate` …）
  - `kernel/src/filesystem/vfs/syscall/rename_utils.rs`（`do_renameat2`）
  - `kernel/src/filesystem/vfs/file.rs` / `IndexNode::{read_at, write_at, open, close, set_metadata, resize}`（若需要更细粒度）

### 9.2 推荐的最小落点（MVP）

- 目录相关：
  - 在 `do_mkdir_at` 成功创建后：对父目录 report `IN_CREATE(+IN_ISDIR)`
  - 在 `do_unlink_at` 成功后：对父目录 report `IN_DELETE`；对目标 inode report `IN_DELETE_SELF`（若 link 计数归零/对象不可达时）
  - 在 `do_remove_dir` 成功后：对父目录 report `IN_DELETE(+IN_ISDIR)`；对目标 inode report `IN_DELETE_SELF`（目录自身被删）
  - 在 `do_renameat2` 成功后：对 old/new 父目录分别 report `IN_MOVED_FROM/IN_MOVED_TO`，同 cookie；对目标 inode report `IN_MOVE_SELF`

- 文件内容与属性：
  - `vfs_truncate` 成功且 size 变化：report `IN_MODIFY`
  - `set_metadata`（chmod/chown/utimens/xattr 等）成功：report `IN_ATTRIB`
  - `write` 成功写入非 0 字节：report `IN_MODIFY`

- open/close：
  - 需要在 `File::new/open` 或 `IndexNode::open/close` 的公共路径上报 `IN_OPEN`、`IN_CLOSE_WRITE/NOWRITE`

> 说明：DragonOS 当前 open/close 分散在 inode 实现中；为了低耦合，建议在 `File` 层或 VFS open/close wrapper 统一上报。

## 10. 并发与锁顺序（避免死锁）

### 10.1 锁分层建议

1) VFS/FS 内部锁（文件系统自己的锁）
2) notify registry 锁（短临界区：查找 watchers 并 clone 目标 instance 引用）
3) instance 队列锁（短临界区：push event）
4) 唤醒（WaitQueue / epoll wakeup）

关键规则：
- **上报路径不得持有 instance 队列锁再进入 VFS**。
- registry 查找与队列 push 分开：
  - registry 内只做“取 watchers 列表（或弱引用）并返回”
  - 之后逐个尝试升级 `Weak->Arc` 并 push event

### 10.2 Watch 的 oneshot / excl_unlink

- `IN_ONESHOT`：投递一次后自动移除该 watch 并投递 `IN_IGNORED`。
- `IN_EXCL_UNLINK`：当目标 inode `nlinks==0`（或被标记为“unlinked”）时不再投递后续事件。

这两者都应在 notify 子系统内部处理，避免在 VFS/FS 层散落判断。

## 11. 资源限制与可配置项（建议）

对齐 Linux：
- `max_queued_events`（默认 16384）
- `max_user_instances`（默认 128）
- `max_user_watches`（按内存比例估算）

DragonOS 可先实现 `max_queued_events`（影响溢出），其余后续补齐。

## 12. 测试策略（对齐 gVisor/syscalls）

- 优先使用 DragonOS 约定：
  - 自编单测：`user/apps/c_unitest`
  - gVisor syscalls 测试：如需对照，可参考 DragonOS 社区 gVisor 分支

建议用例：
- init1 flags 校验：非法 flags → `EINVAL`
- add_watch：
  - mask=0 → `EINVAL`
  - `IN_MASK_ADD|IN_MASK_CREATE` 同时设置 → `EINVAL`
  - non-inotify fd → `EINVAL`
  - `IN_ONLYDIR` watch 普通文件 → `ENOTDIR`
- read：
  - 非阻塞且空 → `EAGAIN`
  - 缓冲区不足容纳队首事件 → `EINVAL`
  - 多事件读取，遇到信号/错误时返回已读字节
- rename cookie 配对：`IN_MOVED_FROM` 与 `IN_MOVED_TO` cookie 相同
- overflow：构造大量事件 → 队列溢出，读到 `IN_Q_OVERFLOW`

## 13. 迭代路线（建议）

- M1（最小可用）：
  - inotify fd + read/poll/FIONREAD
  - mkdir/unlink/rmdir/rename/write/truncate/attr/open/close 的事件上报
- M2（增强兼容）：
  - 更完整的 unmount 行为、excl_unlink/oneshot
  - 更细的 close 语义（write-open vs read-open）
- M3（生态）：
  - /proc/sys/fs/inotify/* 配置、per-user 限制
  - fanotify/dnotify 复用同一 fsnotify_core

---

## 附：与 DragonOS 代码结构的对齐建议（文件路径）

- 新增目录（建议）：
  - `kernel/src/filesystem/notify/mod.rs`
  - `kernel/src/filesystem/notify/fsnotify_core.rs`
  - `kernel/src/filesystem/notify/inotify/`（instance/inode/syscalls）
- VFS 上报点（建议优先改动）：
  - `kernel/src/filesystem/vfs/vcore.rs`（mkdir/unlink/rmdir/truncate）
  - `kernel/src/filesystem/vfs/syscall/rename_utils.rs`（rename）
  - `kernel/src/filesystem/vfs/file.rs` 或 open/close wrapper（open/close/write）

如果你希望我下一步直接把上述方案落到代码实现（含 syscalls + 伪文件 inode + VFS 上报点 + 基础测试），我可以继续在此分支上按 M1 目标实现。