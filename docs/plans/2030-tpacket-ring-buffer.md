# TPACKET mmap Ring Buffer 实施计划 (Issue #2030) — v2

> **状态**: 评审修正完成，待实施
> **Issue**: [#2030](https://github.com/DragonOS-Community/DragonOS/issues/2030)
> **目标兼容**: Linux 6.6 语义
> **分支**: `issue-2030-tpacket-ring` (基于 origin/master @ 4831e33a)
>
> **v2 变更**: 纳入对抗性评审的 8 项修正（2 P0 + 2 P1 + 3 P2 + 1 P3），详见 §10。

---

## 1. 目标

为 DragonOS AF_PACKET socket 实现 TPACKET mmap ring buffer，使 `tcpdump`/`libpcap` 能通过零拷贝环形缓冲区高效抓包。

**本次实施范围（Phase 1）**: TPACKET V1 + V2 的 `PACKET_RX_RING` + `mmap` + `poll`/`epoll` 唤醒 + `PACKET_VERSION` + `PACKET_STATISTICS` + `PACKET_RESERVE` + `PACKET_HDRLEN`。

**后续阶段**: Phase 2 = V3（block 模型）; Phase 3 = `PACKET_TX_RING`。本次不实施但预留扩展点。

---

## 2. Linux TPACKET 语义参考

> 来自 `include/uapi/linux/if_packet.h`（已核实）。

### 2.1 Socket Option 编号

| 选项 | 值 | 本次 |
|------|---|------|
| `PACKET_RX_RING` | 5 | ✅ |
| `PACKET_STATISTICS` | 6 | ✅ |
| `PACKET_COPY_THRESH` | 7 | ✅（存储值） |
| `PACKET_VERSION` | 10 | ✅ |
| `PACKET_HDRLEN` | 11 | ✅（只读） |
| `PACKET_RESERVE` | 12 | ✅ |
| `PACKET_TX_RING` | 13 | ⏳ Phase 3 |
| `PACKET_LOSS` | 14 | ⏳ Phase 2 |

uapi.rs 已有部分常量，**需补充**: `PACKET_RX_RING=5`, `PACKET_HDRLEN=11`, `PACKET_TX_RING=13`, `PACKET_LOSS=14`, `PACKET_RECV_OUTPUT=3`。

### 2.2 帧状态标志

- `TP_STATUS_KERNEL` = 0（内核可写）— **uapi.rs 需补充**
- `TP_STATUS_USER` = 1（用户可读）— 已定义

### 2.3 核心数据结构（`#[repr(C)]`）

```
TPACKET_ALIGNMENT = 16
TPACKET_ALIGN(x) = (x + 15) & !15
```

**tpacket_hdr (V1)** — 28 字节:
```
tp_status: u64(unsigned long), tp_len: u32, tp_snaplen: u32,
tp_mac: u16, tp_net: u16, tp_sec: u32, tp_usec: u32
```
`TPACKET_HDRLEN = TPACKET_ALIGN(28) + 20 = 52`

**tpacket2_hdr (V2)** — 32 字节:
```
tp_status: u32, tp_len: u32, tp_snaplen: u32, tp_mac: u16, tp_net: u16,
tp_sec: u32, tp_nsec: u32, tp_vlan_tci: u16, tp_vlan_tpid: u16, tp_padding: [u8;4]
```
`TPACKET2_HDRLEN = TPACKET_ALIGN(32) + 20 = 52`

**tpacket_req** (V1/V2): `tp_block_size: u32, tp_block_nr: u32, tp_frame_size: u32, tp_frame_nr: u32`

**tpacket_stats**: `tp_packets: u32, tp_drops: u32`

### 2.4 V1/V2 Ring 内存布局

Ring = 连续 frame 数组，每个 frame = `tp_frame_size` 字节。

**frame 内部**:
```
[0 .. tp_hdrlen):           header region (tpacketN_hdr + sockaddr_ll 对齐区)
[tp_hdrlen .. tp_mac):      gap
[tp_mac .. tp_net):         MAC header (仅 SOCK_RAW)
[tp_net .. tp_net+snaplen): 数据
[tp_net+snaplen .. frame_size): padding
```

**tp_mac / tp_net**（对照 Linux `tpacket_rcv`，已核实精确公式）:

Linux 公式（以太网）: `netoff = TPACKET_ALIGN(tp_hdrlen + max(maclen, 16)) + tp_reserve`
- SOCK_RAW (maclen=14, reserve=0): `netoff = TPACKET_ALIGN(52+16) = 80`, **tp_net=80, tp_mac=66**（80-14）
  - VLAN: maclen=18, `netoff = TPACKET_ALIGN(52+18) = 80`（已对齐）, tp_mac=62, tp_net=80
- SOCK_DGRAM (无 MAC): **tp_mac = tp_net = 80**

**关键**: tp_net 必须 16 字节对齐（Linux 保证）。tp_reserve 折入 netoff 计算。帧内数据写在 tp_mac 偏移处。libpcap 用 `(char*)hdr + hdr->tp_mac` 定位数据。

### 2.5 Ring 配置验证规则（Linux `packet_setring`）

1. `tp_block_size > 0` 且 `PAGE_SIZE` 对齐
2. `min_frame_size = tp_hdrlen + tp_reserve`
3. `tp_frame_size >= min_frame_size`
4. `tp_frame_size` 是 `TPACKET_ALIGNMENT`(16) 的倍数
5. `frames_per_block = tp_block_size / tp_frame_size > 0`
6. `frames_per_block * tp_block_nr == tp_frame_nr`
7. ring 已 mmap 或有 pending 包时重新设置 → `EBUSY`

---

## 3. DragonOS 现有架构分析

### 3.1 PacketSocket 现状

**核心文件**: `kernel/src/net/socket/packet/`（mod.rs, binding.rs, rx.rs, tx.rs, sockopt.rs, uapi.rs）

**接收路径** (`rx.rs:53-152`): `deliver_from_iface(ifindex, frame, pkt_type)` 是唯一投递入口 → 绑定过滤 → 帧解析 → 内存预算 → 复制到 Vec → 推入 `rx_buffer` 队列 → 唤醒 `wait_queue`。

**poll/epoll 路径**（**关键，评审 P0 发现**）:
- `PollableInode::poll()`（inode.rs:430）→ `check_io_event()`（mod.rs:274）→ `packet_io_event()`（binding.rs:100-108）→ `can_recv()`（rx.rs:153-155）
- `can_recv()` **只检查** `rx_buffer.is_empty()`
- epoll `ep_send_events` Phase 2 对每个 ready_list epitem 调用 `ep_item_poll()` 重新检查就绪状态；若 `poll()` 返回空则跳过事件
- **epoll 唤醒 API**: `EventPoll::wakeup_epoll(self.epoll_items.as_ref(), events)`（**非** `epoll_items.trigger()`，后者不存在）

### 3.2 mmap 机制

DragonOS mmap = **lazy VMA + page fault** 模型:
1. mmap 系统调用（`address_space.rs`）创建 VMA，调用 `inode.mmap_file()` hook
2. `mmap_file` 默认调 `mmap()` 返回 `ENOSYS`，被忽略（继续 page fault 模型）
3. page fault: `inode().fs().map_pages()` + `inode().page_cache()`
4. `filemap_map_pages`（fault.rs:810）从 page_cache 获取物理页映射
5. `filemap_fault`（fault.rs:887）**检查 `metadata().size`**，size=0 返回 SIGBUS

**Socket blanket impl 约束** (`inode.rs:285`): `fs()` = `unreachable!()`，`page_cache()` = 默认 None，`metadata().size` = 0。ioctl 已有转发模式（IndexNode::ioctl → Socket::ioctl）。

### 3.3 参考实现

`perf/bpf.rs` + `perf/mod.rs`: page_cache + fake fs 模式。`do_mmap` 分配物理页 → `insert_ready_page` → `phys_2_virt` 内核写入。`PerfFakeFs` 的 fault/map_pages 调用 `PageFaultHandler::filemap_fault/filemap_map_pages`。

---

## 4. 架构设计

### 4.1 核心决策

**决策 1: mmap 通过 page_cache + fake fs 模型**（与 perf 一致，复用现有 page fault 机制）。

**决策 2: Socket trait 转发突破 blanket impl**。参考 ioctl 转发，新增 `mmap_layout()` 方法返回包含 page_cache + fs + size 的结构体（**评审 P3 修正**：合并为单一方法，一次锁定返回所有信息，减少 page fault 期间多次锁 rx_ring）。

**决策 3: 接收路径双模式**。ring 激活时写 ring 帧；未激活走现有 rx_buffer 路径。向后兼容。

### 4.2 Socket trait 改动（评审修正版）

`base.rs` Socket trait 新增 **2 个方法**:

```rust
/// mmap 布局信息（page_cache + fake fs + size），用于支持 mmap 的 socket。
/// 默认 None：大多数 socket 不支持 mmap。一次调用返回所有信息，避免多次锁。
fn mmap_layout(&self) -> Option<crate::net::socket::packet::ring::PacketMmapLayout> {
    None
}
```

`inode.rs` blanket impl 新增/修改:

```rust
// 新增: page_cache 转发
fn page_cache(&self) -> Option<Arc<PageCache>> {
    Socket::mmap_layout(self).map(|l| l.page_cache)
}

// 修改: fs 不再无条件 panic
fn fs(&self) -> Arc<dyn FileSystem> {
    match Socket::mmap_layout(self) {
        Some(l) => l.fs,
        None => unreachable!("Socket does not have a file system"),
    }
}

// 修改: metadata 支持 mmap size
fn metadata(&self) -> Result<Metadata, SystemError> {
    let mut md = Metadata::new(FileType::Socket, InodeMode::from_bits_truncate(0o755));
    md.inode_id = self.socket_inode_id();
    md.mode |= InodeMode::S_IFSOCK;
    if let Some(layout) = Socket::mmap_layout(self) {
        md.size = layout.size as i64;
    }
    Ok(md)
}

// 新增: check_mmap_file — 无 ring 时拒绝 mmap（评审 P1 修正）
fn check_mmap_file(&self, _file: &Arc<File>, _len: usize, _offset: usize, _vm_flags: VmFlags)
    -> Result<(), SystemError>
{
    if Socket::mmap_layout(self).is_none() {
        return Err(SystemError::EINVAL);  // ring 未创建，拒绝 mmap
    }
    Ok(())
}
```

**`PacketMmapLayout` 结构体**（`ring.rs`）:
```rust
pub struct PacketMmapLayout {
    pub page_cache: Arc<PageCache>,
    pub fs: Arc<dyn FileSystem>,
    pub size: usize,
}
```

> **check_mmap_file 时机**: address_space.rs mmap 流程在创建 VMA **之前**调用 `check_mmap_file`（参考 overlayfs/file.rs:189）。若返回 EINVAL，mmap 直接失败，不会创建 lazy VMA，不会触发后续 page fault → panic。这解决了评审 P1 的"无 ring 时 mmap panic"问题。

### 4.3 新增文件: `packet/ring.rs`

```rust
/// TPACKET 版本
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TpacketVersion { V1 = 1, V2 = 2 }

impl TpacketVersion {
    pub fn hdrlen(&self) -> usize { match self { V1 => 52, V2 => 52 } }
}

/// mmap 布局信息（§4.2）
pub struct PacketMmapLayout {
    pub page_cache: Arc<PageCache>,
    pub fs: Arc<dyn FileSystem>,
    pub size: usize,
}

/// Ring 配置
#[derive(Debug, Clone, Copy)]
pub struct RingConfig {
    pub block_size: usize, pub block_nr: usize,
    pub frame_size: usize, pub frame_nr: usize,
}

/// Ring 运行时状态
pub struct PacketRing {
    config: RingConfig,
    version: TpacketVersion,
    block_vaddrs: Vec<usize>,   // per-block 内核虚址（评审 P2-4: per-block 分配）
    total_size: usize,
    page_cache: Arc<PageCache>,
    frames_per_block: usize,    // block_size / frame_size，用于帧地址计算
    head: u32,                   // 下一个写入帧索引（Mutex 保护）
    reserve: usize,
}

/// 写帧结果
pub enum RingWriteResult { Written { frame_index: u32 }, Dropped }
```

**关键方法**:

- `PacketRing::setup(config, version, reserve)`: 分配物理页、创建 PageCache、清零内存
- `PacketRing::write_frame(&mut self, frame, metadata, ifindex, pkt_type) -> RingWriteResult`: 写入帧
- `PacketRing::has_user_frames(&self) -> bool`: 检查是否有 TP_STATUS_USER 帧（**评审 P0 修正**：供 can_recv 使用）
- `validate_ring_config(req, hdrlen, reserve) -> Result<RingConfig>`: 参数校验

---

## 5. 实施步骤

### Task 1: UAPI 结构体与常量

**File**: `uapi.rs`

补充常量（PACKET_RX_RING=5 等）+ `#[repr(C)]` 结构体（TpacketReq, TpacketHdr, Tpacket2Hdr, TpacketStats）+ TP_STATUS_KERNEL=0 + TPACKET_ALIGNMENT/ALIGN/HDRLEN。

**验证**: `cargo build`

### Task 2: Socket trait mmap 转发 + check_mmap_file 防护

**Files**: `base.rs`（Socket trait 新增 `mmap_layout()`）, `inode.rs`（blanket impl 转发 page_cache/fs/metadata + check_mmap_file）

详见 §4.2 代码。

**验证**: `cargo build`（默认行为不变）

### Task 3: Ring buffer 核心

**File**: 新建 `ring.rs`, 修改 `mod.rs`（`mod ring; pub use ring::PacketMmapLayout;`）

**`PacketRing::setup`**:
```rust
pub fn setup(config: RingConfig, version: TpacketVersion, reserve: usize)
    -> Result<(Self, Arc<PageCache>, Arc<dyn FileSystem>), SystemError>
{
    let total_size = config.block_nr * config.block_size;
    let pages_per_block = config.block_size / PAGE_SIZE;
    let page_cache = Arc::new(PageCache::new(None, None));
    let mut block_vaddrs = Vec::with_capacity(config.block_nr);
    let mut pm = page_manager_lock();
    // per-block 分配（与 Linux alloc_pg_vec 一致）：每个 block 独立分配 block_size 连续物理页，
    // 避免单次 block_nr*block_size 大连续分配在内存碎片化时 ENOMEM（评审 P2-4 修正）。
    for block_idx in 0..config.block_nr {
        let (phy_addr, pages) = pm.create_pages(
            PageType::Normal, PageFlags::PG_UNEVICTABLE,
            &mut LockedFrameAllocator, PageFrameCount::new(pages_per_block))?;
        for j in 0..pages.len() {
            let page = pages.get(j).unwrap();
            page.write().add_flags(PageFlags::PG_UPTODATE);
            page_cache.insert_ready_page(block_idx * pages_per_block + j, page.clone())?;
        }
        let vaddr = unsafe { MMArch::phys_2_virt(phy_addr) }.ok_or(SystemError::EFAULT)?.data();
        // 清零：TP_STATUS_KERNEL=0，清零后所有帧默认 KERNEL
        unsafe { core::ptr::write_bytes(vaddr as *mut u8, 0, config.block_size); }
        block_vaddrs.push(vaddr);
    }
    Ok((PacketRing { config, version, block_vaddrs, total_size,
         page_cache: page_cache.clone(),
         frames_per_block: config.block_size / config.frame_size,
         head: 0, reserve },
        page_cache, Arc::new(PacketFakeFs)))
}
```

**`frame_addr`**（per-block 帧地址计算）:
```rust
/// 通过帧索引计算内核虚拟地址。per-block 分配后不同 block 物理不连续，
/// 但同一 block 内连续。frame_idx → block_idx + block 内偏移。
fn frame_addr(&self, frame_idx: usize) -> usize {
    let block_idx = frame_idx / self.frames_per_block;
    let block_offset = (frame_idx % self.frames_per_block) * self.config.frame_size;
    self.block_vaddrs[block_idx] + block_offset
}
```
**`write_frame`**（**评审 P2 修正：VLAN 剥离 + tp_status 版本原子写**）:

> **并发模型**（评审 P2 修正）: write_frame 在调用者持有的 `rx_ring` Mutex 保护下执行，与 Linux `sk_receive_queue.lock` 串行化语义一致。head 是普通 u32（Mutex 保护，非原子）。

```rust
/// 写入一帧。调用者必须持有 rx_ring Mutex。
pub fn write_frame(&mut self, frame: &[u8], meta: &PacketMetadata,
                   ifindex: u32, pkt_type: PacketType) -> RingWriteResult {
    let hdrlen = self.version.hdrlen();
    let data_cap = self.config.frame_size.saturating_sub(hdrlen);
    if data_cap == 0 { return RingWriteResult::Dropped; }

    // 查找 KERNEL 帧（从 head 开始扫描）
    let mut found = None;
    for i in 0..self.config.frame_nr {
        let idx = (self.head as usize + i) % self.config.frame_nr;
        let frame_base = self.frame_addr(idx);
        if self.read_tp_status(frame_base) == TP_STATUS_KERNEL {
            found = Some((idx, frame_base));
            break;
        }
    }
    let (idx, frame_base) = match found { Some(x) => x, None => return RingWriteResult::Dropped };

    // VLAN 处理（评审 P2 修正）
    let (data_src, mac_len, net_off_in_frame) = self.prepare_data(frame, meta, hdrlen);

    let snaplen = data_src.len().min(data_cap);
    self.fill_header(frame_base, meta, ifindex, pkt_type, snaplen,
                     frame.len(), mac_len, net_off_in_frame, hdrlen);

    // 复制数据到帧数据区
    unsafe {
        core::ptr::copy_nonoverlapping(
            data_src.as_ptr(),
            (frame_base + net_off_in_frame) as *mut u8,
            snaplen);
    }

    // 最后翻转 tp_status KERNEL→USER（Release 序，保证数据可见性）
    self.publish_frame(frame_base);  // AtomicU64(V1) 或 AtomicU32(V2), Release

    self.head = ((idx + 1) % self.config.frame_nr) as u32;
    RingWriteResult::Written { frame_index: idx as u32 }
}
```

**`prepare_data`**（VLAN 剥离，评审 P2 修正）:
```rust
/// 返回 (数据切片, mac_len, 数据在帧中的起始偏移)
fn prepare_data<'a>(&self, frame: &'a [u8], meta: &PacketMetadata, hdrlen: usize)
    -> (&'a [u8], usize, usize)
{
    let is_vlan = meta.vlan_tpid != 0;
    let mac_len = if is_vlan { 18 } else { 14 };
    // Linux 公式（评审 P2-2 修正）: netoff = TPACKET_ALIGN(hdrlen + max(maclen,16)) + reserve
    // 保证 tp_net 16 字节对齐。原方案 tp_mac=52/tp_net=66 是错误的。
    let netoff = TPACKET_ALIGN(hdrlen + core::cmp::max(mac_len, 16)) + self.reserve;

    if self.sock_type_raw {
        let tp_mac = netoff - mac_len;  // MAC header 在 tp_mac，网络头在 tp_net=netoff
        if is_vlan {
            // VLAN tag 剥离: 数据 = frame[0..12] + frame[16..]，拷贝时跳过 frame[12..16]
        }
        (frame, mac_len, tp_mac)  // 数据（含 MAC）写在 tp_mac 偏移处
    } else {
        // DGRAM: 无 MAC，tp_mac == tp_net == netoff
        let start = if is_vlan { 18 } else { 14 };
        (&frame[start..], 0, netoff)
    }
}
```

> **V2 VLAN header**: fill_header 中若 is_vlan，设置 tp_vlan_tci, tp_vlan_tpid, tp_status |= TP_STATUS_VLAN_VALID | TP_STATUS_VLAN_TPID_VALID。

**tp_status 版本相关原子写**（评审 P2 修正）:
```rust
fn publish_frame(&self, frame_base: usize) {
    match self.version {
        TpacketVersion::V1 => {
            // V1 tp_status 是 u64 (unsigned long on x86_64)
            let status = unsafe { &*(frame_base as *const AtomicU64) };
            status.store(TP_STATUS_USER as u64, Ordering::Release);
        }
        TpacketVersion::V2 => {
            let status = unsafe { &*(frame_base as *const AtomicU32) };
            status.store(TP_STATUS_USER, Ordering::Release);
        }
    }
}
```

**`has_user_frames`**（评审 P0 修正）:
```rust
/// 检查 ring 中是否有 TP_STATUS_USER 帧（供 can_recv 使用）
pub fn has_user_frames(&self) -> bool {
    for i in 0..self.config.frame_nr {
        let frame_base = self.frame_addr(i);
        if self.read_tp_status(frame_base) == TP_STATUS_USER {
            return true;
        }
    }
    false
}
```

> has_user_frames 是 O(N) 扫描，但仅在 poll/epoll readiness 检查时调用（非热路径），可接受。优化：维护一个 AtomicU32 计数 user_frame_count。

**`PacketFakeFs`**:
```rust
pub struct PacketFakeFs;
impl FileSystem for PacketFakeFs {
    unsafe fn fault(&self, pfm) -> VmFaultReason { PageFaultHandler::filemap_fault(pfm) }
    unsafe fn map_pages(&self, pfm, s, e) -> VmFaultReason { PageFaultHandler::filemap_map_pages(pfm, s, e) }
    // 【关键 P1-1】RX ring 是 MAP_SHARED|PROT_WRITE。用户写 tp_status 翻回 KERNEL 时，
    // filemap_map_pages 对共享可写页设只读 PTE → 写保护异常 → do_shared_fault →
    // fs.page_mkwrite()。默认返回 SIGBUS（vfs/mod.rs:1884），导致 ring 完全不可写。
    // 必须实现 page_mkwrite → filemap_page_mkwrite 使写入合法。
    unsafe fn page_mkwrite(&self, pfm: &mut PageFaultMessage) -> VmFaultReason { PageFaultHandler::filemap_page_mkwrite(pfm) }
    // root_inode 等 → 参照 PerfFakeFs 返回合理值
}
```

**验证**: `cargo build`

### Task 4: PacketSocket 集成 ring

**File**: `mod.rs`

1. 新增字段:
```rust
rx_ring: Mutex<Option<Arc<Mutex<PacketRing>>>>,  // Arc<Mutex> 允许 deliver 时 clone 后释放外层锁
tpacket_version: Mutex<TpacketVersion>,
tp_reserve: AtomicU32,
```

> **ring 存储**: `Arc<Mutex<PacketRing>>`。deliver_from_iface clone Arc，释放 rx_ring 外层 Mutex，再 lock 内层 Mutex 执行 write_frame。这样外层 Mutex 只保护 setup/teardown，不阻塞并发 deliver。

2. 实现 `mmap_layout`:
```rust
fn mmap_layout(&self) -> Option<PacketMmapLayout> {
    let ring = self.rx_ring.lock();
    ring.as_ref().map(|r| {
        let inner = r.lock();
        PacketMmapLayout {
            page_cache: inner.page_cache.clone(),
            fs: Arc::new(PacketFakeFs),
            size: inner.total_size,
        }
    })
}
```

**验证**: `cargo build`

### Task 5: 接收路径 + poll/epoll 集成（评审 P0 核心修正）

**File**: `rx.rs`, `binding.rs`

**5a. 修改 `can_recv`**（rx.rs:153）:
```rust
pub(super) fn can_recv(&self) -> bool {
    // ring 模式：检查 ring 中是否有 USER 帧
    let ring = self.rx_ring.lock();
    if let Some(r) = ring.as_ref() {
        return r.lock().has_user_frames();
    }
    drop(ring);
    // 队列模式（现有逻辑）
    !self.rx_buffer.lock().is_empty()
}
```

> `packet_io_event()`（binding.rs:100）无需修改——它已调用 `can_recv()`，can_recv 更新后自动生效。

**5b. 修改 `deliver_from_iface`**（rx.rs:53，在绑定过滤和帧解析之后，内存预算检查之前）:
```rust
// ring 路径
let ring_arc = {
    let ring = self.rx_ring.lock();
    ring.as_ref().cloned()  // clone Arc，立即释放外层锁
};
if let Some(ring_arc) = ring_arc {
    let mut ring = ring_arc.lock();
    match ring.write_frame(frame, &metadata, ifindex, pkt_type) {
        RingWriteResult::Written { .. } => {
            self.stats_packets.fetch_add(1, Ordering::Relaxed);
            drop(ring);
            // 唤醒 wait_queue
            self.wait_queue.wakeup(None);
            // 唤醒 epoll/fasync（评审 P0 修正：用正确的 API）
            let _ = EventPoll::wakeup_epoll(
                self.epoll_items.as_ref(),
                EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
            );
            self.fasync_items.send_sigio(FASYNC_POLL_IN);
        }
        RingWriteResult::Dropped => {
            self.stats_drops.fetch_add(1, Ordering::Relaxed);
        }
    }
    return;
}
// 现有 rx_buffer 队列路径（不变）
```

> **无 regression**: ring 未启用时 rx_ring 为 None，完全走现有路径。
> **epoll 正确性**: wakeup_epoll 唤醒后，ep_send_events 重新 poll → can_recv() → has_user_frames() → 返回 true → EPOLLIN 传递给用户。

**验证**: `cargo build`

### Task 6: SOL_PACKET sockopt

**File**: `sockopt.rs`

`set_packet_option` SOL_PACKET 分支新增:
- `PACKET_VERSION(10)`: 解析 i32，存入 tpacket_version（ring 未创建时才允许）
- `PACKET_RX_RING(5)`: 解析 TpacketReq → validate_ring_config → PacketRing::setup → 存入 rx_ring
- `PACKET_RESERVE(12)`: 存储到 tp_reserve（ring 未创建时才允许）
- `PACKET_COPY_THRESH(7)`: 存储值

`get_packet_option` SOL_PACKET 分支新增:
- `PACKET_VERSION(10)`: 返回当前版本
- `PACKET_STATISTICS(6)`: 返回 TpacketStats { tp_packets, tp_drops }，**重置计数器**（Linux 语义）
- `PACKET_HDRLEN(11)`: 返回当前版本 hdrlen

**验证**: `cargo build`

### Task 7: 用户态测试程序

**File**: 新建 `user/apps/c_unitest/packet_ring_test/`

测试: socket(AF_PACKET, SOCK_RAW) → setsockopt(VERSION, V2) → setsockopt(RX_RING) → mmap → poll → 读帧 → getsockopt(STATISTICS)。

### Task 8: 内核编译

```bash
make kernel
```

---

## 6. 测试计划

| 测试 | 方法 | 预期 |
|------|------|------|
| 编译 | `make kernel` | 0 error |
| poll 唤醒 | C 测试: poll() 后读帧 | EPOLLIN 正确报告（评审 P0 修正验证） |
| 无 ring mmap | C 测试: 未 setsockopt 就 mmap | 返回 EINVAL（评审 P1 修正验证） |
| mmap + 抓包 | C 测试: 完整流程 | 能读到帧数据 |
| VLAN 帧 | 发送带 VLAN tag 的包 | tp_vlan_tci 正确，数据剥离 tag（评审 P2 修正） |
| 向后兼容 | 现有 packet socket 测试 | ring 未启用时行为不变 |
| tcpdump | QEMU 中运行 | 能抓包 |

---

## 7. 风险与坑点（评审修正版）

### 7.1 mmap size 精确匹配
用户 mmap 传错 size → 超出 page_cache 范围 SIGBUS。初版接受此行为（与 Linux 一致）。check_mmap_file 已防护无 ring 的情况。

### 7.2 物理页分配
大 ring 可能 OOM。返回 ENOMEM（与 Linux 一致）。

### 7.3 并发安全（评审 P2 修正）
- write_frame 在 `Arc<Mutex<PacketRing>>` 内层 Mutex 保护下**串行执行**（与 Linux `sk_receive_queue.lock` 一致）
- 外层 `rx_ring: Mutex<Option<Arc<Mutex<PacketRing>>>>` 只保护 setup/teardown
- deliver_from_iface clone Arc 后释放外层锁，再 lock 内层执行 write_frame → 不阻塞其他 deliver 的外层锁竞争
- tp_status 用 Release 序最后写（V1 AtomicU64, V2 AtomicU32）

### 7.4 ring 释放（评审 P1 修正）
**不在 do_close 中手动 drop ring**。依赖 PacketSocket 的自然 Drop：当所有 Arc 引用（包括 VMA 持有的 File 引用）释放后，PacketSocket Drop → PacketRing Drop → PageCache Arc 引用减少 → 物理页通过 Page 管理器回收。

> 关键：PacketRing 的 Drop **不应**显式释放物理页（与 bpf.rs:272 不同）。物理页由 PageCache 内部的 Arc<Page> 管理。当 PageCache 最后引用释放时，Page Drop 自动回收物理页。

### 7.5 tp_status 发布顺序
先写完所有 header 字段和数据 → 最后 store tp_status=USER（Release）。用户 load tp_status（Acquire）看到 USER 后读数据。

### 7.6 内存清零
ring 内存清零后 tp_status=0=KERNEL，所有帧默认可写。

---

## 8. Phase 2/3 扩展路径

**Phase 2 (V3)**: block 模型 + retire 定时器。`PacketRing` 通过 `version` 字段分发 V1/V2/V3 write_frame。block 级状态机。

**Phase 3 (TX_RING)**: `tx_ring: Mutex<Option<Arc<Mutex<PacketRing>>>>`。mmap_layout.size 返回 rx+tx 总大小。发送路径扫描 TP_STATUS_SEND_REQUEST 帧。

---

## 9. 文件变更总览

| 文件 | 操作 |
|------|------|
| `packet/uapi.rs` | 修改：补充常量 + 结构体 |
| `net/socket/base.rs` | 修改：Socket trait +mmap_layout() |
| `net/socket/inode.rs` | 修改：blanket impl 转发 + check_mmap_file |
| `packet/ring.rs` | **新建**：ring buffer 核心 + PacketFakeFs |
| `packet/mod.rs` | 修改：字段 + mmap_layout + mod ring |
| `packet/rx.rs` | 修改：can_recv ring 检查 + deliver ring 路径 |
| `packet/sockopt.rs` | 修改：SOL_PACKET 选项 |
| `packet/binding.rs` | 无需修改（packet_io_event 已调 can_recv） |
| `user/apps/c_unitest/packet_ring_test/` | **新建** |

---

## 10. 评审修正记录（v1 → v2）

| # | 严重度 | 问题 | 修正 |
|---|--------|------|------|
| 1 | P0 | can_recv() 不检查 ring，epoll 永不报告 EPOLLIN | §5 Task 5a: can_recv 增加 ring USER 帧检查 |
| 2 | P0 | 伪代码用不存在的 EPollItems::trigger() | §5 Task 5b: 改用 EventPoll::wakeup_epoll() |
| 3 | P1 | 无 ring 时 mmap → page fault → fs() panic | §4.2: blanket impl 新增 check_mmap_file 返回 EINVAL |
| 4 | P1 | §7.4 建议在 do_close drop ring 破坏活跃 VMA | §7.4: 依赖自然 Drop，不手动 drop |
| 5 | P2 | write_frame 锁策略矛盾 | §7.3: 统一为 Arc<Mutex> 串行化 |
| 6 | P2 | ring 写入未处理 VLAN 剥离 | §5 Task 3: prepare_data 处理 VLAN + tp_net 偏移 |
| 7 | P2 | V1 tp_status 8字节但可能用 4字节原子写 | §5 Task 3: publish_frame 版本相关 AtomicU64/U32 |
| 8 | P3 | 3 个 mmap trait 方法导致多次锁 | §4.2: 合并为 mmap_layout() 单一方法 |
