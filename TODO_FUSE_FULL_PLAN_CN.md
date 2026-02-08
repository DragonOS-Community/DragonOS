# DragonOS FUSE 完整实现分阶段计划（中文）

> 目标：在现有 Phase A~D 基础上，补齐 Linux 6.6 关键语义与生态兼容能力，最终达到可用、可维护、可回归测试的完整 FUSE 实现。

## 1. 现状与核心缺口

- 当前已具备 `/dev/fuse`、`mount(fd=...)`、`INIT`、基础读写与目录操作闭环。
- 主要缺口集中在：协议协商完整性、请求生命周期（尤其 FORGET）、权限语义细节、卸载销毁语义、以及 libfuse/fusermount 兼容路径。
- 当前 dead code（如 `FUSE_MIN_READ_BUFFER`、`FUSE_FORGET`、`FATTR_ATIME/MTIME/...`）对应的是“功能未落地”，不建议简单删除。

## 2. dead code 与缺失功能映射

- `FUSE_MIN_READ_BUFFER`：应接入 `/dev/fuse` 读缓冲校验逻辑。
- `FUSE_FORGET`：应接入 inode nlookup 生命周期与 forget 队列。
- `FATTR_ATIME/MTIME/FH/ATIME_NOW/MTIME_NOW/CTIME`：应接入 `SETATTR(valid)` 精确编码与 `utimens/futimens` 语义。
- `FuseFS` 中未消费字段（如 `owner_uid/owner_gid/allow_other/fd`）：要么用于语义与导出路径，要么移除避免误导。

## 3. 分阶段实现计划

### P0：协议与初始化补强（先把基础打牢）

**目标**：修正 INIT 协商和设备层基础语义，清零当前 warning 的“伪完成”状态。  
**范围**：

- 完整实现 `FUSE_INIT` 协商状态：记录并使用 negotiated `minor/flags/max_write/max_readahead/time_gran/max_pages`。
- 在 `/dev/fuse` 读取路径加入 Linux 语义的最小缓冲校验（使用 `FUSE_MIN_READ_BUFFER`）。
- 清理或落地当前未使用字段与常量，确保 dead code 反映真实功能状态。

**验收**：

- `mount + INIT` 协商字段在连接对象中可观测。
- `/dev/fuse` 小缓冲读取返回符合预期错误。
- FUSE 相关 dead code warning 显著下降且不靠“静默忽略”。

---

### P1：请求生命周期与缓存一致性

**目标**：补齐 inode 生命周期核心语义，避免长期运行下的资源与一致性问题。  
**范围**：

- 实现 `FUSE_FORGET`（可扩展到 `FUSE_BATCH_FORGET`）。
- 建立 `lookup -> nlookup++ -> forget` 的闭环。
- 接入 `entry_valid/attr_valid` 超时逻辑，完善 attr/dentry 缓存失效策略。
- 卸载时在已初始化连接发送 `FUSE_DESTROY`（再进行 abort/清理）。

**验收**：

- 压测反复 `lookup/readdir/umount` 不出现引用泄漏或僵死请求。
- daemon 侧可观测到 forget/destroy 请求序列，且时序稳定。

---

### P2：Linux 关键语义 opcode 补齐

**目标**：补齐“常用但目前缺失”的协议能力，使通用用户态程序更易跑通。  
**范围**：

- 权限相关：`FUSE_ACCESS`（尤其 Remote 权限模型下 `access/chdir` 语义）。
- 同步相关：`FUSE_FLUSH`、`FUSE_FSYNC`、`FUSE_FSYNCDIR`。
- 创建/链接族增强：`FUSE_CREATE`、`FUSE_LINK`、`FUSE_SYMLINK`、`FUSE_READLINK`、`FUSE_RENAME2`。
- 属性扩展：`SETATTR` 完整 valid 位处理；接入 xattr 族（`GETXATTR/SETXATTR/LISTXATTR/REMOVEXATTR`）。

**验收**：

- `chmod/chown/truncate/utimens`、`readlink/symlink/link/renameat2`、`fsync/fdatasync` 语义可回归。
- xattr 相关 syscall 测例可跑通基本路径。

---

### P3：高级协议与并发/中断语义

**目标**：提升复杂场景可靠性，向 Linux 6.6 行为进一步收敛。  
**范围**：

- `FUSE_INTERRUPT`：请求可中断、重入与竞态处理（含 `EAGAIN` 语义）。
- 支持 notification（`unique=0`）基础框架与关键 notify 类型。
- 目录增强：`READDIRPLUS`（可先按能力协商后开关）。
- 协商优化：`FUSE_NO_OPEN_SUPPORT`、`FUSE_NO_OPENDIR_SUPPORT` 等能力位处理。

**验收**：

- 信号打断下不死锁，错误码与请求完成路径可预测。
- 大目录与并发 readdir/open 场景行为稳定。

---

### P4：生态兼容与工程化收口

**目标**：形成“可演示 + 可回归 + 可迁移”的完整交付。  
**范围**：

- 跑通 `libfuse3 + fusermount3`（先单线程，再多线程 clone 路径）。
- 完善 `FUSE_DEV_IOC_CLONE` 与多 daemon worker 协作语义。
- 建立分层测试矩阵：`c_unitest`（设备/协议/VFS）+ gVisor syscall 回归 + demo 演示脚本。
- 形成故障注入用例：daemon 崩溃、超时、中断、umount 竞争。

**验收**：

- libfuse hello/passthrough 可稳定挂载与读写。
- CI/回归中包含关键 FUSE 测试集并可长期运行。

## 4. 推荐 PR 拆分策略

- 每阶段拆 2~4 个小 PR，先“接口与状态机”，后“opcode 实现”，最后“测试补齐”。
- 每个 PR 必须携带最小可复现实验（至少 1 个 c_unitest）。
- 禁止以临时 workaround 通过测试，必须按 Linux 6.6 语义修根因。

## 5. 实施顺序建议

1. 先做 P0（基础正确性）  
2. 再做 P1（生命周期）  
3. 之后并行推进 P2（功能）与 P3（复杂语义）  
4. 最后 P4（生态与工程化）收口

---

如需，我可以基于本计划继续输出：**第一批 P0 的具体代码任务清单（文件级别 + 函数级别 + 测试用例）**。
