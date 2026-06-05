# PR #1961 cgroup v2 review 与模块拆分实施计划

## 任务边界

本计划针对 PR #1961 的两个目标：

1. 处理 review comment `discussion_r3361813521` 关于 root cgroup controller 文件暴露的意见。
2. 将 `kernel/src/filesystem/cgroup2/mod.rs` 按职责拆分，降低单文件体积，并让对象/函数职责更接近 Linux cgroup v2 的组织方式。

本次不引入非 P1 controller，不伪造 legacy v1 controller，不把未实现资源统计伪装成真实统计。P1 可以 no-op 的文件必须明确保持“保存配置/返回零统计”的语义。

## Linux 6.6 参考结论

参考 Linux 6.6.139：

- `kernel/cgroup/cgroup.c` 使用 `struct cftype` 描述 cgroup 文件，并通过 `cgroup_addrm_files()` 根据 `CFTYPE_NOT_ON_ROOT`、`CFTYPE_ONLY_ON_ROOT` 等 flag 判断文件是否出现在 root cgroup。
- default hierarchy 的基础文件在 `cgroup_base_files[]` 中声明。root 可见基础文件包括 `cgroup.procs`、`cgroup.controllers`、`cgroup.subtree_control`、`cgroup.stat` 等；`cgroup.type`、`cgroup.events`、`cgroup.freeze`、`cgroup.kill` 带 `CFTYPE_NOT_ON_ROOT`。
- `kernel/sched/core.c` 的 `cpu_files[]` 中，`cpu.weight`、`cpu.max` 等 controller 配置文件均带 `CFTYPE_NOT_ON_ROOT`。root 的 `cpu.stat` 来自 `cgroup_base_files[]`，不是来自 `cpu_files[]`。
- `mm/memcontrol.c` 的 `memory_files[]` 中，`memory.current`、`memory.min`、`memory.low`、`memory.high`、`memory.max`、`memory.events` 带 `CFTYPE_NOT_ON_ROOT`；`memory.stat` 没有该 flag，因此 root 可见。
- `kernel/cgroup/pids.c` 的 `pids.max`、`pids.current`、`pids.events` 均带 `CFTYPE_NOT_ON_ROOT`，root 不可见。
- 宿主机 cgroup v2 根目录的实际文件也与上述源码一致：root 有 `cpu.stat`、`memory.stat`，没有 `cpu.max`、`cpu.weight`、`memory.max`、`memory.current`、`pids.max`。

因此，review comment 中“Linux cgroup v2 root 同样暴露 `cpu.max`、`cpu.weight`、`memory.max`、`pids.max` 等接口”的判断不符合 Linux 6.6。正确处理不是把这些文件加到 root，而是把 DragonOS 的 root/non-root 可见性显式建模，并补测试防止误改。

## 子 agent 评审采纳项

方案输出后已启动子 agent 做独立评审。评审意见中确认为真实问题并采纳的点如下：

- `cpu.stat` 应按 Linux 6.6 建模为 cgroup base file，而不是 cpu controller file；否则禁用 cpu controller 时容易错误删除 non-root `cpu.stat`。
- 文件同步应基于完整 desired file set，而不是只基于 controller 文件表；这样 root/non-root core file、controller file、always-visible stat file 由统一规则维护。
- `cgroup2_init()` 保留在 `mod.rs` 更合理，避免 mount 模块承担 sysfs 默认挂载职责。
- `IndexNode::resize(0)` 必须继续作为 cgroup2 伪文件 no-op，保持 shell 重定向和 `O_TRUNC` 写路径兼容。
- 回归测试需要覆盖 root `subtree_control` 启用前后的 root 文件可见性，以及 non-root `cpu.stat` 在 cpu controller 启用前和禁用后的持续存在。

## DragonOS 当前问题

`kernel/src/filesystem/cgroup2/mod.rs` 当前约 1500 行，混合了以下职责：

- cgroup2 文件系统 mount/metadata。
- inode 类型、目录缓存、lookup/list/create/rmdir/unlink。
- cgroup 文件规格表、controller 可用性和文件可见性。
- 文本格式编码/解析。
- `cgroup.procs` 迁移权限与任务移动。
- `cgroup.subtree_control` 解析、验证、提交和子目录文件同步。
- cpu/memory/pids/freezer P1 文件读写。

这导致两个问题：

- `desired_controller_specs()` 对 root 使用 `ROOT_CONTROLLER_FILE_SPECS` 特判，语义正确但缺少 Linux 风格的 visibility flag，容易被误读成遗漏 controller 文件。
- 文件职责不清，后续继续补 cgroup v2 文件时会让 `mod.rs` 继续膨胀。

## 目标架构

将 `kernel/src/filesystem/cgroup2/mod.rs` 拆分成目录模块：

```text
kernel/src/filesystem/cgroup2/
├── mod.rs
├── files.rs
├── inode.rs
└── mount.rs
```

### `mod.rs`

职责：

- 声明子模块。
- 保存 cgroup2 全局常量，例如 max name length、block size、available/domain controllers。
- re-export 当前模块内部需要共享的类型。
- 提供对外函数：
  - `cgroup2_check_attach_permissions()`
  - `cgroup2_inode_to_node()`
- 调用 `register_mountable_fs!`。

### `mount.rs`

职责：

- `Cgroup2Fs`
- `Cgroup2MountData`
- `FileSystemMakerData`
- `FileSystem`
- `MountableFileSystem`
- `Cgroup2Fs::new()` 和 `nsdelegate()`。

mount 相关代码不应知道具体 controller 文件表，也不应直接处理文件读写。

### `cgroup2_init()`

`cgroup2_init()` 保留在 `mod.rs`，负责 sysfs mount point 和默认挂载。`mod.rs` 通过 `pub(super) use mount::Cgroup2Fs;` 给 `register_mountable_fs!` 使用；内部类型使用 `pub(super)`，不把 `Cgroup2Fs`、`Cgroup2Inode`、`CgroupCoreFile`、`CgroupFileSpec` 泄露到 `filesystem` 外。

### `files.rs`

职责：

- `CgroupCoreFile`
- `CgroupFileSpec`
- Linux 风格 root visibility：
  - `CgroupFileVisibility::All`
  - `CgroupFileVisibility::NotOnRoot`
- P1 文件规格表：
  - base files
  - non-root core files
  - cpu files
  - memory files
  - pids files
- `available_controllers_for()`
- `desired_file_specs()` / `desired_controller_specs()`。
- 编码/解析工具：
  - controller list
  - `max`/整数
  - `cpu.max`
  - pids max
  - zero-stat text
- controller P1 文件读写：
  - cpu/memory/pids/freezer 的配置读写。
  - 只读统计文件返回 0/default。

设计原则：

- root 文件可见性必须从同一份 `CgroupFileSpec` 通过 visibility 计算得出，避免 `ROOT_CONTROLLER_FILE_SPECS` 这种容易产生误解的独立表。
- `cpu.stat` 按 Linux 6.6 建模为 base/all-cgroup 文件，不归入 cpu controller 文件表；禁用 cpu controller 后 non-root `cpu.stat` 仍应存在。
- P1 no-op 文件只保存配置或返回零统计；注释说明暂未接入调度/内存统计，不写私有路径、网络地址或测试机信息。
- `files.rs` 不处理 task migration、不访问 inode parent、不操作目录缓存。

### `inode.rs`

职责：

- `Cgroup2Inode`
- `Cgroup2InodeInner`
- `Cgroup2InodeKind`
- inode metadata 构造。
- 目录缓存维护：
  - stale dir prune
  - controller file sync
  - cached child sync
  - lookup/list/create/rmdir/unlink
- `IndexNode` 实现。
- 需要 inode/fs 上下文的写入：
  - `cgroup.procs`
  - `cgroup.subtree_control`
- 将 cpu/memory/pids/freezer 等普通 controller 文件读写委托给 `files.rs`。

`write_at()` 继续避免持有 inode inner lock 跨任务迁移或权限检查，保留当前防自死锁约束。
`IndexNode::resize(0)` 必须保留为 cgroup2 伪文件 no-op；DragonOS open path 会在 shell 重定向或 Rust `File::create()` 的真正 `write()` 前先执行 `resize(0)`。

## 审查意见处理方案

1. 不将 `cpu.max`、`cpu.weight`、`memory.current`、`memory.max`、`pids.max` 加入 root cgroup。
2. 删除 `ROOT_CONTROLLER_FILE_SPECS` 特判，改为完整 desired file set：
   - `cpu.stat` 作为 base/root-visible P1 stat 文件保留 root 可见。
   - `memory.stat` 作为 memory controller 文件但 visibility 为 `All`，root 和已启用 memory 的 non-root 均可见。
   - `cpu.weight`、`cpu.max`、memory limit/current/event、swap、pids 文件 visibility 为 `NotOnRoot`。
   - `inode.rs` 用 `files::desired_file_specs(cgroup)` 计算完整期望文件集合；prune 只删除“受 cgroup2 文件表管理但不在 desired set 中”的文件，而不是只按 `controller()` 删除。
3. 在 dunitest 中补充 root 可见性断言：
   - root 有 `cpu.stat`、`memory.stat`。
   - root 没有 `cpu.max`、`cpu.weight`、`memory.current`、`memory.max`、`pids.current`、`pids.max`。
4. 对 GitHub review comment 回复说明 Linux 6.6 证据，并指出代码已通过 visibility flag 和测试固定该语义。

## 实施顺序

1. 新建 `files.rs`，迁移 `CgroupCoreFile`、`CgroupFileSpec`、controller 列表、文件规格、解析/编码、普通 controller 文件读写。
2. 新建 `mount.rs`，迁移 `Cgroup2Fs`、mount data 和 fs trait impl。
3. 新建 `inode.rs`，迁移 inode 数据结构、目录缓存、`IndexNode` impl、`cgroup.procs` 与 `subtree_control` 写入。
4. 将 `mod.rs` 缩减为模块组织、常量、公共入口函数和 register。
5. 用 visibility 统一 root/non-root 文件选择，移除 `ROOT_CONTROLLER_FILE_SPECS`。
6. 扩展 dunitest root 文件可见性断言。
7. `make fmt`、`make kernel`、`make -C user/apps/tests/dunitest build-suites`。
8. 启动 DragonOS guest，运行新版 `sysfs_cgroup2_mount_test`，并手工确认 root 文件列表关键项。
9. 提交到 PR 分支，推送并回复 review comment。

## 回归风险与防护

- 风险：模块拆分后 private 可见性错误导致编译失败。
  - 防护：仅使用 `pub(super)` 暴露内部必要类型；不把 cgroup2 inode 类型公开到 filesystem 外。
- 风险：root 文件可见性被误改。
  - 防护：dunitest 明确断言 root 不存在 `cpu.max`、`memory.max`、`pids.max` 等 Linux `CFTYPE_NOT_ON_ROOT` 文件。
- 风险：`subtree_control` 更新后已缓存子目录 controller 文件不同步。
  - 防护：`inode.rs` 保留 `sync_cached_child_controller_files()`，测试继续覆盖禁用 controller 后 child 文件消失。
- 风险：锁顺序反转。
  - 防护：禁止任何路径持有 inode inner lock 后再获取 `cgroup_accounting_lock()`；`cgroup.procs` 继续在释放 inode lock 后进入 accounting lock；`subtree_control` 在 accounting lock 下只做提交和子目录同步，不能引入反向锁路径。
- 风险：`O_TRUNC` 伪文件写路径回退。
  - 防护：`inode.rs` 保留 `resize(0) -> Ok(())`，dunitest 的写 helper 保持 `O_WRONLY | O_TRUNC`。
- 风险：普通 controller 写入与 `cgroup.procs` 迁移混用锁导致死锁。
  - 防护：只有 `cgroup.procs` 和 `subtree_control` 在 `inode.rs` 中执行需要 fs/inode 上下文的操作；普通文件委托给 `files.rs`，不持有 inode inner lock。

## 验证计划

- 静态：
  - `git diff --check`
  - 搜索代码和 PR 文案不包含私有环境路径、私有网络地址、临时测试机信息。
- 构建：
  - `make fmt`
  - `make kernel`
  - `make -C user/apps/tests/dunitest build-suites`
- guest：
  - 刷新镜像后运行 `/opt/tests/dunitest/bin/normal/sysfs_cgroup2_mount_test`。
  - 若镜像中的 dunitest 未更新，则通过临时传输运行 host 构建出的新版测试二进制，并明确记录。
  - 手工确认：
    - root: `cgroup.controllers` 包含 `cpu memory pids`
    - root: 启用 `subtree_control` 前后 `cpu.stat`、`memory.stat` 均存在
    - root: 启用 `subtree_control` 前后 `cpu.max`、`cpu.weight`、`memory.current`、`memory.max`、`pids.current`、`pids.max` 均不存在
    - non-root 在父级 `subtree_control` 启用后出现 P1 controller 文件
    - non-root 未启用 cpu 前和禁用 cpu 后 `cpu.stat` 均存在，但 `cpu.max` 不存在
