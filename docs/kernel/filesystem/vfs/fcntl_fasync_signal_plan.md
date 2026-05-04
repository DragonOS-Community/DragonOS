# F_SETSIG/F_GETSIG 与 fasync 修复方案

## 1. 背景

Issue: https://github.com/DragonOS-Community/DragonOS/issues/1841

PR: https://github.com/DragonOS-Community/DragonOS/pull/1880

目标是在 DragonOS 中实现 Linux 兼容的 `fcntl(F_SETSIG)` 和 `fcntl(F_GETSIG)` 语义，使异步 I/O 通知不只支持默认 `SIGIO`，还支持用户通过 `F_SETSIG` 设置的标准信号或实时信号。

PR 1880 已经补充了 `FcntlCommand::SetSig`、`FcntlCommand::GetSig` 以及基础的 set/get 测试，但 review 指出了两个关键问题：

- `F_SETSIG` 的值只被保存，没有接入 `fasync` 实际投递路径。
- signal owner 状态不应作为独立原子变量直接放在 `File` 中，应与 owner pid 一起建模并使用同一把锁保护。

## 2. 测试范围理解

gVisor `test/syscalls/linux/fcntl.cc` 中相关测试覆盖两类行为：

| 测试范围 | 期望语义 |
|---------|---------|
| `FcntlTest.SetSig*` | `F_SETSIG` 校验信号值并持久化，`F_GETSIG` 返回当前设置，非法值不覆盖旧值 |
| `FcntlSignalTest.SetSigDefault` | `signum == 0` 表示默认行为，I/O ready 时投递 `SIGIO` |
| `FcntlSignalTest.SetSigCustom` | 非 0 signum 应投递用户指定信号，并在 `siginfo_t` 中携带 `si_fd` 和 `si_band` |
| `FcntlSignalTest.SetSigWithSigioStillGetsSiginfo` | 显式设置 `SIGIO` 与默认 `0` 不等价；前者应携带 `siginfo_t` |
| `FcntlSignalTest.SetSigDup*` | `dup` 后共享同一个 open file description，但 fasync 注册项记录触发通知时使用的 fd |
| `FcntlSignalTest.SetSigDupUnregister*` | 重新设置 `F_SETSIG(0)` 后回到默认 `SIGIO` 行为 |
| `FcntlSignalTest.ConcurrentSetSigSetOwnSetOAsync` | `F_SETOWN`、`F_SETSIG`、`F_SETFL(O_ASYNC)` 并发更新不能产生不一致状态 |

因此，修复不能只做 `F_SETSIG/F_GETSIG` round-trip，还必须打通真实异步通知链路。

## 3. 内核现状

| 位置 | 现状 | 问题 |
|-----|------|------|
| `kernel/src/filesystem/vfs/fcntl.rs` | 当前 master 尚无 `SetSig = 10`、`GetSig = 11` | 命令无法被 `FcntlCommand::from_u32` 识别 |
| `kernel/src/filesystem/vfs/file.rs` | `File` 使用 `pid: Mutex<Option<Arc<ProcessControlBlock>>>` 记录异步通知 owner | owner pid 与 signum 分散存储会产生状态一致性问题 |
| `kernel/src/filesystem/vfs/fasync.rs` | `send_sigio()` 固定发送 `Signal::SIGIO_OR_POLL` | 忽略 `F_SETSIG` 设置，用户仍只能收到默认 `SIGIO` |
| `kernel/src/filesystem/vfs/fasync.rs` | `FAsyncItem` 只保存 `Weak<File>` | 无法按 Linux `fasync_struct::fa_fd` 语义返回注册 fd |
| `kernel/src/filesystem/vfs/syscall/sys_fcntl.rs` | `F_SETFL` 只更新 `FileFlags::FASYNC` | 通过 `fcntl(F_SETFL, O_ASYNC)` 不会注册/注销 `FAsyncItem` |
| `kernel/src/filesystem/vfs/syscall/sys_ioctl.rs` | `FIOASYNC` 内联完成 flags 更新和 fasync 注册 | 可复用逻辑没有抽成 VFS helper，容易与 `F_SETFL` 行为分叉 |
| `kernel/src/ipc/signal_types.rs` | `PosixSiginfoSigpoll` 已有 `si_band/si_fd` 布局，但 `SigType` 没有 sigpoll 变体 | 无法构造符合 `SA_SIGINFO` 的异步 I/O 信号 |

## 4. 根因分析

| 测试点 | Linux 期望 | DragonOS 实际 | 差距 |
|-------|------------|---------------|------|
| `F_GETSIG/F_SETSIG` 命令识别 | `F_GETSIG = 11` 返回 `file.f_owner.signum`，`F_SETSIG = 10` 校验后更新 | 当前 master 未识别命令；PR 1880 只补了存取 | 缺少完整命令实现 |
| owner/signum 状态 | Linux `struct fown_struct` 同时保存 pid、pid type、uid/euid、signum，并通过 lock 保护 owner 字段 | DragonOS 只有 `pid`，PR 1880 将 `signum` 作为独立原子字段 | owner 和 signum 不是同一一致性域 |
| fasync 投递 | Linux `kill_fasync` 从 `fa_file->f_owner` 读取 signum，并调用 `send_sigio(fown, fa_fd, band)` | DragonOS `FAsyncItems::send_sigio()` 固定发送 `SIGIO_OR_POLL` | `F_SETSIG` 不影响真实通知 |
| `siginfo_t` | 显式 `F_SETSIG(nonzero)` 投递 queued SIGIO 风格信号，携带 `si_fd` 和 `si_band` | 当前信号路径只构造 kill/user 类型信息 | `SA_SIGINFO` 测试无法通过 |
| `F_SETFL(O_ASYNC)` | 设置/清除 `O_ASYNC` 时更新 fasync 注册状态 | `F_SETFL` 只改 flags；`FIOASYNC` 才注册 | gVisor `RegisterFD()` 使用 `F_SETFL`，真实投递链路缺失 |

## 5. 修复方案

### 5.1 关键改动

| 文件 | 改动 | 原因 |
|-----|------|------|
| `kernel/src/filesystem/vfs/fcntl.rs` | 增加 `SetSig = 10` 与 `GetSig = 11` | 识别 Linux 标准 fcntl 命令 |
| `kernel/src/filesystem/vfs/file.rs` | 引入 `FileOwner`，用 `Mutex<FileOwner>` 替代 `pid: Mutex<Option<Arc<ProcessControlBlock>>>` | 对齐 Linux `fown_struct` 的 owner/signum 一致性域 |
| `kernel/src/filesystem/vfs/file.rs` | 提供 `owner_snapshot()`、`set_owner()`、`set_owner_signum()`、`owner_signum()` 等方法 | 避免调用者直接拼装或拆分 owner 状态 |
| `kernel/src/filesystem/vfs/fasync.rs` | `FAsyncItem` 保存注册 fd，并在发送时使用 owner signum 与 fd | 支持 `si_fd` 和 dup 场景 |
| `kernel/src/filesystem/vfs/fasync.rs` | 将固定 `SIGIO` 发送改为 `signum == 0` 发默认 `SIGIO_OR_POLL`，非 0 发指定信号 | 让 `F_SETSIG` 影响真实投递 |
| `kernel/src/ipc/pipe.rs` | 为 pipe 维护读端/写端两组 fasync 注册队列 | 避免写端收到可读通知，读端收到可写通知 |
| `kernel/src/ipc/signal_types.rs` | 增加 `SigType::SigPoll { fd, band }` 并转换到 `_sigpoll` | 支持 `SA_SIGINFO` 下的 `si_fd/si_band` |
| `kernel/src/filesystem/vfs/syscall/sys_fcntl.rs` | 实现 `F_SETSIG/F_GETSIG`，并复用统一 fasync 切换 helper | 保持 `F_SETFL(O_ASYNC)` 与 `FIOASYNC` 行为一致 |
| `kernel/src/filesystem/vfs/syscall/sys_ioctl.rs` | `FIOASYNC` 改为调用统一 helper | 避免两条入口语义分叉 |

### 5.2 推荐数据结构

在 `file.rs` 中新增 owner 状态对象：

```rust
#[derive(Clone, Debug)]
pub struct FileOwnerSnapshot {
    pub pcb: Option<Arc<ProcessControlBlock>>,
    pub signum: i32,
}

#[derive(Debug)]
struct FileOwner {
    pcb: Option<Arc<ProcessControlBlock>>,
    signum: i32,
}
```

`File` 中使用：

```rust
owner: Mutex<FileOwner>,
```

约束：

- `signum == 0` 表示默认 `SIGIO` 行为。
- `F_SETSIG` 只接受 `0..=Signal::SIGRTMAX`。
- 不允许新增独立 `AtomicI32` 保存 `F_SETSIG` 状态。
- `F_SETOWN` 与 `F_SETSIG` 必须通过 `FileOwner` 方法更新，避免调用者绕过锁。

### 5.3 fasync helper

建议新增一个统一 helper，供 `F_SETFL` 与 `FIOASYNC` 同时调用：

```rust
pub fn set_file_fasync(file: &Arc<File>, fd: i32, enabled: bool) -> Result<(), SystemError>
```

职责：

- 更新 `FileFlags::FASYNC`。
- `enabled == true` 时创建或更新 `FAsyncItem`，记录 `Weak<File>` 与注册 fd。
- `enabled == false` 时移除当前 file 对应注册项。
- 对不支持 `PollableInode::add_fasync` 的 inode 保持现有兼容策略：flags 可更新，注册失败不应破坏普通 `F_SETFL` 语义，但需要在代码注释中说明。

### 5.4 信号投递策略

`FAsyncItems::send_sigio()` 应改为按事件类型传递 band：

```rust
pub fn send_sigio(&self, band: i32)
```

初期可先在调用点传入常用值：

- 可读事件：`POLL_IN` 对应 `EPOLLIN | EPOLLRDNORM`。
- 可写事件：`POLL_OUT` 对应 `EPOLLOUT | EPOLLWRNORM | EPOLLWRBAND`。
- 暂时无法区分的旧调用点可先传入 `POLL_IN`，但必须在文档或注释中标明后续要按事件类型细分。

发送规则：

- `signum == 0`：发送默认 `Signal::SIGIO_OR_POLL`，`siginfo_t` 内容可不保证。
- `signum != 0`：发送指定信号，并构造 `SigInfo { sig_code: SigCode::SigIO, sig_type: SigType::SigPoll { fd, band } }`。
- 如果 DragonOS 信号队列暂不支持 Linux 的 queued signal fallback，可先不实现 fallback，但要在测试计划中明确风险。

## 6. 分阶段实施计划

### 阶段一：补齐 fcntl 命令与状态建模

1. 在 `FcntlCommand` 中增加 `SetSig`、`GetSig`。
2. 将 `File::pid` 重构为 `File::owner: Mutex<FileOwner>`。
3. 更新 `set_owner()`、`owner()`、`get_owner()` 等接口。
4. 在 `sys_fcntl.rs` 增加 `F_SETSIG/F_GETSIG` 分支。
5. 保留 `EBADF` 优先级：先查 fd，再校验 `arg`。

验收标准：

- `F_GETSIG` 默认返回 0。
- `F_SETSIG(SIGUSR1)` 后 `F_GETSIG` 返回 `SIGUSR1`。
- `F_SETSIG(SIGRTMAX + 1)` 返回 `EINVAL`，且不覆盖旧值。
- 无效 fd 返回 `EBADF`。

### 阶段二：统一 O_ASYNC/fasync 注册

1. 抽出 `set_file_fasync()` helper。
2. `ioctl(FIOASYNC)` 调用 helper。
3. `fcntl(F_SETFL)` 检测 `FASYNC` 位变化并调用 helper。
4. `FAsyncItem` 增加 fd 字段，注册重复项时更新 fd 而不是重复 push。
5. pipe 按 `FilePrivateData::Pipefs(flags)` 将读端注册到 read fasync 队列，将写端注册到 write fasync 队列。

验收标准：

- `fcntl(F_SETFL, old | O_ASYNC)` 能注册 fasync。
- `fcntl(F_SETFL, old & ~O_ASYNC)` 能注销 fasync。
- `ioctl(FIOASYNC)` 与 `F_SETFL(O_ASYNC)` 行为一致。
- pipe 写入只通知读端，pipe 读取释放空间只通知写端。

### 阶段三：接入 signum 与 siginfo 投递

1. `FAsyncItems::send_sigio()` 使用 `FileOwnerSnapshot`。
2. `signum == 0` 走默认 `SIGIO_OR_POLL`。
3. `signum != 0` 发送指定信号。
4. 增加 `SigType::SigPoll`，转换到 `PosixSiginfoSigpoll`。
5. 按调用点传入 read/write 对应 band。

验收标准：

- 设置 `F_SETSIG(SIGUSR1)` 后，pipe/socket ready 时收到 `SIGUSR1`。
- `SA_SIGINFO` handler 中 `si_signo == SIGUSR1`。
- `si_fd` 等于注册 `O_ASYNC` 的 fd。
- `si_band` 至少覆盖 `EPOLLIN | EPOLLRDNORM`。

### 阶段四：补充测试与回归

1. 扩展 `user/apps/tests/dunitest/suites/normal/fcntl_signal.cc`。
2. 覆盖 set/get、非法参数、默认 `SIGIO`、自定义信号、dup 后 fd 语义。
3. 若 gVisor runner 可用，重点跑 `fcntl.cc` 中 `FcntlTest.SetSig*` 与 `FcntlSignalTest.*`。
4. 执行 `make kernel` 检查内核编译。
5. 最后执行 `make fmt`。

## 7. 风险与注意事项

- 当前 `F_SETOWN` 只支持按 pid 查找，尚未完整支持 Linux 的负 pid/process group 语义；本次修复不要扩大范围，避免和 `F_SETOWN_EX` 语义混在一起。
- `dup` 共享同一个 `Arc<File>`，owner/signum 应共享；但 fasync 注册项的 fd 应是注册 `O_ASYNC` 时的 fd。
- 非实时普通信号在 DragonOS 中可能被合并，gVisor 的并发测试需要关注队列语义差异。
- `F_SETFL` 更新 flags 与 fasync 注册之间应避免长时间持有 fd table 锁，防止调度或锁顺序问题。
- 若某个 inode 不支持 `PollableInode::add_fasync`，不要为了通过测试写 workaround；应明确返回或兼容策略并保持 Linux 语义优先。
