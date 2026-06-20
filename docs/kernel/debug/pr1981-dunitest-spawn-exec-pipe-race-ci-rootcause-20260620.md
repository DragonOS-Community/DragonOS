# PR 1981 dunitest spawn_exec_pipe_race CI timeout 根因分析

## 结论

PR #1981 的 dunitest CI 失败不是 workflow timeout 太短，也不是全局内核死锁。
CI job `27741936648/82070969251` 中唯一超时项是
`normal/spawn_exec_pipe_race`，runner 在 60s 后杀掉该测试并继续执行后续用例，
说明系统仍可调度，故障边界集中在该测试反复 `fork + execve` 的热路径。

最终根因是 DragonOS FAT 缺少 Linux VFS dcache 等价的负向目录项缓存。
CI 使用 `ROOTFS_MANIFEST=default` 和 vfat rootfs，rootfs 中没有
`/etc/ld.so.cache`。动态链接器每次执行测试二进制的 `--gtest_help` 路径时，
都会探测多组不存在的 glibc-hwcaps/tls 库路径。Linux 通过 dcache 中的 negative
dentry 把这些 ENOENT miss 缓存住；DragonOS FAT 原先只缓存正向 `children`，
每次 miss 都重新线性扫描目录，并且目录项读取按 32B entry 反复触发 512B block
读，512 次 exec 将这个慢路径放大到 CI 60s timeout。

这不是通过扩大 timeout、跳过用例、改 `.github/workflows/dunitest.yml` 或生成
`ld.so.cache` 绕过问题。修复点应在内核 FAT/VFS 行为上：缓存已确认不存在的
目录项，并在所有会改变目录命名空间的操作中保守失效。

## 失败现场

GitHub Actions 日志关键片段：

```text
[RUNNER] START: normal/spawn_exec_pipe_race
[ RUN      ] SpawnExecPipeRace.GtestHelpSpawnAfterSiblingExecStress
[RUNNER] END: normal/spawn_exec_pipe_race status=TIMEOUT duration_ms=60028
```

CI 汇总里只有 1 个 timeout，后续 dunitest 正常运行并汇总。因此如果存在竞态，
它也不是把整个内核拖入不可调度状态的死锁，而应表现为单个测试进程内某条
exec/pipe/waitpid 路径极慢、等待或引用未释放。

## 测例行为

`user/apps/tests/dunitest/suites/normal/spawn_exec_pipe_race.cc` 做两类压力：

1. 循环 512 次启动自身并执行 `--gtest_help`，父进程使用 stdout/stderr pipe、
   `FD_CLOEXEC` error pipe、`poll(10ms)` drain 和 `waitpid(WNOHANG)` 收割。
2. 每 16 次触发一次兄弟线程 `execve(/proc/self/exe, --spawn-exec-pipe-race-exec-exit)`，
   覆盖 Linux `de_thread()` 语义：exec 成功后旧线程组应被清理，进程退出码为 0。

这类用例对并发正确性很敏感，但本次复现显示时间主要消耗在每轮 exec 的动态链接
路径查找，而不是 pipe/poll 本身。

## 被证伪的方向

### 不是 poll timeout 单位错误

DragonOS `HZ=250`，`next_n_us_timer_jiffies(10000)` 对应约 3 个 jiffies，即约
12ms，不会把测试中的 `poll(..., 10)` 稳定放大成 1s。

### 不是 pipe release 唤醒语义差异

Linux 6.6 `fs/pipe.c::pipe_poll()` 是先 `poll_wait()` 注册再检查 pipe 状态，
避免检查后睡眠的 TOCTOU；`pipe_release()` 也只在 reader/writer 侧状态发生
0/非0 转换时唤醒等待方。DragonOS 当前 epoll add 路径同样是先挂 epitem 再
poll 初始状态，pipe release 条件唤醒也不是这次 timeout 的主要差异。

### 不是全局 reservation wait 丢唤醒

`AddressSpace::read_guard_no_reservation_conflict()` 等 helper 的目的，是在 mmap
两阶段操作存在 reservation 时等待冲突区间消失，并在同一个 wait 条件中返回
地址空间 guard，避免拿到即将失效的 VMA 视图。底层 `WaitQueue::wait_until`
先注册 waiter 再检查条件，reservation commit/cancel 后 `wake_all()`，没有发现
能解释该用例 1s/iter 慢路径的丢唤醒证据。

## Linux 对照

Linux VFS 查找路径会复用 dcache。不存在的路径也会以 negative dentry 的形式被
缓存；后续同名 lookup 能在 `lookup_fast()`/`d_lookup()` 层直接得到 ENOENT，
不需要每次进入具体文件系统扫描目录。

这解释了为什么 Linux host 上同一个测试只需数秒，而 DragonOS vfat rootfs
会被动态链接器的重复 miss 放大：

```text
host: spawn_exec_pipe_race_test --gtest_help 约 0.002s
host: 整个 spawn_exec_pipe_race 测试约 3.9s
```

在 host 上用 `LD_DEBUG=libs` 并禁用 ld.so cache 可以看到动态链接器会探测
`glibc-hwcaps/x86-64-v3`、`glibc-hwcaps/x86-64-v2`、`tls` 等不存在目录。Linux
能承受这种探测，是因为负向 dentry 和目录缓存把 miss 成本压低了。

## DragonOS 根因

DragonOS FAT 的 `FATInode` 原先只有：

```text
children: HashMap<String, Arc<LockedFATInode>>
```

它只缓存已经找到的子 inode。`find(name)` 在 `children` miss 后总是调用
`FATDir::find_entry()` 重新扫描磁盘目录；若返回 ENOENT，下次同名 lookup 仍会
再次扫描。对动态链接器而言，不存在的 hwcaps/tls 路径在每轮 exec 中高度重复，
于是 default/vfat 下产生稳定慢路径。

本地 default/vfat 复现中，临时低频进度采样显示每 16 轮约 15-16s，512 轮总耗时
约 495s，CI 的 60s timeout 正是这个慢路径的前 60s 截断，而不是一次随机死锁。

## 修复设计

### FAT 负向目录项缓存

在 `FATInode` 中新增 `negative_children: LruCache<String, ()>`，key 与 `children`
保持同样的 FAT 大小写折叠规则，并为每个目录设置固定容量上限。`find(name)` 的
不变量为：

1. 正向缓存优先。若 `children` 命中，直接返回 inode。
2. 负向缓存次之。若 `negative_children` 命中，直接返回 ENOENT。
3. 两者都未命中时才扫描 FAT 目录；扫描确认 ENOENT 后写入有界负向缓存。
4. 扫描找到或创建出正向 inode 后，插入 `children` 并移除同名负向项。

所有更新都在父目录 `FATInode` mutex 下完成，不引入新的锁层级。选择有界 LRU
而不是无界集合，是为了避免任意进程在同一 FAT 目录下 lookup 大量随机不存在文件名
造成内核堆不可回收增长；这对应 Linux negative dentry 可被 shrinker 回收的性质。

### 失效规则

为避免 stale ENOENT，所有会改变目录项名称可见性的成功路径都必须失效或更新。
FAT 还存在 8.3 短名 alias 与长名大小写折叠问题：创建 `long-name.so` 可能同时让
另一个短名形式变为可见。因此不能只失效“传入的那个字符串”，而应在目录命名空间
发生变化后清空该目录负缓存：

- `create(File/Dir/Socket)`：磁盘创建成功后清空父目录 negative，再 `find()` 建立 positive。
- `mknod()`：插入特殊 inode 前清空父目录 negative。
- `list()`：目录扫描发现真实 entry 时移除同名 negative，并建立 positive。
- 同目录 `rename()`：清空父目录 negative，移除 old positive，old 记为 negative，new 插入 positive。
- 同目录 case-only `rename()`：FAT 查找大小写不敏感，`foo -> FOO` 不能把源项误当成
  已存在的替换目标；当前实现先确认源项存在，再作为 no-op 返回成功，避免误删源文件。
- 跨目录 `rename()`：源目录和目标目录分别清空 negative；源目录 old 记为 negative，
  目标目录 new 插入 positive。
- `unlink()`/`rmdir()`：只有磁盘删除成功后才 `mark_child_absent()`；该 helper 会先
  清空父目录 negative，再把被删除的名字记为 negative；失败不缓存 ENOENT。

这套规则让 negative cache 只表达“在该父目录锁保护下，最近一次磁盘扫描确认不存在”，
不会把创建或 rename 后的新文件挡在 ENOENT 后面。

### 撤回的辅助优化

子 agent 审查指出两类非必要高风险辅助改动：

- FAT file-cluster cache 若用无界 `Vec` 保存完整簇链前缀，会随大文件读取长期增长；
  这不是 CI timeout 根因必需，已撤回。
- ELF 只读 PT_LOAD file-backed lazy mapping 在 DragonOS 尚未实现 Linux
  `deny_write_access`/`ETXTBSY` 保护时可能让执行镜像受后续文件写入影响。
- `clear_child_tid` 若在 exec 加载完全成功前清除，失败回滚到旧 image 时会丢失
  `CLONE_CHILD_CLEARTID` 退出唤醒语义。

这些改动已撤回，不纳入本轮提交；本轮只提交与 CI 根因直接相关且可验证的 FAT
negative lookup cache 修复。

## 并发和安全性分析

负向缓存不新增跨 inode 全局状态，也不引入 lock-free 读路径。它复用已有
`FATInode` mutex，因此 `children` 与 `negative_children` 的互斥关系在同一临界区内
维护。正向缓存优先可以避免同名 positive 与 stale negative 同时存在时误报 ENOENT；
有界 LRU 避免无界内存增长；创建、rename、mknod、unlink、rmdir 成功改变目录
命名空间后清空该目录负缓存，覆盖 FAT 8.3 短名 alias、大小写折叠和覆盖 rename
等会让多个 lookup 字符串同时改变可见性的情况。

对 TOCTOU 的关键点是：不能在“检查 negative”之后释放父目录锁再做命名空间更新。
当前实现中 `find()`、cache update 和 FAT 目录操作调用都由同一个 `FATInode` guard
串行化；失败路径只有在 ENOENT 被磁盘扫描确认后才写入 negative，删除失败不会写入。

reservation guard 相关 helper 不用于绕过 rwsem 写者等待，也不再使用此前被质疑的
`read_for_file_rmap` 或 `read_bypass_writer_waiters` 风格接口。它们的职责是等待 mmap
reservation 与目标 region 无冲突后返回 guard；等待队列先注册后检查条件，降低丢唤醒
风险。

子 agent 还指出 FAT 跨目录 rename 原有实现按“源目录再目标目录”的方向加锁，两个线程
并发执行 A->B 与 B->A 时存在 ABBA 死锁风险。这是既有锁顺序缺陷，不是 negative cache
引入的新锁层级；本轮不把 CI timeout 根因修复扩成高风险 rename 锁协议重构，应单独
立项用稳定 inode id 或 try-lock 重试重写双目录锁顺序。

## 验证结果

已完成验证：

```text
make fmt
make kernel
```

结果：通过；`make fmt` 内含 kernel clippy。

CI 等价 default/vfat 目标测试：

```text
FORCE_UNSAFE_CONFIGURE=1 ROOTFS_MANIFEST=default \
  DUNITEST_PATTERN=normal/spawn_exec_pipe_race make test-dunit
```

修复后结果：

```text
[       OK ] SpawnExecPipeRace.GtestHelpSpawnAfterSiblingExecStress (7861 ms)
[RUNNER] END: normal/spawn_exec_pipe_race status=PASSED duration_ms=7891
通过: 1 失败: 0 超时: 0
```

修复前同环境表现为约 1s/iter、512 轮约 495s，CI 60s timeout。

ubuntu2404/ext4 对照：

```text
DUNITEST_PATTERN=normal/spawn_exec_pipe_race make test-dunit
```

结果：

```text
[       OK ] SpawnExecPipeRace.GtestHelpSpawnAfterSiblingExecStress (8773 ms)
[RUNNER] END: normal/spawn_exec_pipe_race status=PASSED duration_ms=8829
通过: 1 失败: 0 超时: 0
```

`.github/workflows/dunitest.yml` 和测例文件均未修改。

## 子 agent 审查结论

已使用 `bug-hunter` 多角色审查当前 patch 和本文档，重点检查：

- 架构：negative cache 是否属于 FAT inode 职责，是否应抽到 VFS dcache。
- 安全性：是否可能产生 stale ENOENT、权限绕过、路径可见性错误。
- 并发正确性：锁顺序、rename/unlink/create 与 find 的 TOCTOU、reservation wait。
- Linux 语义：pipe/poll/exec/de_thread/FAT lookup/filemap fault 是否与 Linux 6.6 相符。
- 是否 workaround：是否通过 timeout、rootfs、workflow 或测试绕过问题。

已采纳并处理的意见：

- 无界 `HashSet` negative cache 可能造成内核内存 DoS：已改为每目录有界 LRU。
- FAT 8.3 短名 alias 可能让“按同名失效”的 negative cache 变陈旧：已改为目录命名
  空间成功变更时清空该目录负缓存。
- 同目录 case-only rename 会把源误判成替换目标：已加保护，避免在 FAT 大小写不敏感
  语义下误删源文件。
- FAT file-cluster cache、ELF file-backed exec mapping、filemap readahead、过早清理
  `clear_child_tid` 均不是本次根因必需，且存在语义或生命周期风险：已撤回。
- 跨目录 rename ABBA 死锁是既有独立缺陷：本文记录风险，但不作为本次 CI 根因修复范围。

审查后剩余 patch 聚焦 FAT negative lookup cache，没有 workflow、timeout、rootfs 或
测试绕过。

最终 staged 版本又经安全/并发和架构/Linux 语义两个只读子 agent 复审：未发现阻塞
提交的问题；确认该 patch 没有新增 lock-free 读路径、没有新增锁层级，negative cache
容量有界，目录命名空间变更清空负缓存能够覆盖 FAT 8.3 alias 和大小写折叠导致的
stale ENOENT 风险。
