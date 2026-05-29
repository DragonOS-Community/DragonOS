# PR 1936 审查意见根因分析与修复计划

## 任务约束

先结合 Linux 代码、问题现象、DragonOS 代码深入研究，再制定 plan；制定后先审查 plan 是否符合 Linux 语义、DragonOS 架构、并发/生命周期不变量、错误路径和边界条件，确认无 workaround、无测试特化、无隐藏坑点后才实施代码变更。

代码变更后，必须再次结合 Linux 代码审查 DragonOS 实现。如果发现语义不一致、架构不合理、边界条件遗漏、并发/生命周期风险或 workaround，必须回到 plan 阶段重新制定修复计划，再继续实施。

所有方案都要参考 Linux 代码、DragonOS 代码、深入研究，并且制定正确、完善、无坑点、无 workaround、架构合理、功能正确的实现/根因修复计划。

Linux 参考代码路径：`~/code/linux-6.6.139/`

## 背景

PR 1936 修复了 `umount()` 在 shared mount propagation 场景中的同线程 `umount_lock` 自死锁。死锁链路是：

```text
umount()
  -> sb_state.umount_write()
  -> do_umount()
  -> propagate_umount()
  -> umount_at_peer()
  -> child.sync_filesystem()
  -> sb_state.umount_read()
```

DragonOS 的 propagated child mount 通过 `MountFS::deepcopy()` 共享同一个 `SuperBlockState`，其中包括非递归 `RwSem` 类型的 `umount_lock`。因此在已经持有 write guard 的同一线程中再获取 read guard 会永久等待。

PR 将顶层 `umount()` 调整为先 `sync_filesystem()`，再持有 `umount_write()` 做 detach/propagation，并移除了 `umount_at_peer()` 中的再次同步。这个方向符合 Linux 中“卸载前做 superblock 同步，传播卸载阶段只处理 mount tree detach”的基本结构。

审查意见指出两个新增问题：

1. 移除 page reclaim 线程中的周期性 `sync_fs_with_umount_read(true)` 后，没有替代的后台元数据回写路径。
2. 新增 dunitest 只把 `/` 标记为 shared，没有构造第二个 peer mount，无法真正覆盖 `umount_at_peer()`。

二次审查结论：两条意见均成立。

## DragonOS 代码事实

### 元数据 dirty 队列

DragonOS ext4 使用 `Ext4FileSystem::dirty_inodes` 保存 dirty inode。`mark_inode_dirty()` 在 inode dirty state 中没有 `QUEUED | WRITEBACK` 时将 inode 入队。`sync_fs()` 调用 `flush_dirty_inodes()`，后者会：

1. 从 `dirty_inodes` 中 `take()` 当前队列；
2. 对每个 inode 获取 per-inode `io_guard`；
3. 快照 `SIZE_DIRTY | MTIME_DIRTY`；
4. 调用 `commit_inode_metadata()`；
5. 成功后只清除快照中仍未变化的 dirty bit；
6. 若写回期间又产生 dirty，则重新设置 `QUEUED` 并 requeue。

这个实现支持并发 dirty 更新，不要求后台线程直接操作 dirty 队列内部结构。正确入口应继续走文件系统的 `sync_fs(wait)`。

### 现有同步入口

当前显式同步入口包括：

1. `sync()`：全局 flush dirty pages，然后遍历 `list_unique_mounted_superblocks()`，执行 inode page writeback、`sync_fs(false)`、`sync_fs(true)`、blockdev sync。
2. `syncfs(fd)`：调用 fd 所在 `MountFS::sync_filesystem()`，并结合 errseq 返回 writeback 错误。
3. `umount()`：PR 中调整为在获取 `umount_write()` 前调用 `sync_filesystem()`。

PR 移除 page reclaim 线程中的 `sync_fs_with_umount_read(true)` 后，后台周期性元数据落盘入口消失。只发生 chmod、utimens、truncate、mtime/size 更新等元数据变更，且应用不调用 `sync()`/`syncfs()`/`fsync()` 时，dirty inode 可能长期滞留内存。这是功能退化。

### page reclaim 线程职责

`kernel/src/mm/page.rs::page_reclaim_thread()` 属于内存回收和 dirty page 写回路径。它定期调用 `PageReclaimer::flush_dirty_pages()` 可以理解为现有 DragonOS 的简化 dirty page writeback。

但将 `sync_fs_with_umount_read(true)` 放在 page reclaim 线程中不合理：

1. 它会阻塞获取 `umount_lock.read()`，容易和 umount/remount 争用。
2. 它把 VFS/filesystem metadata writeback 职责耦合进 mm reclaim。
3. 它使用 `wait=true`，后台周期任务可能执行较重同步并放大延迟。

因此不能简单恢复旧调用。正确修复应补齐 VFS 层后台元数据 writeback，而不是让 page reclaim 承担元数据同步。

### RwSem 支持 try lock

DragonOS `RwSem` 已提供 `try_read()`，不会睡眠，获取失败立即返回 `None`。这为实现 Linux 式“后台 writeback 遇到正在 umount 的 superblock 时跳过本轮”提供了基础。

`SuperBlockState` 当前只暴露阻塞的 `umount_read()`/`umount_write()`。修复需要新增封装方法，避免后台线程直接接触内部 `umount_lock` 字段。

### mount propagation 测试问题

当前新增测试只执行：

```text
unshare(CLONE_NEWNS)
mount(NULL, "/", NULL, MS_REC | MS_SHARED, NULL)
mount("", "/tmp/.../mnt", "ramfs", 0, NULL)
umount("/tmp/.../mnt")
```

`propagate_umount()` 会将触发卸载的 parent mount 预先加入 `processed`。如果 peer group 中只有这个 parent，自然不会进入 peer 循环，也不会调用 `umount_at_peer()`。因此测试即使回退到旧的死锁实现，也可能不复现问题。

## Linux 6.6 对照

### `sync_filesystem()`

Linux `fs/sync.c::sync_filesystem(sb)` 要求调用者持有 `sb->s_umount`，其顺序是：

```text
writeback_inodes_sb(sb, WB_REASON_SYNC)
sync_fs(sb, 0)
sync_blockdev_nowait(sb->s_bdev)
sync_inodes_sb(sb)
sync_fs(sb, 1)
sync_blockdev(sb->s_bdev)
```

DragonOS 的 `MountFS::sync_filesystem()` 在函数内部获取 `umount_read()`，随后按类似顺序执行 page cache sync、`sync_fs(false)`、blockdev nowait、page cache sync、`sync_fs(true)`、blockdev wait。这个 public API 设计不同于 Linux 内部 API，但语义目标一致。

### 后台 writeback

Linux 后台 writeback 不由 page reclaim 线程直接调用 filesystem `sync_fs()`。普通 flusher/workqueue 路径负责周期性 writeback。关键点是 `fs/fs-writeback.c::try_to_writeback_inodes_sb()`：

```text
if (!down_read_trylock(&sb->s_umount))
    return;
__writeback_inodes_sb_nr(...)
up_read(&sb->s_umount);
```

该逻辑说明后台 writeback 不应阻塞正在进行的 umount，也不应持有 read lock 等待 write lock 释放。DragonOS 应匹配这个并发语义：后台元数据 writeback 使用 try-lock，拿不到 `umount_lock.read()` 时跳过本轮。

### umount propagation

Linux `fs/namespace.c::umount_tree()` 先收集待卸载 mount tree，`propagate_umount()` 只扩展卸载列表和处理 mount tree 状态，不在每个 propagated peer 上重新执行 `sync_filesystem()`。superblock 同步属于卸载前或 superblock shutdown 语义，不属于每个 peer detach 的职责。

因此 PR 移除 `umount_at_peer()` 中的 `sync_filesystem()` 是正确方向；需要补充的是后台元数据回写和真实覆盖 propagation peer 路径的测试。

## 二次审查后的修复方案

### 目标

1. 保持 PR 对 umount 自死锁的根因修复：不在持有 `umount_write()` 的 propagation 路径中重入 `sync_filesystem()`。
2. 恢复并规范后台周期性元数据 writeback，避免 ext4 dirty inode 无限期滞留。
3. 将 metadata writeback 从 mm page reclaim 中解耦，放到 VFS/filesystem 层。
4. 后台 writeback 不阻塞 umount，不引入新的锁等待链路。
5. 修正 dunitest，使其真实触发 `propagate_umount()` -> `umount_at_peer()`。

### 文件与模块归属

建议修改文件：

1. `kernel/src/filesystem/vfs/mount.rs`
   - 给 `SuperBlockState` 增加 `try_umount_read()`。
   - 给 `MountFS` 增加 `try_sync_fs_with_umount_read(wait: bool)`。
   - 可选：增加 `try_sync_metadata_with_umount_read()` 作为更语义化的 wrapper。

2. 新增 `kernel/src/filesystem/vfs/writeback.rs`
   - 放置 VFS metadata writeback 后台线程。
   - 使用 `#[unified_init(INITCALL_CORE)]` 注册启动。
   - 遍历 `list_unique_mounted_superblocks()`。
   - 调用 try-lock 型 metadata sync，不阻塞 umount。

3. `kernel/src/filesystem/vfs/mod.rs`
   - 导出 `writeback` 子模块。

4. `kernel/src/mm/page.rs`
   - 保留 `PageReclaimer::flush_dirty_pages()`。
   - 不恢复 `sync_fs_with_umount_read(true)`。
   - 修正文案，明确 metadata writeback 由 VFS writeback thread 负责。

5. `user/apps/tests/dunitest/suites/normal/syncfs_semantics.cc`
   - 修正 propagation umount 测试结构，创建真实 peer mount。
   - 增加卸载前后断言，确保测试不是单 peer 空跑。

### `try_umount_read()` 设计

在 `SuperBlockState` 中新增：

```rust
pub fn try_umount_read(&self) -> Option<RwSemReadGuard<'_, ()>> {
    self.umount_lock.try_read()
}
```

注意点：

1. 需要引入 `RwSemReadGuard` 类型，或使用完整路径返回。
2. 不暴露 `umount_lock` 字段本身，保持封装。
3. 不替换已有阻塞 API；显式 `sync()`/`syncfs()` 仍然需要阻塞等待以提供用户可见同步语义。

### try-lock metadata sync 设计

在 `MountFS` 中新增：

```rust
pub fn try_sync_fs_with_umount_read(&self, wait: bool) -> Result<bool, SystemError> {
    let sb_state = self.super_block_state();
    let Some(_umount_guard) = sb_state.try_umount_read() else {
        return Ok(false);
    };

    if self.is_sb_readonly() {
        return Ok(true);
    }

    if let Err(e) = self.sync_fs(wait) {
        self.record_wb_error(e.clone());
        return Err(e);
    }

    Ok(true)
}
```

返回值语义：

1. `Ok(true)`：本轮已经取得锁并执行完成，或只读 superblock 无需处理。
2. `Ok(false)`：正在 umount/remount 或有 writer/upgrader，后台线程跳过本轮。
3. `Err(e)`：取得锁后 writeback 失败，错误记录到 superblock errseq，后台线程继续处理其他 superblock。

为什么不直接复用 `sync_fs_with_umount_read()`：

1. 该函数阻塞获取 `umount_read()`，会重新制造 umount 争用风险。
2. 后台 writeback 的 Linux 语义是 opportunistic，不是强制等待。
3. 显式同步和后台同步的锁等待策略应区分。

### VFS metadata writeback 线程

新增线程建议命名 `vfs_writeback`。核心循环：

```rust
fn vfs_writeback_thread() -> i32 {
    loop {
        let mounts = list_unique_mounted_superblocks();
        for mount in mounts {
            if let Err(e) = mount.try_sync_fs_with_umount_read(false) {
                log::warn!("vfs_writeback: sync_fs failed: {:?}", e);
            }
        }

        let _ = nanosleep(PosixTimeSpec::new(5, 0));
    }
}
```

关键约束：

1. 使用 `wait=false`。后台周期任务只提交元数据，不承担显式 `sync()` 的等待语义。
2. 不调用 `sync_filesystem()`。完整 sync 会遍历 page cache、sync blockdev，周期性执行成本更高，也与 page dirty writeback 职责重复。
3. 不调用 `sync_inodes_of_mount()`。DragonOS 当前 page dirty writeback 仍由 `PageReclaimer::flush_dirty_pages()` 和显式 sync 处理；本修复只补元数据 dirty inode 周期回写。
4. 不因为单个 superblock 错误退出线程。错误记录进 errseq，并继续下一轮。
5. 使用 `list_unique_mounted_superblocks()`，避免 bind/propgation clone 对同一 `SuperBlockState` 重复执行。
6. 周期可以沿用 5 秒，与 PR 中意图匹配 Linux `dirty_writeback_centisecs` 默认值；后续可再做 sysctl 化，但本修复不引入额外配置面。

### 为什么不是 workaround

该方案不是“为了通过测试而加一个定时 sync”：

1. 它恢复的是 OS 必需的后台元数据 writeback 能力。
2. 它参考 Linux flusher/writeback 的职责分层和 `s_umount` try-lock 语义。
3. 它不改变 ext4 dirty inode 队列语义，不绕过错误，不跳过真实 writeback。
4. 它避免把 VFS 元数据同步继续挂在 mm reclaim 下，修复了架构层职责混杂。

## 测试修复方案

### 当前测试缺口

原测试只把 `/` 标记为 shared，然后在同一个 namespace 中挂载并卸载 child。由于 peer group 里只有触发卸载的 parent 本身，`propagate_umount()` 会把该 parent 预先加入 `processed`，循环中不会进入 `umount_at_peer()`，因此无法覆盖原始死锁路径。

当前实现采用跨 mount namespace 的真实 peer parent：父进程先创建 isolated namespace 和 shared parent mount；子进程 `fork()` 后 `unshare(CLONE_NEWNS)`，DragonOS `copy_mnt_ns()` 会把 shared parent 的 copy 注册到同一 peer group。子进程在自己的 peer parent 下挂载 child，父进程通过 `/proc/self/mountinfo` 验证 child mount 被传播到父 namespace，再放行子进程执行 `umount(child)`。这样卸载会经过 `propagate_umount()` -> `umount_at_peer()`。

### 建议测试步骤

1. 父进程先 `unshare(CLONE_NEWNS)`，进入测试专用 mount namespace。
2. 立即将 `/` 递归改为 private，并检查返回值；失败时清理已创建路径后 `GTEST_SKIP()`，不能继续测试：

```c
mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)
```

3. 创建测试目录：

```text
root/
  parent/
```

4. 在 `parent` 上挂载 ramfs，并创建 `parent/child`：

```c
mount("", parent, "ramfs", 0, NULL)
```

5. 将 `parent` 标记为 shared：

```c
mount(NULL, parent, NULL, MS_SHARED, NULL)
```

6. 创建 pipe 用于父子同步，然后 `fork()`。
7. 子进程执行 `unshare(CLONE_NEWNS)`，触发 `copy_mnt_ns()` 复制 shared parent 并注册到同一 peer group。
8. 子进程在自己的 `parent/child` 上挂载 ramfs，并通过 pipe 通知父进程。
9. 父进程在放行子进程前解析 `/proc/self/mountinfo`，确认 `parent/child` 已传播到父 namespace。仅检查路径存在不够，因为目录本身可能存在。
10. 子进程启动并发 `sync()` 线程，然后调用 `umount(parent/child)`。
11. 父进程等待子进程退出后再次解析 `/proc/self/mountinfo`，确认 propagated child mount 已从父 namespace 移除。

该结构与 DragonOS 当前 propagation 实现匹配：`copy_mnt_ns()` 对 shared mount copy 调用 `register_peer()`，使父子 namespace 中的 parent mount 形成真实 peer group。

### 测试注意点

1. 不应在没有验证 peer propagation 成功时继续执行卸载断言，否则仍可能假通过。
2. 如果 `MS_REC | MS_PRIVATE`、`MS_SHARED` 或 `CLONE_NEWNS` 在当前 DragonOS 环境不支持，应 `GTEST_SKIP()` 并说明缺失能力。
3. 父进程在失败路径上应尽量关闭 pipe fd，并在已 fork 后 `waitpid()` 回收子进程。
4. 子进程失败退出前只通过 pipe 通知父进程，不应长期阻塞在等待 go pipe。
5. 清理阶段要用 `umount2(path, MNT_DETACH)` 尽量回收 `child` 和 `parent` 残留 mount，避免影响后续 dunitest。
6. helper 函数使用 `MountInfoContains(path)` 从 `/proc/self/mountinfo` 判断挂载项，避免依赖目录存在性。

## 并发与生命周期不变量审查

### 不变量 1：后台 writeback 不阻塞 umount

后台线程必须使用 `try_umount_read()`。如果改成阻塞 `umount_read()`，则会在 umount write lock 等待期间制造持续 read-side 干扰，违背 Linux `try_to_writeback_inodes_sb()` 的语义。

### 不变量 2：显式 sync 仍然阻塞等待

`sync()`/`syncfs()`/`umount()` 不应改为 try-lock。用户显式同步语义要求尽量完成数据和元数据落盘，并返回可见错误。try-lock 只适用于后台周期任务。

### 不变量 3：不重复同步同一 superblock

后台线程必须使用 `list_unique_mounted_superblocks()`，而不是遍历 mount namespace 的每个 mount。`deepcopy()` 共享 `SuperBlockState`，bind/propgation clone 可能很多；重复同步同一 superblock 会造成低性能和不必要锁竞争。

### 不变量 4：不直接操作 ext4 dirty queue

后台线程不应读取或清空 `dirty_inodes`。dirty state、`QUEUED`、`WRITEBACK`、requeue 逻辑都封装在 ext4 `flush_dirty_inodes()` 中。跨层直接操作会破坏并发 dirty 更新语义。

### 不变量 5：不在 propagation peer detach 中 sync

`umount_at_peer()` 不应恢复 `child.sync_filesystem()`。顶层 umount 已在获取 write lock 前同步；propagation clone 共享 superblock，同一 superblock 重复同步既低效又会重现死锁。

### 不变量 6：后台错误必须进入 errseq

`try_sync_fs_with_umount_read()` 中如果 `sync_fs()` 返回错误，必须调用 `record_wb_error()`。这样后续 `fsync()`/`syncfs()` 能通过 errseq 看到历史 writeback 错误，保持 PR 中 writeback error reporting 的设计。

### 不变量 7：线程生命周期不依赖 mount 生命周期

后台线程每轮通过 `list_unique_mounted_superblocks()` 获取强 `Arc<MountFS>`。列表中 stale `Weak` 会被清理。线程不持有长期 mount 引用，避免阻止卸载释放。

## 性能审查

1. 每 5 秒遍历 `MOUNTED_SUPERBLOCKS` 是可接受的，因为使用 `list_unique_mounted_superblocks()` 去重，且当前 DragonOS mount 数量通常有限。
2. 不执行完整 `sync_filesystem()`，避免周期性扫描所有 page cache。
3. `sync_fs(false)` 对 ext4 当前实现仍会同步执行 `flush_dirty_inodes()`，但它只处理 dirty inode 队列；相比全局 page cache 扫描更小。
4. 后续可为 `FileSystem::sync_fs(wait=false)` 引入更严格的 nowait 语义，但本次不应扩大改动面。当前先恢复周期性元数据 writeback 的正确职责边界。
5. 如果未来 dirty inode 数量巨大，可以在 ext4 层引入批量上限和延迟 requeue；本 PR 不需要为此引入复杂调度。

## 错误路径审查

1. 后台线程单个 superblock 返回错误时，仅记录并继续；不能 panic 或退出。
2. `try_umount_read()` 返回 `None` 时不是错误，不记录 errseq。
3. 只读 superblock 返回 `Ok(true)`，不执行 `sync_fs()`。
4. `list_unique_mounted_superblocks()` 中 stale `Weak` 清理继续保留。
5. dunitest 中所有中途失败都要尽量清理 mount；对不支持的系统能力使用 `GTEST_SKIP()`，对 propagation 未发生则应 fail，因为这代表测试前提未满足或功能缺失。

## 实施顺序

1. 在 `SuperBlockState` 增加 `try_umount_read()`。
2. 在 `MountFS` 增加 `try_sync_fs_with_umount_read(wait)`。
3. 新增 VFS writeback 线程模块并挂到 initcall。
4. 从 page reclaim 注释中移除“metadata sync 只属于 sync/fsync/umount”的表述，改为“由 VFS metadata writeback thread 负责”。
5. 修正 `SharedMountPropagationUmountNoDeadlock`，确保构造真实 peer mount 并验证 mountinfo。
6. `make fmt`。
7. `make kernel`。
8. 启动 DragonOS，在 guest 中运行：

```sh
cd /opt/tests/dunitest/bin/normal/
./syncfs_semantics
```

9. 结合 Linux 代码再次审查最终实现，重点检查：
   - 后台线程是否只使用 try-lock；
   - 显式 sync 是否仍使用阻塞语义；
   - `umount_at_peer()` 是否没有恢复 sync；
   - 测试是否真的覆盖 peer propagation。

## 残余风险

1. DragonOS 当前 `FileSystem::sync_fs(wait=false)` 对 ext4 仍是同步提交元数据；这比 Linux nowait 更重，但已经比完整 `sync_filesystem()` 小，且属于现有接口语义限制。不要在本 PR 中强行重构 ext4 writeback 调度。
2. mount propagation 的 peer group 语义如果与 Linux 仍有缺口，修正测试时可能暴露额外 propagation bug。该情况应作为真实问题处理，不应放宽测试让它假通过。
3. 后台 writeback 线程增加固定周期任务，可能略增空闲系统唤醒；当前与已有 page reclaim 5 秒周期一致，可接受。

## 最终判断

二次审查后，推荐方案是：保留 PR 对 `umount()` 锁顺序和 `umount_at_peer()` 的死锁修复；新增 VFS 层 try-lock 后台 metadata writeback；修正 dunitest 以构造真实 peer mount。该方案匹配 Linux 的职责划分和 `s_umount` try-lock 思路，不依赖测试特化，不恢复低质量旧路径，也不会把 umount 锁竞争从一个线程转移到另一个线程。
