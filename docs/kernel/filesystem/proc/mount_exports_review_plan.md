# /proc mount exports review and implementation plan

## 背景

PR #1832 已经按净语义 rebase 到 `origin/master`，并推送到 `xboHodx/feat/mountstats`，当前 head 为 `48f53054`。rebase 时保留了 PR 的最终功能语义，去掉了旧分支中多次 merge master 带来的历史噪声；同时把旧路径 `kernel/src/filesystem/vfs/mount.rs` 的必要变更落到当前 master 的 `kernel/src/filesystem/vfs/mount/mod.rs`。

本方案遵循 DragonOS 通用开发要求：

> 先结合Linux代码、问题现象、dragonos代码深入研究，再制定plan；制定后先审查plan是否符合Linux语义、DragonOS架构、并发/生命周期不变量、错误路径和边界条件，确认无workaround、无测试特化、无隐藏坑点后才实施代码变更。
>
> 代码变更后，必须再次结合Linux代码审查DragonOS实现。如果发现语义不一致、架构不合理、边界条件遗漏、并发/生命周期风险或workaround，必须回到plan阶段重新制定修复计划，再继续实施。
>
> 所有方案都要参考Linux代码、dragonos代码、深入研究，并且制定正确、完善、无坑点、无workaround、架构合理、功能正确的实现/根因修复计划。
>
> Linux代码在： ~/code/linux-6.6.139/

## Linux 6.6.139 语义依据

Linux 的实现集中在 `~/code/linux-6.6.139/fs/proc_namespace.c`：

- `mounts_open_common()` 在 open 时通过 `get_proc_task(inode)` 取得 `/proc/<pid>` 绑定的目标 task，而不是按全局 pid 再查一次。
- open 阶段在 `task_lock(task)` 下抓取目标 task 的 `nsproxy->mnt_ns` 和 `fs` root，并对 namespace/root 引用计数，之后由 seq_file 迭代该 namespace 的 mount list。
- `/proc/<pid>/mounts` 使用 `show_vfsmnt()`，`/proc/<pid>/mountinfo` 使用 `show_mountinfo()`，`/proc/<pid>/mountstats` 使用 `show_vfsstat()`，三者共享同一 open/iterator 框架。
- `~/code/linux-6.6.139/fs/proc/base.c` 中 TGID 目录包含 `mounts`、`mountinfo`、`mountstats`，其中 `mountstats` 权限为 `S_IRUSR`；TID 目录只包含 `mounts`、`mountinfo`。
- mount path 输出通过 `seq_path_root(..., &p->root, " \t\n\\")` 按目标进程 root 裁剪路径；超出 root 的 mount 会被跳过。
- `/proc/*/mounts` 和 `mountinfo` 的普通 mount option 与 propagation tag 是分离的：`show_mnt_opts()` 输出 `nosuid,nodev,...`，`show_mountinfo()` 额外输出 `shared:X/master:X/propagate_from:X/unbindable` tagged fields。传播状态不应混入 `/proc/mounts` 的普通 option。
- `show_vfsstat()` 对无设备名 mount 会输出 `no device mounted on ...`；有设备名才输出 `device <name> mounted on ...`。文件系统可选实现 `show_stats()`。

## 当前 DragonOS 代码观察

当前 rebase 后的 PR 代码集中在：

- `kernel/src/filesystem/procfs/mount_view.rs`
- `kernel/src/filesystem/procfs/mounts.rs`
- `kernel/src/filesystem/procfs/pid/mounts.rs`
- `kernel/src/filesystem/procfs/pid/mountinfo.rs`
- `kernel/src/filesystem/procfs/pid/mountstats.rs`
- `kernel/src/filesystem/vfs/mod.rs`
- `kernel/src/filesystem/vfs/mount/mod.rs`

积极点：

- `/proc/<pid>/mounts` 和 `/proc/<pid>/mountinfo` 已经使用 `ProcPidTarget`，不会再通过全局 raw pid 查找目标进程；这与 Linux 的 proc inode 绑定目标 task 思路一致。
- `ProcFile::open()` 已经统一初始化 `FilePrivateData::Procfs`，保留了 `lseek(SEEK_END)` 返回 `EINVAL` 的 procfs 行为。
- `FileSystem` trait 中新增了 `proc_show_devname/proc_show_mount_options/proc_show_mountinfo_root/proc_show_mount_stats` 扩展点，方向上对应 Linux `super_operations` 的 `show_devname/show_options/show_path/show_stats`。

需要修复的问题：

- `/proc/<pid>/mountstats` 当前使用 `S_IRUGO` 且没有动态 owner，违反 Linux `S_IRUSR` 语义，并会放宽跨用户读取。
- `mount_view.rs` 仍是函数堆叠式渲染，缺少清晰的“目标视图、挂载条目、格式策略、缓存读”抽象边界。
- `read_cached_mount_file()` 在私有数据类型不匹配时返回 `EINVAL`，而原有 procfs helper 返回 `EIO`；应统一错误语义。
- `render_mountstats_line()` 对没有 device name 的挂载总是输出 `device <fstype>`，与 Linux 的 `no device` 分支不一致。
- `collect_visible_mounts()` 直接使用 `nsproxy.mnt_ns` 字段，应该使用现有访问器 `mnt_namespace()`，避免破坏 `NsProxy` 封装。
- mount option 目前通过 `proc_mount_options_string()` 避免 propagation flag 混入普通选项，这点合理；但应在代码中以命名和注释明确“普通选项”和 mountinfo tagged fields 的职责分离，回应 reviewer 对删除传播选项的疑问。
- `MountList::clone_inner()` 只返回同一路径 mount stack 的 top mount，会丢失下层 stacked mounts；Linux mountinfo 遍历 namespace mount list，不应只展示 top mount。
- pid mount 文件 read 阶段再次查目标进程，和 Linux open 后持有 namespace/root 引用、read 不依赖 task 存活的生命周期语义不一致。
- mountinfo `-` 前后的 option 都复用同一个渲染函数，混淆了 per-mount options 与 superblock/fs-specific options。
- root path 解析失败时 fallback 到 `/`，会把目标 root 裁剪失败扩大成整个 mount namespace 视图，存在隐藏的信息泄露风险。

## 实施计划

1. 收敛目标进程和权限语义。

   - 给 `ProcPidTarget` 增加 `owner_uid_gid()` 或等价 helper，内部复用 `thread_group_leader()`，kthread 返回 `(0, 0)`，普通进程返回目标凭据 `euid/egid`。
   - `PidDirOps::owner()`、`PidMountsFileOps::owner()`、`MountInfoFileOps::owner()`、`MountStatsFileOps::owner()` 复用该 helper，避免 owner 逻辑散落。
   - `MountStatsFileOps::new_inode()` 改为 `InodeMode::S_IRUSR`。
   - 保持 `mounts/mountinfo` 为 `S_IRUGO`，与 Linux 一致。

2. 重构 `mount_view.rs` 的抽象边界。

   - 保留单文件模块，但按职责重组为：
     - `ProcMountView`：从目标 task 抓取 mount namespace 与 root，负责收集可见 mount 条目。
     - `ProcMountEntry`：保存 `Arc<MountFS>`、显示用 mountpoint、parent mount id。
     - `ProcMountFormatter` 或 `ProcMountRenderKind::render_entry()`：封装 mounts、mountinfo、mountstats 三种格式。
     - `open_current_mount_file/open_mount_file_for_target/read_cached_mount_file`：作为 proc file ops 调用的薄入口。
   - 不引入过度抽象，不拆成多层 trait；这里的变化只为隔离“收集”和“格式化”，提升可读性并保留低耦合。
   - `MountList` 新增只读快照 API，返回所有 `(mount_path, MountFS)` records，而不是复用只返回 top mount 的 `clone_inner()`。

3. 对齐 Linux 输出细节。

   - 拆分 option 渲染：`/proc/*/mounts` 输出 `ro/rw`、superblock options、per-mount options、fs-specific show_options；`mountinfo` 的 `-` 前只输出 per-mount `ro/rw` 与 per-mount options；`mountinfo` 的 `-` 后输出 superblock `ro/rw`、superblock options 与 fs-specific show_options。
   - propagation 仅由 mountinfo tagged fields 输出，不混入 `/proc/*/mounts` 的普通 option。
   - `mountstats` 的 device 字段改为显式区分 `Some(devname)` 和 `None`：有 source/devname 时输出 `device <dev>`，否则输出 `no device`。
   - 调整 `FileSystem::proc_show_devname()` 的默认语义：若 `mount.mount_source()` 存在，返回 devname；否则返回 `None`，由 mountstats 决定 `no device`，由 mounts/mountinfo 按 Linux 的 fallback 输出 `none`。
   - 路径 escaping 保持 `/proc/mounts` 和 mountinfo path 不转义 `#`，type/source 转义 `#`；这个与 Linux `mangle()` 和 `seq_path_root()` 的 escape 集一致。
   - root path 解析失败不再 fallback 到 `/`；应返回错误，避免把裁剪失败扩大为完整 namespace 视图。
   - `propagate_from` 需要 root-aware 语义。当前 DragonOS propagation 模型暂未完整暴露 dominating peer id，本次至少不能把 propagation 放错位置；若无法正确实现 `propagate_from`，保留清晰 TODO，后续在 mount propagation 模型中补齐，而不是伪造字段。

4. 生命周期和并发边界。

   - open 时生成缓存内容，保持 DragonOS 当前 procfs 的一次性内容模型，不在本 PR 引入 Linux seq_file cursor/poll 机制，以免扩大范围；这是与 Linux seq_file poll/cursor 的已知差异。
   - 生成内容前只持有必要的 `Arc`，遍历 mount list 使用“所有 mount record 快照”；不在字符串渲染期间持有 mount namespace 内部锁。
   - 目标进程退出时：open 阶段若 `thread_group_leader()` 不存在返回 `ESRCH`；open 成功后 read 只读缓存，不再复查目标进程。
   - DragonOS 当前 `nsproxy` 与 `fs_struct` 分别受不同锁保护；应增加一致快照 helper 或在实现中至少不扩大竞态窗口。若无法在本 PR 内重构进程锁模型，必须记录为残余差异，不能声称完全等同 Linux 的 `task_lock()` 临界区。

5. 测试计划。

   - 扩展 `user/apps/tests/dunitest/suites/normal/proc_mount_exports.cc`：
     - 检查 `/proc/self/mountstats` 可读；mountstats 统计按 entry 起始行计数，即以 `device ` 或 `no device` 开头的行，不强行假设未来 fs-specific stats 只能单行。
     - 检查目标进程 mount namespace/root 视角，确保 `/proc/<pid>/mounts|mountinfo|mountstats` 都使用目标视图。
     - 新增 `stat("/proc/self/mounts|mountinfo|mountstats")` owner 断言：三者 owner 与目标进程一致；`mountstats` mode 为 owner read only，即 `0400` 有效，group/other read 不应出现。
     - 若 dunitest 环境可切换 uid，则补充非 owner 打开其他进程 `mountstats` 返回 `EACCES/EPERM`；若当前 DragonOS 缺少完整多用户权限路径，则至少覆盖 metadata mode/owner，避免测试伪造。
   - 运行 `make fmt` 和 `make kernel`。
   - 在 QEMU 中运行 `/opt/tests/dunitest/bin/normal/proc_mount_exports_test` 验证用户可见行为。

## 回归风险与规避

- 不修改 VFS mount 树结构和挂载传播算法，只修改 procfs 输出层，避免影响 mount/move/propagation 语义。
- 不删除 `options_string()`，新增 `proc_mount_options_string()` 并让旧入口委托它，避免破坏已有调用点。
- 不改变 `/proc/<pid>/mounts`、`/proc/<pid>/mountinfo` 权限，只收紧 `mountstats`。
- 不将 reviewer 的权限问题用打开时手写 credential check workaround 解决，而是按 Linux inode mode + 动态 owner 的模型实现。

## 子 agent 评审待确认点

- `FileSystem::proc_show_devname()` 返回 `Option<String>` 是否比当前直接写 `fmt::Write` 更适合表达 Linux 的 “有 show_devname / 无 show_devname” 分支。
- `ProcPidTarget::owner_uid_gid()` 放在 `procfs/pid/mod.rs` 是否合适，还是应保留为 `PidDirOps` 私有 helper 并在 mountstats 内局部实现。
- `mountstats` 只在 TGID `/proc/<pid>`。已确认当前 DragonOS `/proc/<pid>/task/<tid>` 未复用 `PidDirOps`，且当前 tid 目录未实现 Linux 的 `mounts/mountinfo`；本 PR 不新增 tid mount 文件，作为后续兼容性工作。
- open 时缓存内容是否足够符合 DragonOS 当前 procfs 架构；Linux seq_file poll/cursor 不应在本次修复中扩展，但需要确认不会被误认为语义漏洞。
