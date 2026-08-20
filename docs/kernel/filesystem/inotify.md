# inotify 文件系统事件通知 — 设计与实施计划

> 对应 issue: [DragonOS-Community/DragonOS#2151](https://github.com/DragonOS-Community/DragonOS/issues/2151)
> 参考实现：Linux 6.6 `fs/notify/`、`fs/anon_inodes.c`、`include/uapi/linux/inotify.h`
> 状态：**设计评审中**

本文档是实施前的架构设计。目标：在 DragonOS 内核实现完整的 inotify（`inotify_init1` / `inotify_add_watch` / `inotify_rm_watch` / `read`），覆盖标准事件集，支持 epoll。

---

## 0. 设计原则（不可妥协）

1. **Linux 语义对齐**：行为参考 Linux 6.6。`inotify_event` 字节布局、事件 mask、错误码、read 语义必须与 glibc/strace 期望一致。
2. **低冗余**：VFS 写路径 hook 只在每个操作的**唯一入口**插一处，不逐个文件系统实现去改。
3. **不破坏现有功能**：所有 hook 必须在「操作成功之后」触发，且 hook 本身不能阻塞/失败导致原操作回退。`fsnotify()` 是「尽力而为」投递，绝不影响 syscall 的返回值。
4. **不引入 workaround**：mark 生命周期用强引用 pinning 语义解决，而非「删了再假装没删」。
5. **不过度设计**：fsnotify 层只保留 inotify 当前需要的最小抽象；不预先实现 connector/SRCU/superblock-mark/mount-mark（Linux 有，DragonOS 暂不需要）。

---

## 1. 总体架构

三层，自底向上：

```
┌─────────────────────────────────────────────────────────────┐
│  syscall 层 (inotify_init1/add_watch/rm_watch)               │
│    → 创建 InotifyInstance → 注册为伪文件 fd                   │
└─────────────────────────────────────────────────────────────┘
          │ read(fd) / poll(fd) / epoll(fd)
          ▼
┌─────────────────────────────────────────────────────────────┐
│  inotify 设备 (filesystem/inotify.rs)                        │
│    InotifyInode: impl IndexNode + PollableInode              │
│      - 事件队列 (VecDeque<InotifyEventInfo>)                  │
│      - WaitQueue + LockedEPItemLinkedList (epoll 集成)        │
│    InotifyBackend: impl FsNotifyBackend (事件格式化/入队/合并) │
└─────────────────────────────────────────────────────────────┘
          ▲ handle_event(mark, mask, name, cookie, is_dir)
          │
┌─────────────────────────────────────────────────────────────┐
│  fsnotify 统一通知层 (filesystem/fsnotify/)                   │
│    fsnotify(): VFS hook 调用的统一入口                         │
│      → 用 inode_id 在全局 mark 索引中找匹配的 mark            │
│      → 对每个 mark 调 backend.handle_event()                  │
│    FsNotifyGroup: 一个通知消费者（一个 inotify fd 对应一个）   │
│    FsNotifyMark: 一个 watch (group + inode + mask + wd)      │
└─────────────────────────────────────────────────────────────┘
          ▲ fsnotify() 调用点
          │
┌─────────────────────────────────────────────────────────────┐
│  VFS 写路径 hook 点（syscall-core + File 层，非各 FS 实现）   │
│    vcore.rs / rename_utils.rs / symlink_utils.rs / link_     │
│    utils.rs / open.rs  +  File::do_read/do_write/Drop        │
└─────────────────────────────────────────────────────────────┘
```

### 为什么 fsnotify 与 inotify 分两层

这是 Linux 的天然接缝，**不是为了未来 fanotify 而做的投机抽象**：
- VFS hook 只知道「某 inode 上发生了某事件」，不知道事件如何被消费。
- inotify 负责「把事件格式化成 `inotify_event`、按 wd 关联、合并、入队」。

即使永不做 fanotify，这个边界也让 VFS hook 保持轻薄、与消费侧解耦。`FsNotifyBackend` trait 仅 3 个方法（见 §3），不是过度抽象。

---

## 2. VFS hook 点（核心改动面）

### 2.1 决策：hook 在 syscall-core 层，不在 MountFSInode，不在各 FS 实现

**理由（这是最关键的架构决策）：**

1. **已具备 child inode 上下文**。经核实，vcore/rename_utils 在执行变异**之前**已经 `find()`/`lookup()` 解析出目标 inode：
   - `do_unlink_at` `vcore.rs:705` `let target_inode = parent_inode.find(filename)?;`
   - `do_remove_dir` `vcore.rs:665` `let target_inode = parent_inode.find(filename)?;`
   - `do_renameat2` `rename_utils.rs:87` `let old_inode = old_parent_inode.lookup(old_filename)?;`
   - `do_sys_open` `open.rs:347-358` create 成功后持有 `inode` 与 `parent_inode`。
   
   这意味着 `IN_DELETE_SELF`/`IN_MOVE_SELF` 所需的「被监听 inode 自身」已经握在手里，无需再 lookup（避免 TOCTOU 竞态）。

2. **最低 regression 风险**。MountFSInode (`mount/mod.rs:3585+`) 使用复杂的 `DentryMutationContext`、双层锁（children_gate / dentry_namespace_lock）。在那里插桩要求 hook 在这些锁下安全；而 syscall-core 层的锁上下文更简单、更可控。

3. **唯一入口，低冗余**。所有用户态触发的 namespace 变更都经过 syscall → vcore/open/rename_utils，一处 hook 覆盖全部文件系统（ext4/tmpfs/overlayfs/fuse/ramfs/...）。无需逐 FS 修改。

4. **Linux 同构**。Linux 的 fsnotify hook 就在 `fs/namei.c`（`do_unlinkat`/`vfs_rename` 等）和 `fs/read_write.c`，即 syscall 层。

> 与 issue 原文「修改各文件系统实现」的建议不同——侦察发现 syscall-core 层是更优的单一锚点。这不是 workaround，是更彻底的去重。

### 2.2 hook 点清单

| 事件 | 位置（已核实） | 通知对象 | name 字段 |
|---|---|---|---|
| `IN_CREATE` | `open.rs` create 成功后 (`:359` `created=true`)；`vcore.rs do_mkdir_at` (`:595` mkdir 返回后) | 父目录的 watch | 子项名 |
| `IN_OPEN` | `open.rs do_sys_open`：在 `File` 构造并 `alloc_fd` 成功、返回 fd 前（约 `open.rs` 末尾成功路径） | 被打开 inode 的 watch | 无 |
| `IN_DELETE` | `vcore.rs do_unlink_at` (`:713` unlink 成功后) | 父目录 watch | 子项名 |
| `IN_DELETE_SELF` + `IN_IGNORED` | `do_unlink_at` 同处（已有 `target_inode`） | `target_inode` 自身 watch；随后移除该 mark | 无 |
| `IN_MOVED_FROM` / `IN_MOVED_TO` | `rename_utils.rs do_renameat2` (`:124` move_to 成功后)；共享 cookie | 源/目标父目录 watch | 子项名 |
| `IN_MOVE_SELF` | `do_renameat2` 同处（已有 `old_inode`） | `old_inode` 自身 watch | 无 |
| `IN_MODIFY` | `File::do_write` 成功后（`file.rs` do_write 返回 `Ok(n)` 处） | 被写 inode 的 watch | 无 |
| `IN_MODIFY`（truncate） | `do_ftruncate`/`resize` 成功路径；`fallocate` 的写类模式（`FALLOC_FL_PUNCH_HOLE`/`ZERO_RANGE`/正常分配，非 `KEEP_SIZE`）成功后 | 被 setattr inode 的 watch | 无 |
| `IN_ACCESS` | `File::do_read` 成功后 | 被读 inode 的 watch | 无 |
| `IN_CLOSE_WRITE` / `IN_CLOSE_NOWRITE` | `File::drop`（= 最后一次 close，见 §2.4） | 被关闭 inode 的 watch | 无 |
| `IN_ATTRIB` | `do_chmod`/`do_chown`/`do_utimensat`/`do_truncate`（setattr syscall-core）成功后；定位见 `kernel/src/filesystem/vfs/syscall/` 各 `do_*` | 被 setattr inode 的 watch | 无 |

`do_symlinkat`(`symlink_utils.rs`)/`do_linkat`(`link_utils.rs`) 成功后 → 父目录 `IN_CREATE`（子项名）。

### 2.3 `fsnotify()` 调用约定

```rust
// filesystem/fsnotify/mod.rs
/// 统一事件投递入口。在 VFS 操作成功后调用。
/// - to_parent: 对子项事件，传父目录 inode + 子项名；
/// - to_child:  对自身事件（DELETE_SELF/MOVE_SELF/MODIFY/CLOSE/OPEN），传目标 inode；
/// 二者可同时非空（如 unlink：父目录得 IN_DELETE，子项得 IN_DELETE_SELF）。
pub fn fsnotify(
    mask: FsEvent,                       // FS_CREATE / FS_MODIFY / ...
    parent: Option<(&Arc<dyn IndexNode>, &str)>,  // (父目录, 子项名)
    child: Option<&Arc<dyn IndexNode>>,            // 目标 inode 自身
    cookie: u32,                         // move 配对用，否则 0
)
```

实现：持全局 `FSNOTIFY` 自旋锁（irqsave），分别用 parent/child 的 `inode_id()` 在全局 mark 索引中查匹配 mark，对每个 mark 调 `group.backend.handle_event(mark, mask, name, cookie, is_dir)`。

**铁律**：`fsnotify()` 内部**只能**获取 `FSNOTIFY` 全局锁与 group 队列锁，**绝不**回调任何 `IndexNode` 方法（避免在 MountFSInode/File 锁下重入 VFS）。所需的 `inode_id` 与 `is_dir` 在调用前由调用方从已持有的 inode metadata 取好传入（或 `fsnotify` 内部仅读 `metadata()`——但为安全起见，调用方预取 `is_dir` 传入更稳妥；inode_id 由 fsnotify 内部读 metadata，因为 inode 活着、metadata 只读不锁，安全）。

> 锁序（全代码库一致）：`MountFSInode/File 锁` → `FSNOTIFY 全局锁` → `group 队列锁`。任何反向获取即 bug。

### 2.4 close 事件为什么放 `File::drop`

`Arc<File>` 的 `Drop` 只在**最后一个引用**释放时执行（Rust 语义保证），等价于 Linux 的 `__fput`。`File::drop`（`file.rs:2151`）已是 epoll 释放、flock 释放、`inode.close()` 的汇聚点。在此根据 `self.mode` 是否含 `FMODE_WRITER` 决定 `IN_CLOSE_WRITE` 还是 `IN_CLOSE_NOWRITE`。

注意：`Drop` 里 inode 的 Arc 仍活着（`self.inode`），可安全取 `inode_id`。

### 2.5 `FMODE_NONOTIFY` 的用途

`file.rs:513` 已定义 `FMODE_NONOTIFY = 0x4000000`（open_fmode 已支持从 flags 传入），但当前无人消费。用途：
- inotify fd 自身的 File 打开时设置 `FMODE_NONOTIFY`。
- 所有 File 层 hook（do_read/do_write/Drop）开头检查：`if self.mode.contains(FMODE_NONOTIFY) { return; }`，避免对 inotify fd 的 read/poll 产生递归事件。

namespace hook（vcore 层）操作的是常规文件，不涉及 `FMODE_NONOTIFY`。

---

## 3. fsnotify 通知层

### 3.1 模块结构

```
kernel/src/filesystem/fsnotify/
  mod.rs        — 事件 mask、fsnotify()、全局 mark 索引、FsNotifyBackend trait
  group.rs      — FsNotifyGroup
  mark.rs       — FsNotifyMark + 生命周期
```

### 3.2 核心数据结构（数据结构优先）

```rust
// === 事件 mask（对应 Linux FS_* 内核事件，与用户态 IN_* 分离）===
bitflags! {
    pub struct FsEvent: u32 {
        const ACCESS     = 0x00000001;  // IN_ACCESS
        const MODIFY     = 0x00000002;  // IN_MODIFY
        const ATTRIB     = 0x00000004;  // IN_ATTRIB
        const CLOSE_WRITE= 0x00000008;  // IN_CLOSE_WRITE
        const CLOSE_NOWRITE=0x00000010; // IN_CLOSE_NOWRITE
        const OPEN       = 0x00000020;  // IN_OPEN
        const MOVED_FROM = 0x00000040;  // IN_MOVED_FROM
        const MOVED_TO   = 0x00000080;  // IN_MOVED_TO
        const CREATE     = 0x00000100;  // IN_CREATE
        const DELETE     = 0x00000200;  // IN_DELETE
        const DELETE_SELF= 0x00000400;  // IN_DELETE_SELF
        const MOVE_SELF  = 0x00000800;  // IN_MOVE_SELF
        // 内核内部
        const UNMOUNT    = 0x00002000;  // 文件系统卸载
        const Q_OVERFLOW = 0x00004000;  // 队列溢出
        const IN_IGNORED = 0x00008000;  // watch 被撤销（inode 删除/卸载）
        const ISDIR      = 0x40000000;  // 事件对象是目录（由 dispatch 设置）
    }
}

/// 一个 watch：连接 group 与 inode。
pub struct FsNotifyMark {
    pub wd: i32,                       // watch descriptor，group 内唯一
    pub group: Weak<FsNotifyGroup>,    // 所属 group（避免环引用）
    pub inode: Arc<dyn IndexNode>,     // 强引用：watch 期间 pin 住 inode（防 evict）
    pub mask: AtomicU32,               // 订阅 mask（IN_MASK_ADD 并发改，必须原子读，对齐 Linux fsnotify_mark.mask）
    pub oneshot: AtomicBool,           // IN_ONESHOT：触发一次后自动撤销
    pub excl_unlink: bool,             // IN_EXCL_UNLINK
}

/// 一个通知消费者（一个 inotify fd 对应一个 group）。
pub struct FsNotifyGroup {
    pub backend: Box<dyn FsNotifyBackend>, // 后端自带内部锁；fsnotify 层只依赖 trait，不反向依赖 inotify 类型
    pub marks: Mutex<Vec<Arc<FsNotifyMark>>>,  // group 拥有的所有 mark（强引用）
    pub wait_queue: WaitQueue,         // read 阻塞 / 唤醒
    pub epitems: LockedEPItemLinkedList, // epoll 集成
}

/// inotify 后端。**事件队列与 wd 表用两把独立锁**，使 read（消费）与 add_watch/rm_watch（wd 管理）不互相阻塞，
/// 对齐 Linux notification_lock（事件）与 group 内 mark 锁分离。
pub struct InotifyBackend {
    // —— 事件锁：handle_event(生产) 与 read(消费) 竞争 ——
    pub events: SpinLock<InotifyQueue>,  // irqsave：fsnotify 可在持 VFS 锁时调用
    pub max_queued_events: usize,        // 常量 16384（见 §6.1），入队前检查
    // —— wd 锁：add_watch/rm_watch/read(wd→mark) 竞争 ——
    pub wd: Mutex<WdTable>,              // wd_counter + wd_map
}
pub struct InotifyQueue {
    pub list: VecDeque<InotifyEventInfo>,
    pub overflowed: bool,                // 置位后后续插入一个 IN_Q_OVERFLOW(wd=-1)
}
pub struct WdTable {
    pub counter: i32,                    // 单调分配 wd（饱和见 §6.2）
    pub map: BTreeMap<i32, Weak<FsNotifyMark>>,
}

/// 队列里的一个事件（已格式化为 inotify 语义，含 wd）。
pub struct InotifyEventInfo {
    pub wd: i32,
    pub mask: u32,                       // 已转为用户态 IN_* mask
    pub cookie: u32,
    pub name: Option<Box<str>>,          // 子项名（目录 watch 的子事件才有）
}

/// 后端接口（最小抽象）。fsnotify 层通过此 trait 调用，保持 VFS↔fsnotify↔inotify 单向依赖。
pub trait FsNotifyBackend: Send + Sync {
    fn handle_event(&self, group: &FsNotifyGroup, mark: &FsNotifyMark,
                    mask: FsEvent, name: Option<&str>, cookie: u32);
    fn free_mark(&self, mark: &FsNotifyMark);   // mark 销毁时从 wd 表移除
    fn free_group(&self);                        // fd close 收尾
    fn queue_nonempty(&self) -> bool;            // poll 用
}
```

### 3.3 全局 mark 索引

```rust
// 用 inode_id 反查「挂在该 inode 上的所有 mark」。
// 存 Weak<FsNotifyMark>：group 拥有 mark（强），索引只做查找，不阻止回收。
static FSNOTIFY_MARKS: SpinLock<MaybeUninit<HashMap<InodeId, Vec<Weak<FsNotifyMark>>>>> = ...;
// 亦可在 FsManager / 一个 FsNotifyRegistry 结构体里，避免全局 static 初始化顺序坑点。
```

**为什么 key 用 `InodeId` 安全**（关键正确性论证）：
- `InodeId` 由 `generate_inode_id()`（`vcore.rs:72`，原子计数器）分配，**仅在 inode 被完全释放后才可能复用**。
- mark 持有 inode 的**强 `Arc`**，故 watch 期间 inode 不可能被 evict，其 `InodeId` 不会被复用。
- mark 从 group 移除时，**同步**从全局索引删除对应 Weak。删除后即使原 inode 释放、`InodeId` 被新 inode 复用，索引里已无该 key，不会误匹配。
- dispatch 时 `Weak::upgrade()` 失败的死引用：惰性清理（upgrade 失败即从 vec 移除），不影响正确性。

> FUSE 的 `inode_generation()`：FUSE 可能复用 inode 号。watch 期间 inode 被 Arc pin，generation 稳定；`InodeId` 在 DragonOS 是全局原子值（非 FS 内部号），不随 FUSE 内部号复用而复用。故无需把 generation 纳入 key。（实现时若发现 FUSE 路径有 inode 对象替换，再以 `Arc::addr` 兜底——留作实现期验证点。）

### 3.4 dispatch 流程（fsnotify 主体）

```
// 全局 watch 计数：绝大多数时刻为 0（系统未使用 inotify）。fsnotify 的第一道闸门，
// 无 watch 时零锁开销——对齐 Linux i_fsnotify_mask/DCACHE_FSNOTIFY_PARENT_WATCHED 的快速跳过。
static TOTAL_WATCHES: AtomicUsize = AtomicUsize::new(0);

fsnotify(mask, parent, child, cookie):
  // ① 快速路径：无任何 watch → 直接返回（read/write/close 热路径零成本）
  if TOTAL_WATCHES.load(Relaxed) == 0 { return }
  // ② 收集候选（调用方已预取 is_dir 传入更佳；此处仍可读 metadata，inode 活着且只读）
  lock FSNOTIFY (irqsave)
    for (inode, name_opt) in [(parent.inode, parent.name), (child, None)] 若非空:
        id = inode.metadata().inode_id()
        snapshot += FSNOTIFY_MARKS.get(id) 里 upgrade 成功的 Arc<mark>   // 死 Weak 顺手剔除
  unlock FSNOTIFY                                  // 临界区仅哈希查表
  // ③ 锁外投递（不在全局锁内做 backend 工作）
  for each mark in snapshot:
      if (mark.mask.load(Relaxed) & mask.bits()) == 0 { continue }   // 原子读 mask 过滤
      mark.group.backend.handle_event(&group, &mark, mask, name_opt, cookie)  // 内部取 events 锁
      if mark.oneshot { 撤销该 mark }
```

**锁族分离（无死锁/低竞争）**：
- 数据锁族 A（投递路径）：`FSNOTIFY 全局锁`（查表，秒放）→ mark 所在 group 的 `events 锁`（入队）。二者**不嵌套**：全局锁在 handle_event 前释放。
- 数据锁族 B（控制路径）：add_watch/rm_watch 取 `wd 锁` + `marks 锁` + 全局索引锁；**不取 events 锁**。
- read 路径只取 `events 锁`。
- 故 events 锁 与 wd/marks/全局索引锁 几乎不相交 → read 不阻塞 add_watch，反之亦然。
- 与外部 VFS/File 锁的顺序：`MountFSInode/File 锁`（调用方已持有）→ `FSNOTIFY` → `events`。永不反向。

---

## 4. inotify 设备层

### 4.1 模块结构

```
kernel/src/filesystem/inotify.rs
```

### 4.2 伪文件实现（照搬 eventfd/signalfd 模式，不抽 anon_inode 公共框架）

```rust
pub struct InotifyFs { /* FileSystem：返回 magic，无挂载 */ }   // 类比 EventFdFs
#[cast_to([crate::filesystem::vfs::CastTo])]  // 沿用现有 cast 宏
pub struct InotifyInode {
    group: Arc<FsNotifyGroup>,
    // read_at/poll 直接委托给 group
}
impl IndexNode for InotifyInode { /* read_at / metadata / fs / is_stream=true ... */ }
impl PollableInode for InotifyInode { /* poll/add_epitem/remove_epitem 委托 group */ }
```

**为什么不做 anon_inode 公共框架**：eventfd/signalfd 各自实现伪 FS，工作良好；抽取公共框架是独立重构，会动到 eventfd/signalfd（引入 regression 风险），且对 inotify 无收益。遵循「渐进演化 > 革命性重写」。inotify 照搬现有模式即可。**anon_inode 统一框架列为后续可选重构**（不在本 issue 范围）。

### 4.3 read 语义（必须精确）

`struct inotify_event`（小端，无 padding）：

```c
struct inotify_event {
    int    wd;       // 4
    uint32_t mask;   // 4
    uint32_t cookie; // 4
    uint32_t len;    // 4 = name 长度（含末尾 NUL，向上对齐到 8 的倍数）
    char   name[];   // 变长，NUL 填充到 len
};
```

`read(fd, buf, count)`：
1. 若 `count < sizeof(inotify_event)`（16）→ `EINVAL`。
2. 从队列头部逐个取事件，序列化写入 buf，**只写完整事件**（写不下的留在队列，下次读）。
3. 一个事件之后，若剩余空间 ≥ 下一个事件大小，继续打包；否则停止。
4. `name` 按 Linux：长度 = `strlen+1`，向上对齐到 8 字节倍数，不足补 NUL。
5. 阻塞（`O_NONBLOCK` 未设且队列空）→ `wq_wait_event_interruptible!`，被信号打断 → `EINTR`/`ERESTARTSYS`。
6. `O_NONBLOCK` 且队列空 → `EAGAIN`。

`is_stream() = true`（inotify fd 不可 seek；pread/pwrite/lseek → `ESPIPE`），`read_at` 忽略 offset，从队列头出队。

### 4.4 poll / epoll

```rust
fn poll(&self, ..) -> Result<usize> {
    if !group.backend.events.is_empty() { Ok(EPOLLIN.bits() | EPOLLRDNORM.bits()) }
    else { Ok(0) }
}
```

事件入队后：`wait_queue.wakeup_all()` + `EventPoll::wakeup_epoll(&group.epitems, EPOLLIN|EPOLLRDNORM)`。完全类比 `eventfd.rs` / `signalfd.rs`。

---

## 5. 生命周期管理（最易出 bug 处）

### 5.1 add_watch

```
sys_inotify_add_watch(fd, path, mask):
  1. 从 fd 取 File → 取 InotifyInode → group
  2. 解析 path → inode（遵循 IN_DONT_FOLLOW：不跟随末尾 symlink）
  3. 权限检查：`permission::check_inode_permission(&inode, &md, PermissionMask::MAY_READ)` 失败 → EACCES。对齐 Linux `inode_permission(MAY_READ)`，防止无读权限者通过监听泄露文件名/元数据（评审 Blocker 1 落实点，本就已在设计中，此处明确 API）。
  4. IN_ONLYDIR 且非目录 → ENOTDIR
  5. 同 inode 上已有该 group 的 mark：
       - IN_MASK_CREATE → EEXIST
       - IN_MASK_ADD → 原子 mask.fetch_or(新 mask)（不替换、不换 wd）
       - 否则 → mask.store(新 mask)，返回原 wd
  6. 超 max_user_watches → ENOSPC
  7. 分配 wd（group 内单调，饱和见 §6.2），建 FsNotifyMark{inode: 强 Arc, mask: AtomicU32}
  8. 加入 group.marks；加入全局索引 inode_id → Weak<mark>
  9. `TOTAL_WATCHES.fetch_add(1)`（维护 §3.4 快速路径计数；原子，无锁）
  10. 返回 wd
```

> **TOTAL_WATCHES 维护**：add_watch +1；rm_watch / mark 因 DELETE_SELF/UNMOUNT 撤销 / group 销毁 -1。归零后 fsnotify 即走零开销快速路径。计数为近似值（Relaxed），仅用于短路，不影响正确性。

### 5.2 rm_watch

```
sys_inotify_rm_watch(fd, wd):
  从 wd 表取 mark → 从 group.marks 与全局索引移除 → TOTAL_WATCHES.fetch_sub(1) → drop 强引用
  wd 无效 → EINVAL
```

### 5.3 inode 被删除（unlink/rmdir）— 关键

- 父目录 watch 得 `IN_DELETE`/（rmdir 时子项含 `IN_ISDIR`）。
- **若被删 inode 自身有 mark**：其 group 得 `IN_DELETE_SELF` + `IN_IGNORED`，然后该 mark 从 group 与全局索引移除（强引用释放，inode 方可真正 evict）。
- 见 §2.2，hook 在 `do_unlink_at`/`do_remove_dir` 成功后，`target_inode` 已在手。

### 5.4 inode 被移动（rename）— 关键

- 源父目录 watch 得 `IN_MOVED_FROM`（带 cookie）。
- 目标父目录 watch 得 `IN_MOVED_TO`（同 cookie，便于用户态配对）。
- 若被移动 inode 自身有 mark：得 `IN_MOVE_SELF`。mark **不**删除（inode 仍存活，只是换了位置）。
- cookie：本次 rename 内 `AtomicI32::fetch_add(1)` 取一个，FROM/TO 共用；0 表示无 move。

### 5.5 group 销毁（inotify fd close）

`File::drop` → 触发 inode.close → InotifyInode 收尾：
- 遍历 group.marks，逐个从全局索引移除，drop。
- 唤醒 wait_queue / 清理 epitems。
- group 释放。所有 watch 自然失效（符合 fd 关闭后 watch 失效语义）。

### 5.6 文件系统卸载（unmount）

- 遍历该 sb 上所有 mark 的 inode，发 `IN_UNMOUNT` + `IN_IGNORED`，移除 mark。
- DragonOS 卸载路径需插入一次 mark 扫描（实现期定位卸载入口）。**这是语义铁律，不能省**（Linux `fsnotify_sb_delete`）。列为必须项；若卸载路径当前不发，先记 TODO 但不阻塞 inotify 主体——卸载场景在 ANOLISA 用例中不会触发（skillfs/memory 是长挂载）。

---

## 6. 约束、限制与边界

### 6.1 资源限制（常量，先不接 procfs sysctl）

| 限制 | 默认值 | 超限错误 |
|---|---|---|
| `max_user_instances` | 128 | inotify_init1 → `EMFILE` |
| `max_user_watches` | 8192 | inotify_add_watch → `ENOSPC` |
| `max_queued_events` | 16384 | 超限后丢事件，插入一个 `IN_Q_OVERFLOW`（wd=-1） |
| 单事件队列字节上限 | 按现有 VecDeque 自然增长，配 max_queued_events 上限即可 | — |

`/proc/sys/fs/inotify/*` sysctl 暴露列为后续增强（不影响内核正确性）。**强制执行**：`inotify_init1`/`add_watch` 在分配前检查对应上限并返回错误；`handle_event` 入队前检查 `max_queued_events`，超限即丢弃并置 `overflowed`，随后插入单个 `IN_Q_OVERFLOW`（wd=-1，仅一次，清 flag）。这是防止恶意/失控进程撑爆内核内存的硬约束（评审 Blocker 2 落实点）。

### 6.2 wd 分配

group 内单调递增 `i32`（正数）。Linux 用 idr 回收 wd；DragonOS 用单调计数器——**不引入 idr = 不引入不需要的复杂度**。溢出处理（评审 Minor 落实点）：wd 仅取 `1..=i32::MAX-1`；counter 饱和，再 add_watch 时返回 `ENOSPC`（单 group 21 亿次 watch 才触发，可接受；绝不产生负 wd，因 `-1` 被 `IN_Q_OVERFLOW` 占用）。`cookie: u32` 用 `wrapping_add` 回绕，回绕合法（Linux 同）。

### 6.3 事件合并（coalescing）

为匹配 Linux 行为并防止 write 风暴：
- 入队前，若队列**末尾**事件与本事件 `(wd, mask, cookie, name)` 完全相同且 mask 属于可合并类（`ACCESS`/`MODIFY`，无 name），则**丢弃**新事件（Linux `inotify_merge` / `event_compare`）。
- 带不同 name 或不同 cookie 的事件不合并。

合并是优化也是正确性（避免 `IN_Q_OVERFLOW`）。但**不是**语义硬要求——即便不合并，行为仍合法，只是事件多。实现里做最简末尾去重。

### 6.4 用户态 mask 位（完整支持）

事件位：`IN_ACCESS/MODIFY/ATTRIB/CLOSE_WRITE/CLOSE_NOWRITE/OPEN/MOVED_FROM/MOVED_TO/CREATE/DELETE/DELETE_SELF/MOVE_SELF`，`IN_ISDIR`（dispatch 设置，非订阅）。

控制位（add_watch 传入）：
- `IN_DONT_FOLLOW` — 路径解析不跟随末尾 symlink。
- `IN_EXCL_UNLINK` — 子项被 unlink 后不再为它产生事件（入队时按 mark 的此位过滤 unlinked 子项事件）。
- `IN_MASK_ADD` — 增量并，不替换。
- `IN_MASK_CREATE` — 已存在则 EEXIST。
- `IN_ONESHOT` — 触发一次后自动撤销。
- `IN_ONLYDIR` — 仅当目标是目录。

init1 控制位：
- `IN_CLOEXEC` → fd 设 close-on-exec。
- `IN_NONBLOCK` → fd 的 File 设 `O_NONBLOCK`。

---

## 7. syscall 注册

4 个号已在 `kernel/src/arch/x86_64/syscall/nr.rs` 定义（253/254/255/294），未注册 handler。

在 `kernel/src/syscall/` 新增模块（或 inotify.rs 内），用 `declare_syscall!` 注册：

| nr | handler | 签名约定（沿用 Syscall trait） |
|---|---|---|
| 253 `sys_inotify_init` | 无参，等价 `inotify_init1(0)` | `-> Result<usize>` 返回 fd |
| 294 `sys_inotify_init1` | `(flags: u32)` | 解析 `IN_CLOEXEC`/`IN_NONBLOCK` |
| 254 `sys_inotify_add_watch` | `(fd, path: *const u8, mask: u32)` | `vfs_check_and_clone_cstr` 取路径 |
| 255 `sys_inotify_rm_watch` | `(fd, wd: i32)` | — |

fd 创建流程（沿用 eventfd）：
```
let inode = Arc::new(InotifyInode::new(group));
let mut file = File::new(inode, O_RDONLY);
file.mode |= FMODE_NONOTIFY;        // 防递归
if flags & IN_NONBLOCK { file.mode |= O_NONBLOCK 对应位 }
let fd = pcb.fd_table().alloc_fd(Arc::new(file), cloexec)?;
```

> 实现期需确认：`File::new` 的确切参数、`alloc_fd` 的 cloexec 设置方式、`FMODE_NONOTIFY` 与 FileMode 的关系（是否需新增位）。以 eventfd 的 fd 创建代码为模板。

---

## 8. 改动文件清单

| 文件 | 动作 | 说明 |
|---|---|---|
| `kernel/src/filesystem/fsnotify/mod.rs` | 新增 | mask、fsnotify()、全局索引、Backend trait |
| `kernel/src/filesystem/fsnotify/group.rs` | 新增 | FsNotifyGroup |
| `kernel/src/filesystem/fsnotify/mark.rs` | 新增 | FsNotifyMark + 生命周期 |
| `kernel/src/filesystem/inotify.rs` | 新增 | InotifyFs/Inode/Backend、syscalls |
| `kernel/src/filesystem/mod.rs` | 改 | `pub mod fsnotify; pub mod inotify;` |
| `kernel/src/filesystem/vfs/vcore.rs` | 改 | do_unlink_at/do_remove_dir/do_mkdir_at 后插 fsnotify |
| `kernel/src/filesystem/vfs/syscall/rename_utils.rs` | 改 | do_renameat2 后插 MOVED_FROM/TO/MOVE_SELF |
| `kernel/src/filesystem/vfs/syscall/symlink_utils.rs` | 改 | do_symlinkat 后插 CREATE |
| `kernel/src/filesystem/vfs/syscall/link_utils.rs` | 改 | do_linkat 后插 CREATE |
| `kernel/src/filesystem/vfs/open.rs` | 改 | do_sys_open CREATE 后插 IN_CREATE；成功插 IN_OPEN |
| `kernel/src/filesystem/vfs/file.rs` | 改 | do_read→ACCESS、do_write→MODIFY、Drop→CLOSE_*；FMODE_NONOTIFY 短路 |
| chmod/chown/utimensat/truncate 入口 | 改 | IN_ATTRIB / (truncate)MODIFY |
| syscall 注册文件 | 改 | declare_syscall! ×4 |

**不动**：MountFSInode、各 FS 的 IndexNode 实现（ext4/tmpfs/overlayfs/fuse/...）、eventfd/signalfd。

### 量级估计
- 新增 fsnotify + inotify：~1200–1800 行（含注释）。
- hook 插桩：每处 5–15 行，~10 处 → ~150 行。
- 合计落在 issue 估计的 2000–3000 行区间偏低端（因选择 syscall-core 单点 hook，省去逐 FS 改动）。

---

## 9. 验证计划

### 9.1 单元/集成自测（user/apps/c_unitest 风格）
1. `inotify_init1(O_NONBLOCK|O_CLOEXEC)` 返回有效 fd。
2. `add_watch("/tmp/test", IN_ALL_EVENTS)` 成功，返回 wd≥0。
3. `touch /tmp/test/a` → read 得 `IN_CREATE`；`echo x > a` → `IN_MODIFY`/`IN_CLOSE_WRITE`；`rm a` → `IN_DELETE`。
4. 监听目录、重命名子文件 → `IN_MOVED_FROM`+`IN_MOVED_TO` cookie 相等。
5. `rm` 被监听文件本身 → `IN_DELETE_SELF`+`IN_IGNORED`。
6. epoll inotify fd：有事件时 `EPOLLIN` 触发；无事件阻塞。
7. `O_NONBLOCK` 空队列 read → `EAGAIN`。
8. `count < 16` read → `EINVAL`。
9. 多事件打包、name 对齐到 8 字节。
10. `IN_MASK_ADD`/`IN_MASK_CREATE`/`IN_ONESHOT`/`IN_ONLYDIR` 行为。
11. fd 关闭后所有 watch 失效。

### 9.2 回归保护（铁律：不破坏现有功能）
- `make kernel` 通过编译。
- 既有文件系统测试（ext4/tmpfs）不受影响：hook 在「成功之后」且不改变返回值。
- 特别检查：`FMODE_NONOTIFY` 短路不影响 eventfd/signalfd（它们不走新 hook）。
- 大压力写循环不触发 panic / 死锁（锁序验证）。

### 9.3 用户态证据
最终交付时附上：自测程序源码 + 运行输出（read 得到的 `inotify_event` hexdump）、epoll 触发日志、`make kernel` 编译成功日志。

---

## 10. 风险与缓解

| 风险 | 缓解 |
|---|---|
| hook 在持有 VFS 锁下调用导致死锁 | `fsnotify()` 只取自己的锁；调用方预取 is_dir；不在 hook 内回调 IndexNode 写方法 |
| mark 悬空引用 | mark 强引用 pin inode；删除时同步清索引；dispatch 惰性清理死 Weak |
| inode_id 复用误匹配 | 强引用保证 watch 期 id 不复用；见 §3.3 论证 |
| close 事件误触（多次 Drop） | Rust `Drop` 仅最后一次执行；天然等价 `__fput` |
| FUSE inode 对象替换 | 实现期验证；必要时用 `Arc::addr` 兜底 key |
| 事件风暴 / 队列溢出 | max_queued_events + 末尾去重 + IN_Q_OVERFLOW |
| 递归（inotify fd 自身被监听） | `FMODE_NONOTIFY` 短路 |
| unmount 不发 IN_UNMOUNT | 先标记 TODO，不阻塞主体；ANOLISA 用例不触发 |

---

## 11. 非目标（明确不做）

- fanotify。
- superblock / mount 级 mark（仅 inode mark）。
- anon_inode 公共框架重构（照搬 eventfd 模式）。
- `/proc/sys/fs/inotify/*` sysctl 读写（仅内核常量）。
- procfs/sysfs 的 inotify（仅覆盖经 syscall-core 的常规/挂载 FS）。
- idr wd 回收（单调计数器）。

这些是**有意识的范围裁剪**，均不损害 inotify 核心语义与 ANOLISA 用例。每条都可独立后续迭代。


---

## 12. 对抗性评审记录（maintainer 裁决）

独立 reviewer subagent 对方案做了 7 维对抗性审查，maintainer 逐条裁决如下：

| 评审意见 | 严重度 | 裁决 | 落实 |
|---|---|---|---|
| add_watch 缺读权限检查 | Blocker | **驳回（误读）** | §5.1 本就有；明确为 `check_inode_permission(MAY_READ)` |
| 队列/实例/watch 无上限致 OOM | Blocker | **驳回（误读）** | §6.1 本就有；补强「强制执行」段 |
| mask 无锁读 = 数据竞争 | Blocker | **采纳** | mask 改 `AtomicU32`（§3.2/§3.4） |
| 全局锁无快速路径 | Major | **采纳** | `TOTAL_WATCHES` 原子短路（§3.4） |
| hook 定位不精确 + fallocate 遗漏 | Major | **采纳** | 收紧行号；补 fallocate→IN_MODIFY（§2.2） |
| backend 锁粒度过粗 | Major | **采纳** | events 锁与 wd 锁拆分（§3.2/§3.4） |
| trait 写死具体类型 | Major | **采纳** | backend 改 `Box<dyn FsNotifyBackend>`（§3.2） |
| wd 溢出 | Minor | **采纳** | 饱和到 i32::MAX-1，溢出返 ENOSPC（§6.2） |

6 条有效改进已纳入；2 条误读已驳回并顺手收紧表述。**结论：方案可进入实施。**