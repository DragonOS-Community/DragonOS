# DragonOS Linux 风格 errseq 架构设计与实施计划

## 1. 背景与目标

issue #1909 要求 DragonOS 引入 Linux 风格 `errseq_t`，用于记录异步 page-cache writeback 错误，并让每个 open file description 在 `fsync`、`fdatasync`、`msync(MS_SYNC)`、`sync_file_range`、`syncfs` 等同步路径上按 Linux 语义观察这些错误。

当前 DragonOS 已经有两个临时机制：

- `PageCache::writeback_error: SpinLock<WritebackErrorState>`，保存 `seq + Option<SystemError>`。
- `SuperBlockState::wb_error: SpinLock<WritebackErrorState>`，供 `syncfs` 使用。
- `File::wb_error_seq` / `sb_error_seq` 是 per-open-file-description 游标，但目前只稳定接入了 `sync_file_range` 和 `syncfs`，`fsync/fdatasync/msync` 没有统一消费 page-cache writeback error。

这套机制存在三个语义缺口：

- 不是 Linux `errseq_t` 的原子布局，没有 `SEEN` 位，不能避免无观察者时无意义递增，也不具备 Linux 的采样语义。
- `fsync/fdatasync/msync` 即便触发了同步，也可能不检查并推进 file 游标。
- `File` 游标用原子 load/store 更新，缺少 Linux `file->f_lock` 对 `f_wb_err` 的保护，同一个 open file description 被多个线程并发同步时可能重复报告同一个错误。

目标是替换临时机制，而不是并行保留双路径。

## 2. Linux 6.6 语义证据

Linux 6.6.139 的关键路径如下：

- `lib/errseq.c`
  - `errseq_t` 是 `u32`。
  - 低 12 位保存正 errno 编码，`ERRSEQ_SEEN = 1 << 12`，`ERRSEQ_CTR_INC = 1 << 13`。
  - `errseq_set()` 不接受 0 或超过 `MAX_ERRNO` 的错误；记录新错误；只有旧值带 `SEEN` 时才递增 counter。
  - `errseq_sample()` 在当前错误还未被 seen 时返回 0，让新打开的文件仍能在之后第一次 check 时看到该错误。
  - `errseq_check_and_advance()` 把全局 errseq 的 `SEEN` 位置位，把 watcher 游标推进到当前值，并返回当前错误。

- `include/linux/pagemap.h::mapping_set_error()`
  - writeback 失败时记录到 mapping 的 `wb_err`。
  - 同时记录到 superblock 的 `s_wb_err`，让 `syncfs` 能报告文件系统级写回错误。
  - 还维护 `AS_EIO/AS_ENOSPC` legacy 标志，DragonOS 当前没有等价 legacy 调用方，v1 可不引入这两个标志。

- `mm/filemap.c::file_check_and_advance_wb_err()`
  - 先 lockless check mapping 的 `wb_err`。
  - 若有变化，用 `file->f_lock` 保护 `file->f_wb_err`，调用 `errseq_check_and_advance()`。
  - 这是“每个 open file description 独立观察同一个 mapping 错误”的核心。

- `mm/filemap.c::file_write_and_wait_range()`
  - 发起并等待指定范围 writeback。
  - 之后调用 `file_check_and_advance_wb_err()`，即使写回函数本身返回错误，也会推进 file 游标。

- `fs/sync.c::syncfs()`
  - 先 `sync_filesystem(sb)`，再 `errseq_check_and_advance(&sb->s_wb_err, &file->f_sb_err)`。
  - 若 sync 本身有错误，优先返回 sync 错误；否则返回 errseq 错误。

- `mm/msync.c`
  - 对 `MS_SYNC | MAP_SHARED` 的文件映射调用 `vfs_fsync_range(file, fstart, fend, 1)`，即按 fdatasync/range sync 语义消费 writeback error。

## 3. DragonOS 架构设计

### 3.1 通用 ErrSeq 原语

新增 `kernel/src/libs/errseq.rs`：

- `pub type ErrSeqValue = u32`
- `pub struct ErrSeq { value: AtomicU32 }`
- API：
  - `ErrSeq::new()`
  - `sample() -> ErrSeqValue`
  - `set(error: SystemError) -> ErrSeqValue`
  - `check(since: ErrSeqValue) -> Option<SystemError>`
  - `check_and_advance(since: &mut ErrSeqValue) -> Option<SystemError>`

布局保持 Linux 风格：

- `MAX_ERRNO = SystemError::MAXERRNO as u32 = 4095`
- `ERRSEQ_SHIFT = 12`
- `ERRSEQ_SEEN = 1 << 12`
- `ERRSEQ_CTR_INC = 1 << 13`

内存序采用：

- `load(Ordering::Acquire)` 读取当前 errseq。
- `compare_exchange(..., Ordering::AcqRel, Ordering::Acquire)` 发布 `set()` 或 `SEEN` 位更新。

这是保守选择。Linux `READ_ONCE/cmpxchg` 依赖原子性；DragonOS 当前无更细的内存模型封装，用 Acquire/Release 保证错误写入对观察者可见。

### 3.2 PageCache writeback error

替换：

```rust
writeback_error: SpinLock<WritebackErrorState>
```

为：

```rust
writeback_error: ErrSeq
```

保留公开语义但更换类型：

- `sample_writeback_error() -> ErrSeqValue`
- `check_writeback_error(since: ErrSeqValue) -> Option<SystemError>`
- `check_and_advance_writeback_error(since: &mut ErrSeqValue) -> Option<SystemError>`
- `record_writeback_error(error: SystemError)` 内部调用 `ErrSeq::set(error)`

所有 writeback 失败点继续调用 `record_writeback_error()`，并继续通过 `record_writeback_error_for_fs()` 同步记录 superblock errseq。

### 3.3 SuperBlockState writeback error

替换：

```rust
wb_error: SpinLock<WritebackErrorState>
```

为：

```rust
wb_error: ErrSeq
```

API 改为：

- `sample_wb_error() -> ErrSeqValue`
- `check_and_advance_wb_error(since: &mut ErrSeqValue) -> Option<SystemError>`
- `record_wb_error(error: SystemError)`

`syncfs` 仍保持 Linux 优先级：先同步文件系统，再推进 `f_sb_err`，若同步本身失败则优先返回同步错误。

### 3.4 File 游标与并发

`File` 是 DragonOS 的 open file description；`dup/dup2/dup3` 存放同一个 `Arc<File>`，fork 也共享同一个 `Arc<File>`，这符合 Linux open file description 语义。

将：

```rust
wb_error_seq: AtomicU64,
sb_error_seq: AtomicU64,
```

改为：

```rust
wb_error_seq: Mutex<ErrSeqValue>,
sb_error_seq: Mutex<ErrSeqValue>,
```

理由：

- mapping/superblock errseq 是 lock-free 全局状态。
- watcher 游标是 per-open-file-description 可变状态，Linux 明确由 `file->f_lock` 保护。
- DragonOS 用 `Mutex` 保护游标，可以避免同一 `Arc<File>` 上并发 `fsync`/`sync_file_range` 重复推进或重复报告同一错误。

新方法：

- `File::check_and_advance_wb_error(&Arc<PageCache>) -> Result<(), SystemError>`
- `File::check_and_advance_sb_wb_error(&Arc<MountFS>) -> Result<(), SystemError>`

### 3.5 同步路径集成

#### fsync/fdatasync

`sys_fsync::do_fsync()` 当前只调用 `inode.sync_file()`。修改为：

1. 校验 fd 和 O_PATH。
2. 调用 `inode.sync_file(datasync, private_data)`。
3. 无论第 2 步成功或失败，都检查并推进 page-cache errseq。
4. 若第 2 步失败，优先返回该错误；否则返回 errseq 错误。

这匹配 Linux `file_write_and_wait_range()` 中“写回错误和 errseq 检查都执行，返回优先级保留首个同步错误”的语义。

#### sync_file_range

当前路径已经在 `WAIT_BEFORE` / `WAIT_AFTER` 后调用 `file.check_and_advance_wb_error()`，只需要切到新 ErrSeq API。

#### msync

当前 `MS_SYNC | VM_SHARED` 只调用 `file.inode().sync()` 且丢弃错误。修改为：

1. 对每个 `VM_SHARED` 文件映射执行 `file.inode().sync_file(true, file.private_data.lock())`。
2. 随后检查并推进该 `File` 的 page-cache errseq。
3. 按 Linux `msync` 行为返回遇到的错误，而不是静默忽略。

DragonOS 当前 `IndexNode::sync_file` 没有 range 参数，v1 先复用 whole-file datasync；这比静默忽略错误更接近 Linux。后续可增加 range-aware fsync trait，把 `fstart/fend` 精确传入。

#### syncfs

`sys_sync::SysSyncFsHandle` 当前已有 `sync_filesystem()` + `check_and_advance_sb_wb_error()`，改成新 ErrSeq 游标 API即可。

## 4. 实施计划

1. 新增 `kernel/src/libs/errseq.rs` 和 `pub mod errseq`。
2. 替换 `PageCache` 的 `SpinLock<WritebackErrorState>` 为 `ErrSeq`。
3. 替换 `SuperBlockState` 的 `SpinLock<WritebackErrorState>` 为 `ErrSeq`。
4. 将 `File` 的 wb/sb 游标改为 `Mutex<ErrSeqValue>`，调整 `new_with_private_data()` 和 `try_clone()`。
5. 修改 `fsync/fdatasync`：同步后检查 page-cache errseq，错误优先级对齐 Linux。
6. 修改 `msync`：不再吞掉 `sync()` 错误，改用 file-level datasync 并消费 errseq。
7. 保持 `sync_file_range` 的 WAIT 错误检查路径，但接入新 API。
8. 增加 dunitest：
   - 扩展 `fdatasync.cc` 覆盖 `fsync`/`fdatasync` 对普通文件、目录、O_PATH 的正常错误路径。
   - 新增或扩展 `mlock_semantics.cc`/独立 `msync` 用例，验证 `MS_SYNC` 对共享文件映射返回成功且数据可见。
   - 若可低扰动增加 debugfs selftest，则增加 `/sys/kernel/debug/errseq/selftest` 来验证核心 errseq 的 sampled/seen/多 watcher 语义。
9. 执行 `make fmt` 和 `make kernel`。
10. 按 goal 模式启动 DragonOS，并在 guest 内运行相关 dunitest 二进制。

## 5. 第一轮批判性审查

### 架构

把 ErrSeq 放在 `libs` 而不是 `filesystem` 是合理的：Linux 文档也把 `errseq_t` 定义成通用 primitive，而不是 page-cache 专属结构。这样后续 inode、block device、FUSE 或网络文件系统错误源也能复用。

风险：`ErrSeq` 引入 `SystemError`，会让 `libs` 依赖错误类型。DragonOS 现有 `libs` 已广泛被上层模块使用，但很少依赖 VFS；`system_error` 是基础 crate，不造成 VFS 反向依赖。

### 正确性

Linux `errseq_sample()` 在存在未 seen 错误时返回 0，这一点必须保留。否则新打开的 fd 会跳过已有未报告错误，违反“open at time of error 的文件应看到错误”的语义。

DragonOS `File::new_with_private_data()` 在 open 时 sample page-cache errseq。若错误已被某个 watcher seen，则新 fd 不应看到旧错误；若错误未被 seen，则新 fd 后续 fsync 应看到。这与 Linux 一致。

### 并发

全局 errseq 用 atomic；per-file 游标用 `Mutex`。这比当前原子 load/store 更接近 Linux 的 `file->f_lock`。锁只覆盖一个 `u32` 游标和一次 atomic check/advance，不会持有 page cache 锁，不引入明显锁序风险。

### 安全性

错误码只接受 `SystemError`，并通过 `MAXERRNO` 检查后写入低位。非法或超范围错误不会污染 errseq。

### 边界

counter 仍可能环绕，这是 Linux 设计本身承认的低概率风险。保持 `u32` 布局可获得兼容语义；不改成 `u64`，避免“看似更强但不符合 Linux 文档”的差异。

## 6. 第二轮批判性审查

### fsync 错误优先级

如果 `inode.sync_file()` 返回 `EIO`，同时 errseq 也有 `EIO`，不能因为同步失败就跳过 errseq 推进，否则下次 fsync 会重复报告同一错误。计划中第 3 步“无论成功失败都推进 errseq”修正了这个坑点。

### sync_file_range 与 PageState::Dirty

DragonOS 异步 writeback 失败后 `finish_writeback_entry()` 会把页重新设为 Dirty，并记录 writeback error。`wait_writeback_range()` 等待不到 `Writeback` 后返回成功，此时必须依赖 errseq 报告错误。现有 `sync_file_range` 的 WAIT 后检查 errseq 是正确位置；迁移时不能删掉。

### msync 文件游标

Linux `msync` 使用 VMA 的 `vm_file`，因此消费的是该映射持有的 file 游标。DragonOS VMA 也保存 `Arc<File>`，应使用这个 `File` 的游标，而不是只拿 inode 同步。计划已按此处理。

### superblock errseq

PageCache writeback 失败后必须同时记录 superblock errseq，否则 `syncfs` 无法报告。现有 `record_writeback_error_for_fs()` 会遍历 mounted superblocks 并记录错误；迁移到 ErrSeq 时保留该调用。

## 7. 第三轮批判性审查

### 是否过度抽象

不新增复杂 watcher 类型，只使用 `ErrSeqValue` 作为游标。`File` 维护两个游标，分别对应 mapping 和 superblock。这与 Linux `f_wb_err` / `f_sb_err` 一致，抽象足够小。

### 是否有 workaround

方案没有对具体测试硬编码，也没有绕过 writeback 错误。错误仍在真实 writeback 失败路径记录，并由同步 syscall 消费。

### 可实施性

改动集中在：

- `kernel/src/libs/errseq.rs`
- `kernel/src/libs/mod.rs`
- `kernel/src/filesystem/page_cache.rs`
- `kernel/src/filesystem/vfs/file.rs`
- `kernel/src/filesystem/vfs/mount.rs`
- `kernel/src/filesystem/vfs/syscall/sys_fsync.rs`
- `kernel/src/filesystem/vfs/syscall/sys_sync.rs`
- `kernel/src/filesystem/vfs/syscall/sys_sync_file_range.rs`
- `kernel/src/mm/syscall/sys_msync.rs`
- dunitest normal suites

这些文件正好覆盖 Linux 对应的 `errseq.c`、`filemap.c`、`sync.c`、`msync.c` 语义，没有跨越无关子系统。
