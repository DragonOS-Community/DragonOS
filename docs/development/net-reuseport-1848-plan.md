# TCP SO_REUSEPORT Implementation Plan

> **For agentic workers:** 使用 superpowers:subagent-driven-development，按文件所有权拆分实现并集成审查。

**Goal:** 实现 TCP 逻辑 listener 分发和稳定绑定生命周期。
**Architecture:** DragonOS 管理 Linux admission，smoltcp 管理 opaque listener ID 与包匹配。
**Tech Stack:** Rust、smoltcp 0.12、C syscall tests、Linux 6.6、QEMU。
**Spec:** [已确认设计](net-reuseport-1848-design.md)。

## Global Constraints

- 保留已有未提交改动；不推送、不重写历史。
- 不按 backlog slot 数量加权；full 不 fallback；exact tuple/address 优先。
- 动态 option 预期以 Linux 6.6 探针和源码校正。
- 子模块及 consumer 同时验证，不以 host tests 替代 guest 验收。

## Task 1: smoltcp listener lookup

Files: `kernel/submodules/smoltcp/src/socket/tcp.rs`、`src/iface/interface/tcp.rs` 及包级测试。
Interface: `tcp::Socket::set_listener_id(Option<u64>)` / `listener_id() -> Option<u64>`。
调用方在 `listen()` 成功后设置 ID，在 accept 交付前清除；pending 保留。
hash 使用 Interface 的稳定随机种子，重复 ID 不增加权重。

- [x] 写包级失败测试：四元组/地址优先、等权、full 丢弃、RST、IPv4/IPv6。
- [x] 执行测试确认当前 first-match 行为不能满足测试。
- [x] 实现三阶段匹配与 metadata 生命周期，保留无 ID 调用方。
- [x] 执行 smoltcp 单元测试，审查实现及 consumer 接口。

## Task 2: DragonOS binding 与 listener 生命周期

Files: `kernel/src/net/socket/inet/common/port.rs`、`common/mod.rs`、`stream/*.rs`。
Interface: 不随 slot 替换变化的 binding ID；端口记录分开保存当前选项、owner UID 和监听状态。

- [x] 扩展 Linux 对照矩阵，确认动态选项与不同 UID 的预期。
- [x] 增加 admission/生命周期测试，暴露现有 record 与 handle 绑定的问题。
- [x] 实现稳定 binding 身份、Bound/Listening admission、选项同步、失败回滚。
- [x] 集成 Task 1 API，accept 清除旧 ID，replacement 继承 ID。
- [x] 删除 `tcp_listener_backlog.rs` 及所有注册、刷新、收包旁路。
- [x] `make kernel` 验证所有调用路径。

## Task 3: syscall 回归与集成验收

Files: `user/apps/c_unitest/test_tcp_reuseport.c`。

- [x] 编写可在 Linux 与 DragonOS 运行的有超时回归：共享/拒绝矩阵、分发、地址优先、
  accept 后关闭重绑、成员关闭、shutdown、IPv4/IPv6、动态 option。
- [x] Linux 上编译运行作为预期校验。
- [x] 执行 DragonOS guest 测试、相关 gVisor 与启动检查：C 13 项、目标 gVisor 6 项通过。
- [x] 对子模块和主仓库做最终审查，记录通过项与实际环境阻塞。

## 执行记录

- 2026-09-12：现有隔离工作树验证完成；接口与文件边界无冲突。
- Task 1 只写 smoltcp 子模块；Task 2 只写 DragonOS 内核；Task 3 只写 C 测试。
- 2026-09-12：实现和本轮验收完成；详见 [验证记录](net-reuseport-1848-validation.md)。
  smoltcp 614 项、端口表 16 项通过，内核编译成功。该阶段尚未提交或推送。
- 2026-09-12：按用户要求将 smoltcp 拆为 2 个、主仓库拆为 5 个语义提交；
  包含实现、独立修复、回归与文档，保留 stash，未推送。提交明细见验证记录。
