# AF_PACKET PACKET_FANOUT 实施方案

> Issue #2032：为 AF_PACKET 实现 `PACKET_FANOUT`，支持多 socket 间分发数据包做负载均衡。
> 参考：Linux 6.6 `net/packet/af_packet.c`、`include/uapi/linux/if_packet.h`、`packet(7)`。

## 1. 背景与目标

AF_PACKET 当前只有广播模式：`NetNamespace::deliver_to_packet_sockets()` 把每个入站帧的副本投递给**所有**已注册的 packet socket。多个 socket 绑定同一网卡时各自收到完整副本，无法并行分担负载。

`PACKET_FANOUT`（option 18）让一组 socket 形成 fanout group：每个匹配帧只投递给组内**一个** socket，由分发算法决定选谁。

**目标**：实现符合 Linux 6.6 语义的 `PACKET_FANOUT`，覆盖主流分发算法，不破坏现有广播语义。

---

## 2. Linux PACKET_FANOUT 语义（权威参考）

### 2.1 option value 编码

`setsockopt(fd, SOL_PACKET, PACKET_FANOUT, &val, sizeof(int))`，`val` 为 32 位 int：

| 位段 | 含义 |
|------|------|
| 低 16 位 `val & 0xffff` | **group id**（u16，0 表示请求内核自动分配，需配合 `FLAG_UNIQUEID`） |
| 高 16 位 `val >> 16` | **type_flags**：低 7 位是 mode，其余是 flags |

mode = `type_flags & 0x7f`；flags = `type_flags & ~0x7f`。

### 2.2 mode 常量（`type_flags & 0x7f`）

| 常量 | 值 | 算法 |
|------|----|------|
| `PACKET_FANOUT_HASH` | 0 | flow hash（src/dst IP + ports 的 jhash）取模成员数 |
| `PACKET_FANOUT_LB` | 1 | round-robin（原子计数器取模） |
| `PACKET_FANOUT_CPU` | 2 | 收包 CPU id 取模成员数 |
| `PACKET_FANOUT_ROLLOVER` | 3 | 固定从起始 socket，满了滚到下一个 |
| `PACKET_FANOUT_RND` | 4 | 随机选取 |
| `PACKET_FANOUT_QM` | 5 | 按 NIC RSS queue_mapping |
| `PACKET_FANOUT_CBPF` | 6 | classic BPF 程序选择 |
| `PACKET_FANOUT_EBPF` | 7 | eBPF 程序选择 |

### 2.3 flag 常量（`type_flags & ~0x7f`）

| 常量 | 值 | 含义 |
|------|----|------|
| `PACKET_FANOUT_FLAG_ROLLOVER` | 0x1000 | 其他 mode 的 backup：选中的 socket backlog 则滚到下一个 |
| `PACKET_FANOUT_FLAG_UNIQUEID` | 0x2000 | 请求内核分配唯一 id（忽略 value 中的 id 字段） |
| `PACKET_FANOUT_FLAG_IGNORE_OUTGOING` | 0x4000 | 忽略本机发送的包 |
| `PACKET_FANOUT_FLAG_DEFRAG` | 0x8000 | 分片重组后再 fanout |

### 2.4 group 生命周期与校验

- 第一个 socket join 时**隐式创建** group（记录其 protocol、device、mode、flags）。
- 后续 socket join **必须匹配**：相同 protocol、device(ifindex)、socket type、mode、flags，否则 `EINVAL`。
- socket **只能**通过 close 离开 group（无显式 leave）。
- 最后一个 socket close 时**删除** group。
- 已在 group 中的 socket 再次 set `PACKET_FANOUT` → `EBUSY`。
- `getsockopt(PACKET_FANOUT)` 返回当前 group 的 `(id | (type_flags << 16))`。

### 2.5 分发机制（Linux）

Linux 给每个 fanout group 注册一个**独立的** `packet_type` hook（`dev_add_pack`），hook 函数是 `packet_rcv_fanout`。收包时该 hook 调 `fanout_demux_*` 选一个 socket 投递。普通 socket 各有独立 hook，互不影响。

---

## 3. DragonOS 现有代码分析

### 3.1 PacketSocket 结构（`kernel/src/net/socket/packet/mod.rs:72-94`）

16 字段，关键：
- `binding: PacketBinding`：lock-free `(ifindex, protocol)` 快照（AtomicU64）
- `bound_iface: RwSem<Option<Arc<dyn Iface>>>`
- `rx_buffer: Mutex<VecDeque<ReceivedPacket>>` + `rx_buffer_bytes: AtomicUsize` / `recv_buffer_bytes: AtomicUsize`
- `self_ref: Weak<Self>`、`netns: Arc<NetNamespace>`
- `sock_type: PacketSocketType`（Raw/Dgram）

### 3.2 注册表（`net_namespace.rs:82-86, 108-111`）

```rust
packet_sockets: RcuArcSlot<PacketSocketRegistrySnapshot>,  // RCU 无锁读
packet_sockets_writer: Mutex<()>,                           // COW 写串行化
packet_sockets_need_cleanup: AtomicBool,

struct PacketSocketRegistrySnapshot { sockets: Vec<Weak<PacketSocket>> }
```

`RcuArcSlot` API：`new(Arc)` / `load() -> Arc` / `store_deferred(Arc)`。

### 3.3 分发路径

```
deliver_to_packet_sockets(iface, frame, pkt_type)   // mod.rs:174 — 公共入口
  → netns.deliver_to_packet_sockets(ifindex, frame, pkt_type)   // net_namespace.rs:443
      load snapshot → for each socket: socket.deliver(ifindex, frame, pkt_type)
          → deliver_from_iface(ifindex, frame, pkt_type)   // rx.rs:53
              binding 过滤(ifindex/protocol) → 缓冲区检查 → 拷贝帧入 rx_buffer
```

deliver 在 **NAPI poll（进程上下文）**执行，可持 Mutex/RwSem，但应避免无界遍历/分配。

### 3.4 选项处理（`sockopt.rs`）

`set_packet_option(PSOL::PACKET, name, val)`：当前 `PSOL::PACKET` 分支未知 name 默认 `Ok(())`（静默接受）。`PACKET_FANOUT`(18) 需新增显式分支。

### 3.5 关闭路径

`do_close()`（mod.rs:253）→ `close_binding()`（binding.rs:79）→ `unregister_packet_socket`。fanout 离开挂在此路径。

### 3.6 可用基础设施

| 需求 | DragonOS 设施 |
|------|---------------|
| RCU COW 表 | `RcuArcSlot<T>`（`rcu/mod.rs`） |
| flow hash | `jhash::jhash2(&[u32], 0)`（UDP 已用，`udp_bindings.rs:212`） |
| RNG | `crate::libs::rand::rand()` / `soft_rand()` |
| CPU id | `crate::arch::current_cpu_id().data()`（三架构实现） |
| id 分配 | `IdAllocator`（`kernel/crates/ida`，`&mut self`，需外部锁） |

---

## 4. 架构设计

### 4.1 设计原则

1. **不破坏广播语义**：普通（非 fanout）socket 行为零变化。
2. **复用 RcuArcSlot**：fanout group 表用 RCU COW，与 `packet_sockets` 同构，deliver 路径保持无锁读。
3. **复用 deliver_from_iface**：group 选出 socket 后调用现有 `deliver_from_iface`，binding 过滤逻辑不重复。
4. **成员一致性校验**：join 时强制 (sock_type, ifindex, protocol) 一致，保证 demux 选任何成员过滤结果相同。

### 4.2 deliver 路径改造（核心）

```
netns.deliver_to_packet_sockets(ifindex, frame, pkt_type):
    socket_snapshot = packet_sockets.load()
    group_snapshot  = fanout_groups.load()        // 新增，RCU 读
    for weak in socket_snapshot.sockets:
        if let Some(socket) = weak.upgrade():
            if socket.fanout.read().is_some():    // fanout socket 跳过广播
                continue
            socket.deliver(ifindex, frame, pkt_type)
        else: stale = true
    for group in group_snapshot.groups:
        group.deliver(ifindex, frame, pkt_type)   // 每个 group 投递一份
```

`group.deliver()` 内部：load 成员 → demux 选 index → `members[idx].upgrade()` → `socket.deliver()`。成员 upgrade 失败（stale）时跳过并标记清理。

**为什么 fanout socket 从广播跳过**：贴合 Linux「group 有独立 hook」语义——一个 group 只收一份。从广播路径剔除后由独立 group 遍历统一分发，避免 per-call 去重状态。

### 4.3 新增数据结构

文件：`kernel/src/net/socket/packet/fanout.rs`（新模块）

```rust
/// 分发算法
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanoutMode {
    Hash,      // 0
    Lb,        // 1 — round-robin
    Cpu,       // 2
    Rollover,  // 3
    Rnd,       // 4
}

impl FanoutMode {
    /// 从 type_flags 低 7 位解析，不支持 QM/CBPF/EBPF
    pub fn from_raw(raw: u32) -> Result<Self, SystemError> { ... }
    pub fn as_raw(self) -> u32 { ... }
}

/// fanout group
pub struct FanoutGroup {
    pub id: u16,
    pub mode: FanoutMode,
    pub flags: u16,                              // PACKET_FANOUT_FLAG_* 原始值
    /// HASH 模式 flow hash 随机种子（创建时生成，避免 flow polarization）
    hash_seed: u32,
    /// 成员列表，RCU COW（与 packet_sockets 同构）
    members: RcuArcSlot<FanoutMemberSnapshot>,
    members_writer: Mutex<()>,
    /// LB / ROLLOVER 模式 round-robin 计数器
    rr_counter: AtomicU32,
}

#[derive(Default)]
struct FanoutMemberSnapshot {
    members: Vec<Weak<PacketSocket>>,
}
```

### 4.4 netns 字段新增（`net_namespace.rs`）

```rust
/// fanout group 表，RCU 无锁读（deliver 路径）
fanout_groups: RcuArcSlot<FanoutGroupRegistry>,
/// COW 写串行化 + 保护 id 分配器
fanout_groups_writer: Mutex<FanoutGroupWriter>,
fanout_groups_need_cleanup: AtomicBool,

#[derive(Default)]
struct FanoutGroupRegistry {
    groups: Vec<Arc<FanoutGroup>>,   // 读路径顺序扫描，成员少
}

struct FanoutGroupWriter {
    /// group id → Arc<FanoutGroup> 的权威索引（写路径）
    by_id: BTreeMap<u16, Arc<FanoutGroup>>,
    /// UNIQUEID 自动分配器
    id_alloc: IdAllocator,
}
```

> 写路径重建 `FanoutGroupRegistry`（Vec）并 `store_deferred`，与 `packet_sockets` 完全同构。`by_id` 仅写路径持有，保证 id 唯一与 O(log n) 查找。

### 4.5 PacketSocket 字段新增（`mod.rs`）

```rust
/// fanout group 成员关系，None=普通广播 socket
pub(super) fanout: RwSem<Option<Arc<FanoutGroup>>>,
```

`new()` 初始化为 `RwSem::new(None)`。

### 4.6 uapi 常量（`packet/uapi.rs`）

```rust
pub mod packet_option {
    // ... 已有 ...
    pub const PACKET_FANOUT: usize = 18;
    pub const PACKET_FANOUT_DATA: usize = 22;
}

pub mod fanout_mode {
    pub const PACKET_FANOUT_HASH: u32 = 0;
    pub const PACKET_FANOUT_LB: u32 = 1;
    pub const PACKET_FANOUT_CPU: u32 = 2;
    pub const PACKET_FANOUT_ROLLOVER: u32 = 3;
    pub const PACKET_FANOUT_RND: u32 = 4;
    pub const PACKET_FANOUT_QM: u32 = 5;     // 不支持
    pub const PACKET_FANOUT_CBPF: u32 = 6;   // 不支持
    pub const PACKET_FANOUT_EBPF: u32 = 7;   // 不支持
}

pub mod fanout_flag {
    pub const PACKET_FANOUT_FLAG_ROLLOVER: u16 = 0x1000;
    pub const PACKET_FANOUT_FLAG_UNIQUEID: u16 = 0x2000;
    pub const PACKET_FANOUT_FLAG_IGNORE_OUTGOING: u16 = 0x4000;  // 不支持
    pub const PACKET_FANOUT_FLAG_DEFRAG: u16 = 0x8000;           // 不支持
}
```

---

## 5. 关键流程

### 5.1 set_option(PACKET_FANOUT) — `fanout.rs:join_fanout`

```
1. 解析 val(i32): id = (val as u32) & 0xffff; type_flags = (val as u32) >> 16
   mode = type_flags & 0x7f; flags = (type_flags & !0x7f) as u16
2. mode = FanoutMode::from_raw(mode)?     // QM/CBPF/EBPF → EINVAL
3. flags 校验：仅允许 ROLLOVER(0x1000) | UNIQUEID(0x2000)；其余 → EINVAL
4. 若 self.fanout.read().is_some() → EBUSY   // 已在 group
5. 取 netns fanout_groups_writer 锁（与 leave 互斥）：
   a. 确定 group id：UNIQUEID → id_alloc.alloc()（满 → ENOMEM）
                    否则用 val 中的 id（若已被占用且参数不匹配 → EINVAL）
   b. 查 by_id[id]：
      - 存在 → 校验 mode/flags/(sock_type,ifindex,protocol) 一致，不一致 → EINVAL
      - 不存在 → 创建 FanoutGroup（生成随机 hash_seed），加入 by_id
   c. group.add_member(socket.self_ref)   // 先加成员（RCU publish 新快照）
   d. socket.fanout.write() = Some(Arc::clone(group))  // 后设字段
   e. 重建 registry store_deferred
6. 返回 Ok(())

> **join 操作顺序**：先 add_member（RCU publish）再设 fanout 字段，使二者在 writer 锁内完成。deliver 读 group 快照与 fanout 字段之间的瞬态窗口最多丢一个 join 期帧（与 Linux hook 切换瞬态等价，可接受）。
```

**一致性校验项**：`self.sock_type`、`self.binding.load()` 的 `(ifindex, protocol)` 必须与 group 创建者相同。保证 demux 选任何成员，`deliver_from_iface` 过滤结果一致。

### 5.2 close 路径 — `fanout.rs:leave_fanout`

在 `close_binding()`（binding.rs）中，`unregister_packet_socket` 之后调用 `self.leave_fanout()`：

```
1. group = self.fanout.write().take()?  // 取出，None 则返回
2. 取 netns fanout_groups_writer 锁（与 join 互斥，消除 TOCTOU）：
   a. group.remove_member(&self.self_ref)     // RCU COW 移除
   b. 在 writer 锁内 load group 成员数（join 无法插入）；
      为空 → by_id.remove(group.id) → id_alloc.free(id)
   c. 重建 registry store_deferred
```

> **TOCTOU 修复**：「检查成员为空」与「从 by_id 删除」必须同在 writer 锁内原子完成。否则并发 join 可在空窗内注入成员，导致其 socket 指向已删除的 group（zombie，永不为 deliver 所见）。

### 5.3 group.deliver — 分发

```
1. members = self.members.load()
2. live = 成员列表过滤 upgrade 成功的（惰性清理 stale）；stale → 标记 need_cleanup
3. 若 live 为空 → 返回
4. base = match mode {
     Hash     → flow_hash(frame, self.hash_seed) % live.len()
     Lb       → self.rr_counter.fetch_add(1, Relaxed) as usize % live.len()
     Cpu      → current_cpu_id().data() as usize % live.len()
     Rnd      → rand() as usize % live.len()
     Rollover → self.rr_counter.fetch_add(1, Relaxed) as usize % live.len() // base 起点
   }
5. // ROLLOVER 语义：选中 socket backlog 则滚到下一个有空间的
   let rollover = mode == Rollover || (self.flags & FLAG_ROLLOVER != 0)
   for offset in 0..live.len() {
       idx = (base + offset) % live.len()
       if !rollover || live[idx].rx_has_room() {   // 非 rollover 直接投；rollover 找有空间的
           live[idx].deliver(ifindex, frame, pkt_type)
           return
       }
   }
   // 全部满（仅 rollover 路径）：投给 base，由 deliver_from_iface 记 drop
   live[base].deliver(ifindex, frame, pkt_type)
```

> rr_counter 用 fetch_add 保证多核并发递增无竞争；HASH/RND/CPU 无共享状态。
> rx_has_room() 为 PacketSocket 新增的 pub(super) 方法：rx_buffer_bytes.load(Acquire) < recv_buffer_bytes.load(Relaxed)（复用 rx.rs:74 既有判断）。

### 5.4 ROLLOVER 语义（mode 与 FLAG_ROLLOVER）

- **mode=ROLLOVER**：从 rr_counter 推进的 base 起点顺序找第一个 rx_has_room() 的成员投递。
- **FLAG_ROLLOVER（其他 mode）**：demux 选 base 后，若 backlog 则滚到下一个有空间的成员。两种语义在 5.3 步骤 5 统一实现。
- 全部满时投给 base，由 deliver_from_iface 记 stats_drops（保丢包统计语义）。

### 5.5 flow_hash（HASH 模式）

解析以太网帧（复用 rx.rs:parse_frame 的 dst/src/protocol 解析，再下钻 L3/L4）：
- Ethernet 14B 头后是 IP：若 IPv4 取 src/dst（各 4B），IPv6 取 src/dst（各 16B）。
- L4：若 TCP/UDP 取 src/dst port（各 2B）。
- 拼 [src_ip_words..., dst_ip_words..., src_port, dst_port] → jhash2(&words, self.hash_seed)。
- 非 IP 帧 → hash=0（全部走 base socket，退化但不丢）。
- hash_seed 在 group 创建时随机生成（rand()），避免固定 seed 导致 flow polarization。

---

## 6. getsockopt(PACKET_FANOUT)

```
未加入 group → 返回成功，写回 0（Linux packet_getsockopt：po->fanout 为空时 val=0）
已加入 group → 写回 (group.id as u32) | ((mode.as_raw() as u32 | flags as u32) << 16)，返回 4 字节
```

---

## 7. scope 与不支持项

| 特性 | 处理 | 理由 |
|------|------|------|
| QM(5)/CBPF(6)/EBPF(7) | `EINVAL` | 需 NIC RSS queue_mapping / BPF 基础设施，DragonOS 暂无 |
| FLAG_DEFRAG | `EINVAL` | 需 IP 分片重组，DragonOS 暂无 |
| FLAG_IGNORE_OUTGOING | `EINVAL` | DragonOS 不向 packet socket 回送出站帧，语义空缺 |
| `fanout_args`（struct）传参 | 不支持 | Linux 新接口，传统 int 编码已覆盖主流用例 |

> 对不支持项返回 `EINVAL` 而非静默忽略：避免用户误以为已生效。

---

## 8. 并发与正确性

| 场景 | 保证 |
|------|------|
| deliver 与 join/remove 并发 | deliver 持 RCU 读快照，join/remove 走 COW + store_deferred，读者看到一致快照 |
| LB/ROLLOVER rr_counter 多核竞争 | fetch_add 原子，无丢失 |
| group 空时删除与并发 deliver | deliver 持旧快照（Arc 存活），删除只重建 registry；旧 group Arc 在快照释放后回收 |
| **leave TOCTOU** | 「检查成员空 + 删除 group」整体在 fanout_groups_writer 锁内，与 join 互斥，无 zombie |
| socket close 与 deliver 竞争 | 与现有 packet_sockets 相同：Weak.upgrade 决定，失败标记惰性清理 |
| join 一致性校验 | writer 锁内完成「查找/创建 + 加成员 + 设字段」原子序列 |
| join 瞬态窗口 | add_member 先于设 fanout。**已有 group**：release-acquire 下仅可能重复一帧（广播路径见 fanout=None 仍投 + group 路径见成员也投），不可能丢失——重复比丢失友好，用户态去重即可。**新 group**：registry publish 前被广播跳过，最多丢一帧。两者均等价 Linux hook 切换瞬态，bounded、可接受。消除该窗口需两套状态（fanout 字段 + 成员列表）原子切换，成本不划算。 |

---

## 9. 文件改动清单

| 文件 | 改动 |
|------|------|
| `kernel/src/net/socket/packet/uapi.rs` | 新增 fanout mode/flag 常量、PACKET_FANOUT/PACKET_FANOUT_DATA option 常量 |
| `kernel/src/net/socket/packet/fanout.rs` | **新建**：FanoutMode、FanoutGroup、join/leave/deliver/demux/flow_hash |
| `kernel/src/net/socket/packet/mod.rs` | PacketSocket 加 `fanout` 字段；mod 声明 fanout；new() 初始化；deliver_to_packet_sockets 不变（分发改造在 netns） |
| `kernel/src/net/socket/packet/sockopt.rs` | set/get 的 `PSOL::PACKET` 分支加 `PACKET_FANOUT` 处理 |
| `kernel/src/net/socket/packet/binding.rs` | `close_binding()` 末尾调 `leave_fanout()` |
| `kernel/src/process/namespace/net_namespace.rs` | 加 fanout_groups 注册表字段；deliver_to_packet_sockets 增加 group 分发遍历；join/leave/remove 的 netns 方法 |

---

## 10. 实施步骤

### Task 1: uapi 常量
新增 `PACKET_FANOUT`(18)、`PACKET_FANOUT_DATA`(22) option，`fanout_mode::*`、`fanout_flag::*` 模块。
验证：`make kernel` 编译通过。

### Task 2: FanoutMode + FanoutGroup 骨架（fanout.rs）
定义 `FanoutMode`（含 from_raw/as_raw）、`FanoutGroup`、`FanoutMemberSnapshot`。实现 `add_member`/`remove_member`（RCU COW）。
验证：编译通过。

### Task 3: netns 注册表
netns 加 `fanout_groups`/`fanout_groups_writer`/`fanout_groups_need_cleanup`；初始化；`join_fanout_group`/`leave_fanout_group`/`cleanup_fanout_groups` 方法。
验证：编译通过。

### Task 4: PacketSocket 字段 + join/leave
PacketSocket 加 `fanout` 字段，new() 初始化。实现 `join_fanout`（一致性校验）、`leave_fanout`。
验证：编译通过。

### Task 5: deliver 分发
netns.deliver_to_packet_sockets 增加 group 遍历；FanoutGroup::deliver + 各 mode demux + flow_hash。
验证：编译通过。

### Task 6: sockopt 接入
sockopt.rs 的 set/get 加 PACKET_FANOUT 分支；close_binding 加 leave_fanout。
验证：`make kernel` 编译通过。

### Task 7: 测试
编写用户态测试程序（`user/apps/c_unitest` 或独立），验证：
- LB 模式 round-robin 分发（多 socket 收到不同包）
- HASH 模式同流到同 socket
- group 一致性校验（错误 mode/protocol → EINVAL）
- close 后 group 删除
验证：QEMU 内运行测试通过。

---

## 11. 风险与回归检查

- **广播回归**：普通 socket（fanout=None）deliver 路径仅多一次 `fanout.read()` 判断，逻辑不变。
- **性能**：deliver 多一次 group_snapshot.load()（RCU 读，无锁）+ group 遍历；group 数量通常极少。
- **内存**：FanoutGroup Arc 在 group 删除后随快照释放回收，无泄漏。
- **EBUSY/EINVAL 语义**：严格对齐 Linux man page，用 gvisor/真实程序验证错误码。
