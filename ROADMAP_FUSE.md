# DragonOS FUSE 支持路线图（对齐 Linux 6.6 语义，分 PR/可测/可演示）

> 目标：让 DragonOS 具备**可用、可演示**的 FUSE（Filesystem in Userspace）能力，并逐步对齐 Linux 6.6 的行为语义与用户态生态（libfuse/fusermount/sshfs 等）。
>
> 说明：本文是“路线图”，不是最终设计文档；但每个阶段都给出**必须实现的东西**、**可独立 PR 的拆分**、**可单独测试的点**、以及**最终演示方式**。

---

## 0. 现状对齐：DragonOS 与 Linux 6.6 的关键实现接口

### 0.1 DragonOS 现有能力（与 FUSE 最相关的落点）

- **挂载框架已具备**：`sys_mount` → `do_mount()` → `produce_fs()` 通过 `FSMAKER` 分发创建文件系统实例（见 `kernel/src/filesystem/vfs/syscall/sys_mount.rs`、`kernel/src/filesystem/vfs/mod.rs`）。
- **VFS 操作面清晰**：文件系统通过实现 `FileSystem` + `IndexNode` 接入（`kernel/src/filesystem/vfs/mod.rs`）。
- **devfs 设备节点框架可复用**：/dev 由 devfs 统一管理，内置字符设备在 `DevFS::register_bultinin_device()` 中注册（`kernel/src/filesystem/devfs/mod.rs`）。
- **阻塞/唤醒与 poll/epoll 已具备基础设施**：
  - `WaitQueue` 可用于“读阻塞 / 写唤醒 / 信号打断”（`kernel/src/libs/wait_queue.rs`）。
  - VFS 有 `PollableInode` 抽象与 epoll 集成；`eventfd` 是一个可参考的“伪文件描述符 + wait_queue + epoll item 链表”范例（`kernel/src/filesystem/eventfd.rs`、`kernel/src/filesystem/vfs/mod.rs`）。
- **权限检查在路径遍历阶段发生**：`IndexNode::do_lookup_follow_symlink()` 会对目录执行权限（search/execute）做校验（`kernel/src/filesystem/vfs/mod.rs`）。这对 FUSE 的 `default_permissions` 语义很关键（后文详述）。
- **目录读取当前依赖 `IndexNode::list()+find()`**：`File::read_dir()` 首次会缓存 `inode.list()` 的全量目录项名，然后逐个 `inode.find(name)`（`kernel/src/filesystem/vfs/file.rs`）。这与 Linux 的“readdir 流式 + offset”模型有差异，是一个需要在路线图里显式处理的点。

### 0.2 Linux 6.6 FUSE 的“最小闭环”关键路径（需要对齐的行为）

以下为 Linux 6.6 的参考实现位置：

- 协议与 uapi：`include/uapi/linux/fuse.h`（当前内核版本号 `7.39`，定义了 `FUSE_INIT`、各 opcode、结构体与 feature flags）。
- /dev/fuse 设备（请求队列、read/write、poll、ioctl clone）：`fs/fuse/dev.c`
- 挂载参数解析与 superblock 建立：`fs/fuse/inode.c`
- 行为语义概述与 mount 选项：`Documentation/filesystems/fuse.rst`

Linux 的基本闭环如下：

1. 用户态 daemon `open("/dev/fuse")` 得到 fd；
2. `mount("fuse", target, "fuse", flags, "fd=N,rootmode=...,user_id=...,group_id=...")`；
3. 内核在连接建立后，**向 daemon 发送 `FUSE_INIT` 请求**（daemon read /dev/fuse 得到请求）；
4. daemon write 回 `FUSE_INIT` reply，完成版本/feature 协商；
5. 后续每个 VFS 操作（lookup/getattr/readdir/open/read/write…）转换成一条 FUSE request，daemon 处理后写回 reply，内核把结果映射回 VFS。

> 结论：DragonOS 要“有可用东西出来”，必须先做出上述闭环：**/dev/fuse + mount(fd=) + INIT 协商 + 若干核心 opcode**。

---

## 1. 必须实现的东西（按“能跑起来”到“能用起来”分层）

为了便于拆 PR/验收，这里把“必须实现”分成三层：MVP（可演示）、可用版（能跑常见 FUSE FS）、生产版（安全/语义/性能接近 Linux）。

### 1.1 MVP（可演示、可交付）的“最低必需集”

目标：实现一个**只依赖 DragonOS 自己的用户态 demo daemon**就能挂载并访问的 FUSE 文件系统（至少能 `ls`/`stat`/`cat`）。

MVP 必须具备：

1) **/dev/fuse 字符设备**
- devfs 中提供 `/dev/fuse`（最好保证路径一致；若 DragonOS 把大多数字符设备放在 `/dev/char/*`，也要给出 `/dev/fuse` 兼容入口：直接节点或符号链接）。
- 支持：
  - `read()`：从内核请求队列取下一条 request（无请求时可阻塞；支持 nonblock 返回 `EAGAIN`）。
  - `write()`：写入 userspace reply，按 `unique` 匹配并唤醒等待该 reply 的内核线程。
  - `poll()/epoll()`：队列非空 → 可读；通常永远可写；断连 → `POLLERR`。
  - `close()`：daemon 退出时触发断连/abort，避免内核永久阻塞。

2) **FUSE 连接对象（connection）与请求生命周期**
- 统一的 `FuseConn`：维护 `connected/aborted`、request id（unique）、请求队列、等待队列、以及与挂载点的关联。
- 请求：`(header + payload)` → 入队 → wake daemon → 内核线程 wait → reply 到达 → 唤醒返回。
- 信号语义（MVP 允许简化）：至少保证被信号打断时内核不死锁；可以先不实现完整 `FUSE_INTERRUPT`，但要规划后续补齐。

3) **挂载：`-t fuse -o fd=...`**
- 新增 `FuseFS` 作为 `FSMAKER` 的一个条目（如 `"fuse"`/`"fuse3"` 选其一或都支持）。
- `make_mount_data(raw_data)` 解析 `fd=,rootmode=,user_id=,group_id=`，并从当前进程 `fd_table` 获取对应 `File`，校验它确实来自 `/dev/fuse`。
- mount 成功后：触发或保证 daemon 读 /dev/fuse 时**能收到 `FUSE_INIT` 请求**（建议 mount 时把 INIT request 放入队列）。

4) **核心 opcode（覆盖 `ls/stat/cat` 的最小集合）**
- `FUSE_INIT`：版本协商 + feature flags（MVP 可只支持一个较小子集）。
- `FUSE_LOOKUP`：路径遍历（`IndexNode::find()`）必需。
- `FUSE_GETATTR`：`stat()`/权限判断/类型判断必需。
- `FUSE_OPENDIR + FUSE_READDIR + FUSE_RELEASEDIR`：目录 `list()` 必需。
- `FUSE_OPEN + FUSE_READ + FUSE_RELEASE`：读取文件内容必需。
- 建议同时包含：
  - `FUSE_FORGET`（可先“弱化实现”，至少能减少内核对象泄漏风险）。
  - `FUSE_STATFS`（可先返回固定值，后续再对齐）。

5) **权限与路径遍历的最小策略**
- DragonOS 当前在路径遍历时做 `MAY_EXEC` 检查；MVP 为了不被权限卡死，建议：
  - demo daemon 返回的目录权限至少 `0555`（或更宽）。
  - 先以 root 演示，降低权限语义复杂度。
- 但路线图里必须明确：后续要支持 Linux 的 `default_permissions` / `allow_other` / “仅 mount owner 可访问”语义（见 1.2/1.3）。

### 1.2 可用版（能跑更多现成 FUSE FS）的必需补齐

目标：能跑“更真实”的 FUSE FS（例如更完整的 hello/passthrough 示例，或未来接入 libfuse）。

需要补齐：

- **写路径与创建类操作**
  - `FUSE_CREATE`/`MKNOD`/`MKDIR`/`UNLINK`/`RMDIR`/`RENAME`（至少覆盖基础文件增删改名）
  - `FUSE_WRITE`/`SETATTR`（truncate/chmod/chown/utimens 等）
  - `FUSE_FLUSH`/`FSYNC`/`RELEASE` 的行为对齐（可分阶段）
- **更完整的目录语义**
  - `.`/`..`、`nlink`、inode id 稳定性、重名/硬链接等
  - 解决 DragonOS 当前 `list()` 模式与 FUSE `readdir+offset` 的差异：要么在 FUSE inode 内部做“全量拉取+缓存”，要么推动 VFS 抽象升级为“流式 readdir 回调”（推荐长期方向）
- **超时与缓存（attr/dentry cache）**
  - Linux FUSE 有属性缓存与超时（由 daemon 返回）；可先实现最小缓存策略，否则会导致性能与语义不稳（例如频繁 getattr）。
- **基础 xattr（可选但对生态有帮助）**
  - `GETXATTR/SETXATTR/LISTXATTR/REMOVEXATTR`（可后置，但要规划）

### 1.3 生产版（安全/语义/性能接近 Linux）的必需补齐

目标：支持非特权挂载（fusermount 模型）、更完善的信号/中断语义、可控的连接管理、以及性能可接受。

需要补齐：

- **非特权挂载安全模型**
  - “默认仅 mount owner 可访问”，`allow_other` 的限制（Linux fuse.rst 有详细动机）
  - 与 DragonOS `Cred`/capability 的对齐（哪些能力允许 `allow_other`、哪些 mount flags 允许普通用户）
  - 若要兼容 Linux 生态：最终需要用户态 `fusermount`（setuid root）或等价机制
- **请求中断与连接 abort**
  - `FUSE_INTERRUPT` 以及被信号打断时的竞态处理
  - 断连（daemon 死亡/close fd）时：所有 pending/processing 请求应快速失败并唤醒
- **并发与多线程**
  - `FUSE_DEV_IOC_CLONE`（libfuse 多线程常用）
  - 背景队列/最大并发（Linux 有 max_background 等控制）
- **性能与 IO 能力**
  - page cache / readahead / writeback cache（与 DragonOS `FileSystem::support_readahead()`、mm/page_cache 机制协同）
  - 大 IO、零拷贝（splice/pipe）、mmap（`FOPEN_DIRECT_IO` 等）

---

## 2. 推荐路线图（阶段 → 可独立 PR → 可独立测试）

下面按“每个 PR 可单测/可集成测”为原则拆分。实际合并粒度可根据人力调整，但建议保持“每步都能验收”。

### Phase A：协议与内核基础设施（不引入 mount 行为变更或只做最小变更）

**PR A1：引入 FUSE uapi 与协议编码/解码层（纯内核库）**
- 内容：
  - 在 `kernel/src/filesystem/` 下新增 `fuse/` 模块：定义 `fuse_in_header/fuse_out_header`、opcode 常量、init/lookup/getattr/readdir/open/read/write 等结构体（参考 Linux `include/uapi/linux/fuse.h`）。
  - 提供安全的序列化/反序列化工具（对齐 64-bit 对齐要求，避免未对齐访问）。
- 单测建议：
  - 内核侧（或 host-side）结构体布局/大小断言；
  - round-trip 编解码测试（给定样例字节流解析为结构体再编码一致）。

**PR A2：/dev/fuse 设备节点（devfs 注册 + 最小 file op）**
- 内容：
  - 在 devfs 内注册字符设备 `fuse`，并保证 `/dev/fuse` 路径可用（必要时增加 symlink）。
  - `open()` 为每个 fd 初始化私有状态（建议新增 `FilePrivateData::FuseDev(...)`，内部持有 `Arc<FuseConn>` 或 `Arc<FuseDevState>`）。
  - `read_at/write_at/poll/ioctl(close)` 的骨架先跑通（先不实现完整协议，只做队列读写框架）。
- 单测/集成测建议：
  - 用户态 smoke：`open("/dev/fuse")`，`poll()` 超时，向内核注入一条假 request（可通过调试接口或内核测试 hook），验证 poll 变为可读，read 能取到数据。
  - nonblock 行为：`O_NONBLOCK` 下 read 无数据返回 `EAGAIN`。

### Phase B：挂载闭环 + INIT 协商（做到“可演示”的里程碑）

**PR B1：FuseFS 注册到 `FSMAKER`，实现 `mount -t fuse -o fd=...`**
- 内容：
  - 新增 `FuseFS: MountableFileSystem`，注册 `"fuse"`（可选额外支持 `"fuse3"`，但建议从 `"fuse"` 起步）。
  - 解析 mount data（至少 `fd/rootmode/user_id/group_id`，参考 Linux `fs/fuse/inode.c` 与 `Documentation/filesystems/fuse.rst`）。
  - 通过 fd 取到 `/dev/fuse` 对应的 file/private_data，建立 `FuseConn` 与该挂载点绑定。
  - `SuperBlock.magic` 增加 `FUSE_SUPER_MAGIC`（Linux 常用值为 `0x65735546`，建议对齐），并为 `Magic` 增补该枚举项（`kernel/src/filesystem/vfs/mod.rs`）。
- 集成测建议：
  - 用户态：用最小 demo 程序完成 `open("/dev/fuse")` + `mount()`（即使 daemon 还没实现协议，也应能 mount 成功或在 INIT 未完成时明确返回错误）。

**PR B2：实现 INIT 请求生成与 init reply 处理（连接初始化状态机）**
- 内容：
  - mount 后将 `FUSE_INIT` request 入队，daemon `read("/dev/fuse")` 能拿到 INIT。
  - 处理 init reply：记录 negotiated version、max_write/max_read、flags 等（先实现最小必要集合）。
  - INIT 前禁止其它请求或让其它请求阻塞，避免未初始化就访问。
- 集成测建议（最重要的 MVP 里程碑测试）：
  - 用户态 demo daemon：只实现 INIT（收到 INIT → 回 INIT reply → sleep），验证 mount 后内核 `fc.initialized==true`，并且 `/proc/mounts`（若已有）或 `statfs` 能正常返回。

### Phase C：最小可用 opcode（做到 `ls/stat/cat`）

**PR C1：目录与路径遍历（LOOKUP/GETATTR/OPENDIR/READDIR/RELEASEDIR）**
- 内容：
  - `IndexNode::find(name)` → `FUSE_LOOKUP(parent, name)` → 返回子 inode（nodeid/attr）。
  - `IndexNode::metadata()` → 触发或使用缓存的 `FUSE_GETATTR`。
  - `IndexNode::list()`：通过 `FUSE_OPENDIR + FUSE_READDIR` 拉取目录项并返回 `Vec<String>`（MVP 允许“全量拉取+缓存”，但需写清楚局限）。
- 集成测建议：
  - 用户态 demo daemon 实现一个固定目录树（例如 `/hello.txt`、`/dir/a.txt`），在 DragonOS 内执行：`ls -l mountpoint`、`stat mountpoint/dir`。

**PR C2：文件读（OPEN/READ/RELEASE）**
- 内容：
  - `open()` 返回 file handle（fh）写入 `FilePrivateData::FuseFile`；
  - `read_at()` 生成 `FUSE_READ(nodeid, fh, offset, size)`，把 reply 数据拷回用户缓冲区。
- 集成测建议：
  - `cat mountpoint/hello.txt` 输出正确内容；
  - `dd if=... bs=... count=...` 验证分块 read 与 offset 正确。

> 至此应达到“可演示”标准：一个用户态 daemon 在 DragonOS 内运行，完成 mount 并提供可访问文件。

### Phase D：写与创建（更接近可用）

**PR D1：创建/删除/重命名（CREATE/MKDIR/UNLINK/RMDIR/RENAME）**
- 集成测建议：
  - `mkdir`, `touch`, `rm`, `mv` 在挂载点上工作（可以只支持最小集合，逐步扩展）。

**PR D2：写与属性修改（WRITE/SETATTR/FSYNC/FLUSH）**
- 集成测建议：
  - `echo hi > file`、`truncate -s`、`chmod/chown`（视 DragonOS 用户态工具链而定）。

### Phase E：Linux 语义与生态兼容（逐步接入 libfuse/fusermount）

**PR E1：权限语义对齐（default_permissions / allow_other / mount owner 限制）**
- 关键点：
  - DragonOS 的权限检查发生在 VFS 路径遍历与 open 等处；需要一个“按文件系统/挂载选项控制”的开关：
    - `default_permissions`：按 mode 做内核侧权限裁决；
    - 未开启时：尽量让访问透传给 daemon（更接近 Linux FUSE 默认行为）。
  - “默认仅 mount owner 可访问”与 `allow_other` 的安全限制，需要与 `Cred`/capability 结合设计。
- 测试建议：
  - 不同 uid/gid 下访问同一挂载点，行为符合预期。

**PR E2：`FUSE_DEV_IOC_CLONE` 与多线程（libfuse 常用路径）**
- 测试建议：
  - 单线程模式 `-s`（不依赖 clone）先跑通，再开多线程。

**PR E3：提供用户态最小工具链**
- 方向 1：先把 DragonOS 的 demo daemon 做成“低层协议实现”，作为持续回归测试。
- 方向 2：移植/引入 `libfuse3 + fusermount3`，逐步跑通现成 FUSE 文件系统（如 `sshfs`）。
- 测试建议：
  - `libfuse` 官方 example（hello/passthrough）的分步跑通清单。

---

## 3. 每个阶段“可以单独测试”的建议清单

为了避免“大工程最后才发现不通”，建议把测试分三层：

### 3.1 设备层（/dev/fuse）测试
- open/close：多次 open 是否产生独立连接或独立 dev state（按设计决定，但要可预测）。
- poll/epoll：队列空/非空/断连时返回事件是否正确。
- 阻塞 read：可被信号打断（返回 `ERESTARTSYS` 或 DragonOS 约定的等价错误）。
- write reply：unique 不存在/重复 reply 的错误处理。

### 3.2 协议层测试
- INIT：版本协商、feature flags 记录、max_read/max_write 限制是否生效。
- LOOKUP/GETATTR：inode 类型（dir/file/symlink）与 mode 位语义。
- READDIR：目录项编码解析（`fuse_dirent`）与 offset 行为（即使 MVP 做全量拉取也要保证不会重复/漏项）。

### 3.3 VFS 集成测试
- `ls -l`/`stat`/`cat`（MVP）
- `mkdir/touch/rm/mv`（写路径阶段）
- `umount`：daemon 存活/daemon 已死两种情况下都不会卡住；挂载点清理正确。

---

## 4. 最终成品如何演示（建议的“可复制演示脚本”）

建议把演示分成两档：**最小可演示**与**生态兼容演示**。

### 4.1 最小可演示（强烈建议优先做）

**演示目标**：在 DragonOS 里运行一个 `fuse_demo`（用户态 daemon），挂载到 `/mnt/fuse`，展示目录与文件读写。

建议的演示流程（单终端也能完成）：

1. 创建挂载点：`mkdir -p /mnt/fuse`
2. 启动 daemon（前台或后台）：`/bin/fuse_demo /mnt/fuse &`
3. 展示读路径：
   - `ls -l /mnt/fuse`
   - `cat /mnt/fuse/hello.txt`
4. 若已支持写路径，再展示：
   - `echo hi > /mnt/fuse/new.txt`
   - `cat /mnt/fuse/new.txt`
5. 卸载：`umount /mnt/fuse`（或 `umount2` 对应的用户态命令）

**验收标准（建议写入 CI/回归脚本）**：
- `mount()` 成功并完成 INIT；
- `ls/stat/cat` 全部成功；
- daemon 被 kill 后，内核访问不会永久阻塞（应快速失败并可 umount）。

### 4.2 生态兼容演示（后续阶段）

当 `libfuse3 + fusermount3` 跑通后，可以用更“直观”的演示：

- 跑 `libfuse` 官方 hello/passthrough 示例；
- 跑 `sshfs`（如果 DragonOS 网络栈与 openssh/ dropbear 可用）；
- 或跑 `rclone mount` 等典型 FUSE 应用（取决于用户态生态与依赖）。

---

## 5. 风险点与关键决策（需要在实施前明确）

1) **DragonOS 的 `IndexNode::list()` 是“全量目录项列表”接口**  
FUSE 原生是流式 `READDIR`（带 offset），大目录会导致内存/性能问题。  
路线建议：
- MVP：先在 FUSE inode 内部做“分页拉取→拼成 Vec→缓存”跑通；
- 可用版/生产版：推动 VFS 增加“流式 readdir”接口，减少一次性全量拉取。

2) **权限语义：DragonOS 当前在路径遍历阶段就检查目录执行权限**  
Linux FUSE 在 `default_permissions` 关闭时倾向把访问控制交给 daemon。  
要对齐语义，可能需要在 VFS 权限检查处引入“按文件系统/挂载选项”的可配置策略（这可能是跨模块的改动，应单独 PR、单独评审）。

3) **用户态生态的分阶段策略**  
建议先用自研 `fuse_demo` 打通内核闭环并做回归，再考虑移植 libfuse/fusermount；否则调试成本会很高且难以定位问题在内核还是用户库。

---

## 6. 建议的 Roadmap 里程碑（便于汇报/追踪）

- M0：PR A1+A2 合并（有 /dev/fuse 与协议骨架）
- M1：PR B1+B2 合并（mount(fd=) + INIT 协商跑通）
- M2：PR C1+C2 合并（`ls/stat/cat` 可演示）
- M3：PR D1+D2 合并（基础写路径可用）
- M4：PR E1 合并（权限语义对齐，安全模型清晰）
- M5：PR E2+E3 合并（libfuse/fusermount/生态演示）

