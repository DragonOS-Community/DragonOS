# DragonOS inotify：用户语义与架构设计

> 对应 issue：[DragonOS-Community/DragonOS#2151](https://github.com/DragonOS-Community/DragonOS/issues/2151)
>
> 兼容基线：Linux 6.6.139
>
> 状态：**已实现并持续维护**


### 0. 设计目标

DragonOS 的 inotify 以 Linux 6.6.139 为兼容基线，遵循四个原则：

1. **Linux 用户语义优先**：系统调用、事件布局、mask、错误码、事件顺序和 read/poll 行为应与 Linux 一致。
2. **通知不能改变原操作结果**：事件投递是尽力而为的。文件操作一旦成功，通知侧的队列满或内存不足不能把它改成失败。
3. **热路径低开销**：没有 watch 时直接跳过；有 watch 时也尽量只检查当前对象，而不是争用系统级全局锁。
4. **状态有明确所有者**：watch、对象删除、卸载和队列溢出都有明确的状态转换，避免重复释放、漏通知或事后猜测。

inotify 是用户接口，fsnotify 是内核中的事件路由层。目前 DragonOS 只提供 inotify 后端，不预先实现 fanotify、mount mark 或 Linux 完整的 SRCU/connector 体系。

### 1. 用户看到的模型

一个 inotify 实例就是一个只读文件描述符：

```text
inotify_init1()
       │
       ▼
  inotify fd ── inotify_add_watch(path, mask) ──► wd
       │
       ├── read() 读取若干 inotify_event
       ├── poll()/epoll() 等待队列可读
       ├── ioctl(FIONREAD) 查询当前可读字节数
       └── close() 撤销该实例的全部 watch
```

- `fd` 表示一个独立事件消费者，拥有自己的事件队列。
- `wd`（watch descriptor）只在该 inotify 实例内有意义。
- 对同一实例中的同一文件重复 `inotify_add_watch()` 会更新原 watch，并返回原来的 `wd`。
- 一个 watch 监听的是**文件系统对象**，不是一段永久不变的路径字符串。硬链接别名共享同一个对象 watch；目录 watch 收到的子项事件则带发生事件时的当前名称。

#### 1.1 目录 watch 与对象自身 watch

理解事件路由最重要的规则是区分“父目录中的子项事件”和“对象自身事件”：

| 操作 | 父目录 watch | 对象自身 watch |
|---|---|---|
| 创建、删除、移入、移出子项 | 收到，带子项名 | 不以父目录事件形式收到 |
| 打开、读取、写入、关闭、修改属性 | 收到，带当前子项名 | 收到，不带名称 |
| 被监听对象自身移动 | 不适用 | `IN_MOVE_SELF` |
| 被监听对象最终删除 | 父目录先收到删除事件 | `IN_DELETE_SELF`，随后 `IN_IGNORED` |

因此，监听一个目录可以观察其中子项的活动；监听文件本身可以跨 rename 或硬链接别名继续跟踪同一对象。

### 2. 总体架构

实现分为四层，每层只承担一种职责：

```text
┌──────────────────────────────────────────────────────────────┐
│ VFS 与文件操作                                               │
│ 在 open/read/write/metadata/namespace 操作的提交边界产生事件 │
└─────────────────────────────┬────────────────────────────────┘
                              ▼
┌──────────────────────────────────────────────────────────────┐
│ fsnotify 路由层                                               │
│ 识别父目录与对象、取得 mark 快照、过滤 mask、管理撤销         │
└─────────────────────────────┬────────────────────────────────┘
                              ▼
┌──────────────────────────────────────────────────────────────┐
│ inotify 后端                                                  │
│ 将内核事件转为 wd/mask/cookie/name，合并并写入实例队列        │
└─────────────────────────────┬────────────────────────────────┘
                              ▼
┌──────────────────────────────────────────────────────────────┐
│ inotify fd                                                    │
│ read / poll / epoll / FIONREAD                               │
└──────────────────────────────────────────────────────────────┘
```

| 层 | 主要职责 | 不负责什么 |
|---|---|---|
| VFS/File | 决定操作何时真正提交，以及事件顺序 | 不管理 watch 或用户队列 |
| fsnotify | 对象身份、父/自身路由、mark 生命周期 | 不解释用户缓冲区 |
| inotify 后端 | 用户 ABI、队列、合并、溢出、唤醒 | 不重新查询文件系统状态 |
| inotify fd | 阻塞/非阻塞读取和 poll/epoll 接口 | 不参与文件系统变更 |

这种分层不是为了抽象而抽象。它确保文件系统操作不依赖某个具体通知消费者，同时让 inotify 的队列规则集中在一个位置。

### 3. 事件如何产生

#### 3.1 以“已提交的事实”为准

普通事件在相应操作达到稳定提交点后产生。通知代码不会为了判断结果，再次读取可能已经变化的 metadata。

命名空间操作尤其需要准确的提交边界。DragonOS 的底层文件系统会返回带类型的结果，例如“删除后仍有其他硬链接”或“这是最后一个链接”。Mount/VFS 只消费这个权威结果，不使用操作后的 `nlink` 猜测。FUSE、OverlayFS、ext4、tmpfs、ramfs 等文件系统各自在拥有真实状态和串行锁的位置生成结果。

这带来两个好处：

- 不会因为缓存过期或额外的 FUSE `GETATTR` 得到陈旧链接数。
- 不会在文件系统操作已经成功后，因为通知记账分配失败而改变 syscall 结果。

#### 3.2 事件顺序

事件顺序属于兼容语义，而不是日志上的偶然结果。关键顺序如下：

| 操作 | 主要事件顺序 |
|---|---|
| 创建子项 | 提交创建 → 父目录 `IN_CREATE` |
| 删除普通文件 | 文件 `IN_ATTRIB`（链接数变化）→ 父目录 `IN_DELETE` → 最终 detach 时对象 `IN_DELETE_SELF` / `IN_IGNORED` |
| 删除目录 | 父目录 `IN_DELETE|IN_ISDIR` → 最终对象删除 |
| 普通 rename | `IN_MOVED_FROM` → `IN_MOVED_TO` → 被覆盖目标的 `IN_ATTRIB`（若有）→ 源对象 `IN_MOVE_SELF` |
| `RENAME_EXCHANGE` | 两组独立的 FROM/TO 配对事件，双方各有 `IN_MOVE_SELF` |
| 写入时清除 set-id 位 | `IN_ATTRIB` → `IN_MODIFY` |

同一次移动的 `IN_MOVED_FROM` 和 `IN_MOVED_TO` 使用相同的非零 cookie，便于用户态配对。交换 rename 使用两组 cookie。

对于同一父目录中的并发创建、删除和重命名，通知在父目录的命名空间串行范围内发布，防止后提交操作的事件越过先提交操作。

#### 3.3 内容与属性事件

当前实现覆盖常见 Linux 数据路径：

- 打开与执行：`IN_OPEN`
- read/pread/readv/preadv 及文件传输源：`IN_ACCESS`
- write/pwrite/writev、truncate、fallocate 及文件传输目标：`IN_MODIFY`
- chmod/chown/timestamp/xattr、链接数变化和写入导致的权限位变化：`IN_ATTRIB`
- 最后一个 open file description 关闭：`IN_CLOSE_WRITE` 或 `IN_CLOSE_NOWRITE`

即使内核为了内存边界把一次大 I/O 分成多个内部块，用户可见的单次 syscall 也只产生一次对应的 ACCESS/MODIFY 通知。内核内部 I/O 和带 `FMODE_NONOTIFY` 的文件不会递归产生通知。

### 4. 对象身份与生命周期

#### 4.1 稳定对象身份

挂载文件系统中的通知身份由以下三部分组成：

```text
(superblock identity, inode identity, inode generation)
```

这可以区分不同挂载中相同的 inode 号，也能防止 inode 号复用后误投事件。硬链接别名在同一 superblock 中解析到同一对象状态，因此：

- 从任一别名建立的对象 watch 都能观察其他别名上的对象事件。
- rename 不会让对象 watch 失效。
- 父目录事件仍按实际发生操作的目录和名称路由。

watch 持有被监听对象的强引用，索引和事件快照只保存弱引用，避免引用环。

#### 4.2 最后链接删除状态机

`unlink()` 只删除一个目录项，不一定删除对象。DragonOS 将对象删除建模为三个阶段：

```text
     删除一个非最后链接
Linked ─────────────────────────► Linked
   │
   │ 删除最后链接
   ▼
Zero-link pending
   │  最后一个断连 dentry/open fd 脱离对象
   ▼
Delete committed ──► IN_DELETE_SELF ──► IN_IGNORED

Zero-link pending ── 成功重新链接 ──► Linked
```

这解释了一个容易误解的现象：文件被 unlink 后，如果仍由打开的 fd 持有，直接对象 watch 不会立即收到 `IN_DELETE_SELF`；它在对象真正脱离最后一个 dentry/open-file 生命周期边界时才收到。若对象在此期间被合法重新链接，待删除状态会被取消。

链接变更由对象级协调器排序，add-watch 不持有该协调器跨文件系统或 FUSE I/O。这样既保证最后链接事实不会乱序，又避免用户态 FUSE daemon 重入时形成锁环。

#### 4.3 watch 生命周期

每个 watch 的生命周期是显式的：

```text
Allocated (inactive)
      │  group/wd/index/quota 全部准备完成
      ▼
Active
      │  rm_watch / one-shot / object delete / unmount / fd close
      ▼
Retiring ── 唯一清理令牌 ──► Retired
```

`Active` 是最后发布的状态。事件分发只能看到完整初始化的 active watch；发布失败则按相反顺序回滚。撤销时只有一个执行者能取得清理责任，因此 wd、quota、对象计数和索引都只释放一次。

- 显式 `inotify_rm_watch()`、one-shot、对象删除和卸载会产生 `IN_IGNORED`。
- 关闭 inotify fd 会撤销该实例的全部 watch，但队列已无消费者，因此不再排入 `IN_IGNORED`。

#### 4.4 卸载

首次 add-watch 必须通过 superblock 的卸载准入检查。最终卸载进入关闭阶段后，不再允许新 watch 发布；已有对象的本地 mark 快照被逐一投递 `IN_UNMOUNT`，随后 watch 撤销并产生 `IN_IGNORED`。

`IN_UNMOUNT` 是每个 watch 的内建生命周期关注项，用户不需要显式把它加入订阅 mask。

### 5. 事件分发、队列与读取

#### 5.1 分发与过滤

fsnotify 先取得不可变的 mark 快照，然后释放索引锁，再逐个分发。后端入队、wake-up 和 watch 清理不会在对象索引锁内执行。

分发时应用以下规则：

- 只保留 watch 订阅的事件位。
- 目录子项事件带 `name`；对象自身事件不带名称。
- 子项是目录时，适用的事件附加 `IN_ISDIR`；为兼容 Linux，`IN_DELETE_SELF` 和 `IN_MOVE_SELF` 不附加该位。
- `IN_EXCL_UNLINK` 抑制已断连路径上的 ACCESS/MODIFY 等 path-data 事件；不应抑制 ftruncate 等 dentry 属性/数据事件。
- `IN_ONESHOT` 在首个成功匹配的事件后撤销。队列已满导致事件丢失仍视为触发；事件对象分配失败则与 Linux 一样只记录 overflow，不消费 one-shot。

#### 5.2 合并与溢出

连续、尚未被读取且具有相同 `wd`、mask 和 name 的事件会合并。`IN_IGNORED` 不合并，移动 cookie 不参与相等判断，这与 Linux 的 inotify 合并规则一致。

队列最多保存 16384 个逻辑事件。队列满或事件分配失败时：

1. 原文件系统操作保持成功。
2. 内核记录一个逻辑 `IN_Q_OVERFLOW` 边界。
3. 用户读取到 `wd = -1`、`cookie = 0` 的 overflow 事件。

overflow 状态不依赖再次分配队列节点，因此在低内存场景下仍能报告事件丢失。边界之前已接受的事件仍排在 overflow 前面，之后接受的事件排在它后面。

#### 5.3 read、poll 与 epoll

`inotify_event` 使用 Linux ABI：16 字节固定头，名称包含 NUL，并按 16 字节边界填充。

- 缓冲区小于 16 字节，或放不下队首完整事件：`EINVAL`。
- 非阻塞 fd 的空队列：`EAGAIN`。
- 阻塞 fd 的空队列：可被信号中断，否则等待新事件。
- 一次 read 只返回完整事件，不拆分记录。
- 多个 reader 可以逐事件竞争消费；把事件复制到用户空间时不持队列锁。
- 用户空间复制失败时，该事件已出队，并按 Linux inotify 的专用规则返回 `EFAULT`。
- 队列从空变为可读时会唤醒 read 等待者，并通知 poll/epoll。
- `ioctl(FIONREAD)` 返回当前完整逻辑队列可读的序列化字节数，包括待发的 overflow 记录。
- inotify fd 是 stream-like、不可 seek 的对象；pread/pwrite/lseek 不适用。

### 6. 性能与故障隔离

#### 6.1 多级快速路径

事件生产是 open/read/write/close 的热路径，不能因为系统中存在一个无关 watch 就争用全局锁。当前实现依次使用以下短路：

```text
系统无任何 watch？ ── 是 ──► 返回
        │ 否
当前 superblock 无 watch？ ── 是 ──► 返回
        │ 否
当前 dentry 的负缓存仍有效？ ── 是 ──► 返回
        │ 否
当前对象或父目录有 watch？ ── 否 ──► 更新负缓存并返回
        │ 是
        ▼
克隆对象局部 mark 快照，锁外分发
```

具体设计包括：

- **全局 presence 计数**：系统完全没有 watch 时，热路径只有一次原子读取。
- **per-superblock 计数**：无关挂载不会解析对象状态。
- **目录 watch 计数**：没有目录 watch 时，不复制 parent/name。
- **对象局部不可变快照**：mounted 对象命中事件时不扫描系统级全局索引。
- **epoch 校验的负缓存**：确认某 dentry 的对象和父目录都无人监听后，后续 I/O 只做原子校验；watch 从 0↔1 变化或 rename/unlink 拓扑变化会使缓存失效。
- **全局后备索引**：只用于没有 mounted object state 的匿名或特殊对象；支持对象自身计数提示的实现仍可提前短路。

watch 增删属于冷路径，允许构造新的不可变 mark 列表；事件分发只克隆 `Arc` 快照，不在锁内遍历和执行后端工作。这里没有引入 RCU、分片表或无锁状态字，因为当前互斥锁加不可变快照已能表达所需不变量。

#### 6.2 内存不足与回滚

- add-watch 在变为 active 前完成所有可能失败的分配和 quota 预留；任何失败都不会留下半发布 watch。
- watch 撤销即使无法分配新的压缩快照，也会继续完成资源清理；失效的弱引用可稍后惰性清理。
- 事件名称分配失败和队列满统一转为 `IN_Q_OVERFLOW`。
- 命名空间 mutation 不会为了创建通知状态而分配；通知记账不能让原本可成功的 unlink/rename 变成 `ENOMEM`。

### 7. 用户 API 与支持范围

#### 7.1 系统调用

| 接口 | 行为 |
|---|---|
| `inotify_init()` | 创建阻塞实例；仅 x86_64 保留该旧 ABI |
| `inotify_init1(flags)` | 支持 `IN_NONBLOCK`、`IN_CLOEXEC` |
| `inotify_add_watch(fd, path, mask)` | 添加或更新 watch，检查目标读权限 |
| `inotify_rm_watch(fd, wd)` | 显式撤销 watch |

`IN_DONT_FOLLOW`、`IN_ONLYDIR`、`IN_EXCL_UNLINK`、`IN_MASK_ADD`、`IN_MASK_CREATE` 和 `IN_ONESHOT` 均受支持。未知位、空 mask，以及同时使用 `IN_MASK_ADD|IN_MASK_CREATE` 会按 Linux 语义返回错误。

#### 7.2 事件

实现支持标准事件位：

```text
IN_ACCESS       IN_MODIFY       IN_ATTRIB
IN_CLOSE_WRITE  IN_CLOSE_NOWRITE IN_OPEN
IN_MOVED_FROM   IN_MOVED_TO     IN_CREATE
IN_DELETE       IN_DELETE_SELF  IN_MOVE_SELF
IN_UNMOUNT      IN_Q_OVERFLOW   IN_IGNORED
IN_ISDIR
```

#### 7.3 资源限制

当前限制为内核常量，尚未接入 `/proc/sys/fs/inotify/*`：

| 资源 | 限制 |
|---|---:|
| 每个用户最多实例数 | 128 |
| 每个用户最多 watch 数 | 8192 |
| 每个实例最多排队事件数 | 16384 |

配额按当前 user namespace 与有效 UID 计费，并沿祖先 user namespace 计费，防止通过嵌套 namespace 重置限制。实例超限返回 `EMFILE`，watch 超限返回 `ENOSPC`。

### 8. 常见场景

#### 8.1 监听目录中新文件

```text
watch(dir, IN_CREATE | IN_OPEN | IN_MODIFY)

create dir/a  → IN_CREATE(name="a")
open dir/a    → IN_OPEN(name="a")
write dir/a   → IN_MODIFY(name="a")
```

#### 8.2 配对 rename

```text
dir/a ──rename──► dir/b

IN_MOVED_FROM(name="a", cookie=42)
IN_MOVED_TO  (name="b", cookie=42)
```

用户态应使用 cookie 配对，而不要假设队列中相邻的任意两个 move 事件必然属于同一次操作。

#### 8.3 unlink 后仍由 fd 持有

```text
open(file) → watch(file) → unlink(file)

父目录：IN_DELETE
对象自身：暂时没有 IN_DELETE_SELF

close(last fd)
对象自身：IN_DELETE_SELF → IN_IGNORED
```

如果文件还有另一个硬链接，删除一个名称只产生链接数/父目录事件，不会结束对象 watch。

### 9. 当前边界与非目标

- 当前后端是 inotify；fanotify、dnotify、mount mark 和 superblock mark 不在本实现范围内。
- 资源上限是编译期常量，不提供 Linux 的 inotify sysctl 调节接口。
- mounted 文件系统使用对象局部 mark 快照；匿名与部分特殊 inode 使用全局后备身份。两者对用户提供相同的 inotify ABI。
- fsnotify 是异步观察机制，不是事务日志。发生 `IN_Q_OVERFLOW` 后，用户态必须重新扫描并重建状态。

### 10. 实现位置与验证

主要实现边界：

| 位置 | 职责 |
|---|---|
| `kernel/src/filesystem/fsnotify/` | 事件路由、对象状态、mark/group 生命周期 |
| `kernel/src/filesystem/inotify.rs` | syscall、队列、ABI、read/poll/epoll、quota |
| `kernel/src/filesystem/vfs/mount/mod.rs` | mounted 对象身份、dentry 快照、删除与卸载生命周期、热路径缓存 |
| `kernel/src/filesystem/vfs/file.rs` | open/read/write/close 等文件事件入口 |
| `kernel/src/filesystem/vfs/syscall/` | rename、link、xattr、I/O transfer 等 syscall 级提交事件 |
| 各具体文件系统 | 返回权威 link/rename/fallocate 结果，不管理 inotify 队列 |

兼容语义参考 Linux 6.6.139 的 `fs/notify/`、`include/linux/fsnotify.h`、`include/linux/fsnotify_backend.h` 和 `include/uapi/linux/inotify.h`。DragonOS 的回归覆盖位于：

- `user/apps/tests/dunitest/suites/normal/inotify_events.cc`
- `user/apps/tests/dunitest/suites/normal/inotify_dir_watch.cc`
- FUSE、OverlayFS、fallocate 与并发 namespace 的相关 dunitest
