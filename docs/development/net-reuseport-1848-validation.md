# TCP SO_REUSEPORT 验证记录

## 原子提交整理

2026-09-12：以主仓库 `7edf3e48` 和 smoltcp `ebeaec6` 为基线拆分本地提交。

| 仓库 | 提交 | 语义 |
| --- | --- | --- |
| smoltcp | `97ba313` | 逻辑监听器身份、可选状态及生命周期 |
| smoltcp | `3e0cfb4` | 地址优先级、按流选择逻辑监听器及包级回归 |
| DragonOS | `8d32371a` | 网络命名空间 TCP 绑定、reuseport 监听组、accept 切换及配套子模块引用 |
| DragonOS | `fc23cb75` | 进入监听状态时清理旧就绪事件 |
| DragonOS | `19fa252f` | 双栈 TCP 的 IPv4-mapped IPv6 地址返回 |
| DragonOS | `17fe1244` | 端口管理器主机测试和 TCP reuseport 用户态回归 |

设计、计划和本验证记录作为最后一个独立文档提交。
子模块提交保存在本地分支 `feat/tcp-logical-listeners-1848`，主仓库功能提交引用 `3e0cfb4`。
主仓库与子模块的原 stash 均保留，未推送。

整理时逐文件 SHA256 对比初始快照，全部实现与测试文件内容保持一致；
仅文档补充提交状态。因此此前的内核构建及 `make test-syscall` 129/129 结果仍对应
同一份实现，本轮未重复启动 QEMU。

- smoltcp 第一个提交的独立暂存快照：635 项通过。
- smoltcp 最终提交使用独立 Cargo target 目录重新构建：645 项通过。
- 日志：`/tmp/reuseport-smol-stage1-test.log`、
  `/tmp/reuseport-atomic-smoltcp-final-test.log`。

## Rebase 后的 stash 恢复验证

2026-09-12：在 `7edf3e48`（已 rebase 到 `master`）上恢复 stash 并解决冲突。
保留上游 UDP 独立绑定表、网络驱动模块拆分、路由发布租约和 Closed 清理；
将 reuseport 的选择刷新迁移到新 poll/NAPI 路径，删除旧 backlog 旁路。

smoltcp 更新到上游内核依赖的 `ebeaec612ccdd36239588e4d8987fc14d623a52b`，
并恢复原有 reuseport 修改。子模块恢复备份 stash `5d8f1c9` 保留，主仓库
原始 stash 也保留。

- `make kernel`：通过；内核 SHA256
  `48358ffdef8f33f0ac6a28acfa6d68cd5ece6af60aabd2e34b2983e25a8d7673`。
- 端口表：16 项通过；smoltcp：645 项通过，no_std 检查通过。
- 编译日志 `/tmp/reuseport-post-rebase-kernel.log`；端口测试日志
  `/tmp/reuseport-post-rebase-port.log`；smoltcp 日志
  `/tmp/reuseport-smoltcp-ebeaec6-full.log`。
- 按用户要求使用仓库标准入口 `make test-syscall`，通过 `qemu-nographic`、
  KVM、Ubuntu rootfs 执行默认白名单：**129/129 个测试程序通过**，命令退出码 0，
  监控报告耗时 1033 秒。完整日志 `/tmp/reuseport-make-test-syscall.log`。
  该流程重新构建的实际测试内核 SHA256 为
  `280b0644f03e35cb02cedcd52f44d8da6ea9cdeecc443ca0f05da58a8686338d`。
- 标准监控脚本已终止 QEMU，`config/app-blocklist.toml` 已恢复。
  所有 stash 冲突已标记解决，`git diff --cached --check` 通过；该阶段尚未提交或推送。
- 默认白名单不包含 `socket_inet_loopback_test`。本轮未重跑前述专项 C13/10k；
  两次手动启动均未执行到 C13，不计为测试结果。下文为 rebase 前历史验收。

## Rebase 前的实现验收

日期：2026-09-12。主仓库基线 `f6a98466`，smoltcp 基线
`3933571d212b5c10cca7222c2e9a20fb2050870b`，当时均包含本次未提交修改。

## 主机测试

```sh
python3 tools/test_tcp_port_manager.py --offline
cargo test --manifest-path kernel/submodules/smoltcp/Cargo.toml --lib
cargo check --manifest-path kernel/submodules/smoltcp/Cargo.toml \
  --no-default-features --features alloc,medium-ip,socket-tcp,proto-ipv4,proto-ipv6
make kernel
```

- 实际 `port.rs` 的主机适配测试：16 通过，覆盖 UID、地址冲突、动态选项、
  fastreuse 缓存、监听组顺序与生命周期。适配层不验证内核锁/IRQ/调度。
- smoltcp：614 通过，其中逻辑 listener 回归 11 项；no_std 配置检查通过。
  覆盖四元组优先、三层地址优先级、重复 slot 不加权、full 不 fallback、
  disabled 成员、pending RST/timeout 恢复及显式关闭不复活。
- `make kernel`：通过；基线缺失 `set_reuseport()` 的编译错误已消除。
- C 回归在 Linux `6.6.87.2-microsoft-standard-WSL2` 上：12 通过、0 失败、
  1 跳过；唯一跳过是非 root 环境下的不同 UID 用例。

```sh
gcc -std=c11 -Wall -Wextra -Werror -O2 \
  user/apps/c_unitest/test_tcp_reuseport.c -o /tmp/test_tcp_reuseport
timeout 40s /tmp/test_tcp_reuseport
```

## Guest 执行方式

在 QEMU TCG 中启动构建出的内核，使用已有 FAT32 rootfs 的临时副本及
`-snapshot`。C 程序使用项目的 musl 交叉编译器静态编译，置于
`/opt/tests/test_tcp_reuseport`。直接指定该程序为 init，避免交互串口丢字符。
程序成功退出后内核会记录 PID 1 `group_exit code 0`，这是此运行方式的退出日志。

镜像内外二进制 SHA256 已核对一致：C 程序为
`482320ef1d2e19bf0ec06139c3a1abfddc4c3b41a61c14f76caf7b56451a84e1`；
gVisor 为 `4f8d3402d4d1e1b93e73a0d9aa94906d6e1f8fd0823e9eb6ea27374ed3032ae9`。
本记录不将本地 gVisor 源码提交等同于预置二进制的构建版本。

gVisor 的 init 参数必须放在内核命令行分隔符 `--` 后，例如：

```text
rw init=/opt/tests/gvisor/tests/socket_inet_loopback_test console=/dev/hvc0 -- --gtest_filter=*TcpPortReuseMultiThread*
```

使用 musl 是本次已有 rootfs 的运行要求：静态 glibc 版本在启动阶段的 brk/TLS
路径异常退出，未执行任何 socket 测试，不能计为 reuseport 测试结果。

## 集成结果

内核 SHA256 `95e35830cb113a839c0ee56fe7367fa9574ee0e6f9ad093ffe34d6e997b08f5c`：
C 回归 **12 通过、0 失败、0 跳过**，包括不同有效 UID 的隔离。
backlog 1:8 的 80 次连接分布为 34:46。

先前 guest 明确复现空 listener 的 poll 返回 `POLLHUP (0x10)`；修复后空队列
`poll` 返回 0、事件位为 0，shutdown/relisten 也通过。完整临时日志：
`/tmp/reuseport-poll2-serial.log`（RED）、`/tmp/reuseport-final-c-serial.log`（GREEN）。

加入 IPv4-mapped 用户可见端点修复后，最终内核 SHA256
`3f11d9313bd9c1f19f9da3de9abad92a2bc52d0032b543a5c7d095c01564266a`：
C 回归 **13 通过、0 失败、0 跳过**，分布 40:40。新增用例验证 IPv6 `::`
接收 IPv4 连接时，accept/getsockname/getpeername 返回 AF_INET6、正确的
sockaddr_in6 长度、mapped loopback 地址及两端端口。
日志：`/tmp/reuseport-final3-c-serial.log`。

gVisor 短回归 **5 通过、0 失败**（同一最终内核，guest 64 ms）：

```text
AllFamilies/SocketMultiProtocolInetLoopbackTest.DualStackV6Any*/TCP:AllFamilies/SocketMultiProtocolInetLoopbackTest.NoReusePortFollowingReusePort/TCP
```

包括 IPv6 `::` 的独占预留、REUSEADDR 下的绑定/监听差异，以及非 REUSEPORT
后续成员拒绝；完整日志 `/tmp/reuseport-short-gvisor-serial.log`。

多线程 gVisor **1 通过、0 失败**，guest 测试耗时 650892 ms：

```text
All/SocketInetReusePortTest.TcpPortReuseMultiThread/ListenV4Any_ConnectV4Loopback
```

10,000 次连接全部完成，三个 listener 的分流比例断言和 shutdown 后线程退出
均通过；日志 `/tmp/reuseport-long-gvisor-serial.log`。本次未运行其他四种地址
参数的 10,000 连接用例；IPv6 与默认双栈另由 C 回归及上述短 gVisor 覆盖。

预置二进制在主机 Linux 上执行单个
`*TcpPortReuseMultiThread/ListenV4Any_ConnectV4Loopback` 已通过（3308 ms）。
该用例有 3 个 backlog=40 的 listener 和 10,000 次串行连接，无中间进度输出；
首次 guest 运行 60 秒后停止，只能记为未完成，不能据此判断死锁。
随后在同一最终内核中持续运行，通过 GDB 读取已有 `TcpBindId::new::NEXT_ID`
观察到 1250、1555、2501、4131、5751、6804、8322、9304 的连续增长，
且 CPU 位于网络处理/通知路径，最终完整通过。未为取证加入高频日志或修改内核。

## 最终检查

- 主仓库和 smoltcp 子模块 `git diff --check` 均通过。
- 最终定向审查提出的 accept 地址 family 问题已修复并通过 guest 回归。
- 运行验收对应上述最终内核 SHA256；最后一次构建后只有测试/文档修改。

## 交付边界

主仓库 consumer 与 smoltcp 子模块修改必须成对交付。现已按下述语义边界创建本地提交，未推送。
上述测试不能证明完整 Linux 网络语义兼容；未扩展 UDP reuseport 或 TCP IPV6_V6ONLY。
临时日志用于本次现场取证，复现以代码中的回归测试和以上命令为准。
