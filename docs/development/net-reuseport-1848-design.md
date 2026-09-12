# TCP SO_REUSEPORT 续作设计

状态：用户于 2026-09-12 确认，实现与本轮验收完成，已拆分为本地语义提交，未推送。
接续 Claude 会话 `#1848 SO_REUSEPORT`。结果见 [验证记录](net-reuseport-1848-validation.md)。

## 续作开始时的基线

- DragonOS HEAD：`f6a98466`，分支 `feat/net-reuseport-1848`。
- smoltcp 已作为 `kernel/submodules/smoltcp` 子模块引入，当时提交为
  `3933571d212b5c10cca7222c2e9a20fb2050870b`。
- 本次实际执行 `make kernel`，失败于 `stream/inner.rs:61,363`：
  `E0599: no method named set_reuseport`。完整日志在
  `/tmp/reuseport-1848-baseline-build.log`，该临时文件不属于交付物。
- 续作前的三个未提交文件是 `common/port.rs`、`stream/inner.rs`、
  `stream/lifecycle.rs`；其现有注释与格式修改应保留。
- 上次最后结论是 opaque logical listener ID 候选方案；独立架构评审中断，
  旧计划尚未按该方案更新。不能把旧计划直接当成已经验证的实现规格。

## 推荐边界

DragonOS 管理 Linux socket 语义：端口所有权、socket owner UID、地址及协议族、
bind/listen admission、选项变化、监听组成员和关闭过程。
smoltcp 管理包匹配与处理：已连接四元组优先、地址匹配层级、逻辑 listener 选择、
选中 listener 内的空闲 slot，以及队列满时的静默丢弃。

不采用只放宽 bind 的方案，因为当前 smoltcp TCP ingress 按 SocketSet 遍历顺序
命中第一个 socket。也不把 Linux UID、SO_REUSEPORT 标志或进程身份带入 smoltcp。

### 1. 稳定端口身份

为每个成功 binding 分配不随 smoltcp handle 变化的 `TcpBindId`。
将 `Init::Bound` 的 tuple 改为具名绑定状态，让端口所有权独立于 backlog slot。
`accept()` 替换 slot、跨接口展开均保留同一身份。shutdown 回退 Bound 时，
显式端口保留绑定；自动分配端口释放绑定，再 listen/connect 时取得新身份与端口。
每个 binding 的端口记录只释放一次。

PortManager 分开记录 Bound 与 Listening 状态。在同一锁域内检查冲突并保留
listen admission；slot 构建失败必须回滚保留状态并释放新增 handle。
跨层锁顺序应沿用现有 poll 约束：不在持有端口表锁时再获取 SocketSet 锁。

### 2. 通用 listener 选择

一个用户 listener 的全部 slots 共享 opaque `listen_id`；新增 slot 不增加选路权重。
ID 在 pending connection 阶段保留，用于判断选中 listener 是否已经没有空闲 slot；
accept 交付连接时清除其 listener 身份，replacement slot 继承原 ID。
显式关闭和失败回滚清理该身份；内部 pending RST/超时需要保留逻辑监听意图并及时补槽，
否则同一批收包中的下一个 SYN 会看到监听器短暂消失。

fresh SYN 的分发顺序：

1. 先搜索已有连接/半连接的完整四元组，保证重传回到原 socket。
2. 查找匹配协议族、目的地址和端口的 listener；优先级为 exact address、
   指定协议族的 wildcard、未指定协议族的 wildcard。DragonOS 默认 IPv6 `::`
   监听使用最后一种形式保持双栈；IPv4 `0.0.0.0` 使用 IPv4 wildcard。
3. 在该地址层按逻辑 ID 做带种子的 rendezvous hash；相同 ID 的重复 slots
   得到相同分数，不需要为去重分配 Vec。平分按 ID 决定，不能依赖遍历顺序。
4. 只在选中的逻辑 listener 内找空闲 LISTEN slot。
5. 已匹配但没有空闲 slot 时静默丢 SYN；没有 listener 时保持现有 RST 行为。

公共入口当前返回 `Option<Packet>`，已经可以通过直接返回 `None` 表达静默处理；
实现时可在内部使用选择结果枚举，区分 no-match 与 full，避免扩大公共返回类型。
未配置逻辑 ID 的普通 smoltcp 使用者需要保留原有处理能力；不能要求所有下游
调用者先接入 DragonOS 的 listener 模型。混合配置的精确规则须用包级测试固定。

选中 listener 满时不转投其他成员。种子属于通用分发机制，由调用方配置；
不能以每个 slot 的独立随机数代替逻辑成员身份。

实施中增补通用 `listener_enabled` 标记：DragonOS 在持有 SocketSet 锁、进入 poll 前
刷新当前可选成员，smoltcp 保留 ID 并只对 fresh SYN 应用标记。它不影响已有四元组。
这使 DragonOS 能区分当前 REUSEPORT 值与既有组成员：关闭选项并不总是退出已有组。
IPv4 hash 链头插入与 IPv6 reuseport 链尾插入由 DragonOS 记录；smoltcp 不解释这些策略。

### 3. 移除端口级丢 SYN 旁路

在 smoltcp 的 full/no-match 测试通过后，移除 `tcp_listener_backlog.rs` 以及
接口注册、刷新和收包检查的调用链。当前旁路只按端口计数，不能表示独立成员
或 exact/wildcard 层级；仅加引用计数仍不足以支撑选中成员队列满的语义。

## 对旧计划的语义修正

旧计划将 option 永久冻结为 bind-time snapshot，并断言 bind 后启用 reuseport
仍必然使后续 bind 失败。此断言已被本机 Linux 行为反驳，不能据此写回归测试。

本次在 `6.6.87.2-microsoft-standard-WSL2` 的 IPv4 loopback 上运行探针：
第一个 socket 不设置 REUSEADDR，以 bind(0) 获取端口；分别在 Bound 或 Listening
阶段将 REUSEPORT 从 0 改为 1、从 1 改为 0；第二个启用 REUSEPORT 的 socket
均成功 bind 和 listen。只验证了这组调用序列，未验证这些组合的连接分流。

Linux 6.6 的动态字段、端口桶 fastreuseport 缓存与实际监听组是不同状态：
不能由该探针推导为“只同步一个 bool 即兼容 Linux”。实施 admission 前应补齐
显式端口、选项修改时机、成员加入/退出、UID 和连接分发的矩阵，分别对应源码路径。

2026-09-12 的补充探针与实现约束：

- IPv4 已有 A、B 同组，停用 A 后两者仍分流；停用较晚 listen 的 B 后仅 B 接收。
  IPv6 reuseport 链尾插入使这组顺序相反。
- strict fastreuse 缓存仅允许缓存地址一侧通配；不能因新绑定为 wildcard 跳过其他独占绑定。
- bind(0) 的 listener shutdown 后 getsockname 暂保留原端口，再 listen 分配新端口；
  显式绑定非零端口则保留绑定。二者在 IPv4/IPv6、exact/wildcard 探针中均已验证。
- accept 在同一 SocketSet 锁内复核连接状态、快照两端地址、清除旧 slot 身份并
  发布 replacement，避免 peer RST 与交付竞争；仅 Established/CloseWait 可交付。
- listen 成功后立即重建 readiness；监听状态按队列状态写入完整事件掩码，
  避免继承未连接状态的 HUP，导致空监听队列的 poll 提前返回。
- 用户可见端点按 socket 协议族输出：AF_INET6 socket 接收 IPv4 连接时，
  accept/getsockname/getpeername 将地址映射为 `::ffff:` 形式，传输四元组不变。

参考：

- [Linux 6.6 inet_connection_sock.c](https://raw.githubusercontent.com/torvalds/linux/v6.6/net/ipv4/inet_connection_sock.c)：
  `inet_bind_conflict`、`sk_reuseport_match`、`inet_csk_update_fastreuse`。
- [Linux 6.6 inet_hashtables.c](https://raw.githubusercontent.com/torvalds/linux/v6.6/net/ipv4/inet_hashtables.c)：
  `inet_reuseport_add_sock`、`__inet_hash`、`inet_unhash`。

## 实施与验收顺序

1. 用 Linux 行为探针校正 admission 与动态 option 的预期，不复用旧计划中的错误断言。
2. smoltcp：增加逻辑 listener API 和包级测试，覆盖 IPv4/IPv6、四元组优先、
   地址优先、slot 数量不影响权重、full 静默丢弃和未监听 RST。
3. DragonOS：稳定 binding 身份、admission 与回滚、listener slots 生命周期，
   替换当前不存在的 `set_reuseport()` 调用。
4. 接入新 lookup 后删除端口级旁路；验证 accept 后关闭、成员关闭、shutdown 后
   再 listen、动态 option、不同 UID、跨接口 wildcard 和失败回滚。
5. 在 `user/apps/c_unitest` 增加系统调用回归，先 Linux 对照，再 DragonOS guest；
   执行 `make kernel`、相关 gVisor 测例和 QEMU 启动检查。

子模块改动与 DragonOS consumer 必须成对交付。未经实际构建及 guest 验证，
不把设计、smoltcp 单元测试或历史运行结果视为 SO_REUSEPORT 已完成。
