# 实现笔记：uprobe 支持（issue #2150 阶段一）

> 对应计划：`docs/plans/2026-07-24-uprobe-support.md`（步骤 1+2）。
> 本文件为工作产物，不进 git。本批只完成 **crate 骨架 + x86 指令分析**，不动 kernel/src/mm、perf、exception。

## 本批交付物

新建 `kernel/crates/uprobe/`：
- `Cargo.toml`：依赖 `yaxpeax-x86=2` + `yaxpeax-arch=0`（照 kprobe，仅 x86_64 target）。
- `src/lib.rs`：crate 根，导出 `core` 与 `arch`。
- `src/core.rs`（架构无关）：`UprobePoint` / `UprobeBasic` / `UprobeBuilder`。
- `src/arch/mod.rs`：`ProbeArgs` / `UprobeOps` / `CallBackFunc` trait + `impl UprobeOps for UprobePoint` + `pub(crate) ProbeHandler`。
- `src/arch/x86/mod.rs`：`analyze_insn` / `build_xol_slot` / `InsnAnalysis` / `RipReloc` / `UprobeInsnError` + 7 个 `#[cfg(test)]` 单测。
- `kernel/Cargo.toml`：新增 `uprobe = { path = "crates/uprobe" }`。

验证：
- `cargo test -p uprobe` → 7 passed。
- `cargo clippy -p uprobe --all-features` → clean。
- `cargo fmt -p uprobe --check` → clean。
- `cargo build --release -p dragonos_kernel --target arch/x86_64/x86_64-unknown-none.json`（即 `make kernel` 的 kernel_rust 步骤）→ 无 error / 无 warning。

---

## 计划偏离（与计划的差异 + 理由）

1. **模块名 `core` 与 `core` crate 同名** → 仍按计划命名为 `pub mod core`，但 crate 内所有对*外部* `core`/`alloc` crate 的引用一律用前导冒号 `::core::` / `::alloc::`，避免被本地 `core` 模块遮蔽（实测：`pub mod core;` + `use ::core::fmt::Debug` 可正常编译）。这是 no_std 下保留语义清晰模块名的标准做法。

2. **`UprobePoint.old_instruction: [u8; 16]`（非 kprobe 的 15）** → 按任务规格采用 16（`UPROBE_INSN_COPY_SIZE = 16`）。理由：2 的幂、对齐友好，且与典型 XOL slot 宽度一致（slot 需容纳 ≤15 字节指令副本，并保证其后字节可安全执行）。x86_64 单指令上限仍按 15 校验。

3. **`UprobeBuilder` 只接收 `probe_vaddr`，不接收 `path`/`offset`** → 计划步骤文字写“字段含 path/offset 或 probe_vaddr”。本 crate 选 `probe_vaddr`（已解析的用户虚拟地址）：`path`+`offset`→`probe_vaddr` 的解析依赖 VMA/ELF，属 perf 层（步骤 7）职责，引入会耦合 mm。保持 crate 纯粹。

4. **`event_callback: Arc<dyn CallBackFunc>`（kprobe 用 `Box`）** → 按任务要求用 `Arc`：同一 eBPF 回调需在多个 per-mm 探测点间共享。

5. **`UprobeBasic` 直接持有 `Arc<UprobePoint>`**（不做 kprobe 那样的 `Kprobe{basic,point}` 分层包装）→ 更简单，且与任务“UprobeBasic 含 probe_point”一致。

6. **`ProbeArgs` / `CallBackFunc` 在 uprobe crate 内独立定义，不 `use`/依赖 kprobe crate** → 低耦合。签名与 kprobe 对齐，以便复用同一套 TrapFrame 适配模式（后续步骤各写各的 args 适配器）。

---

## 边界情况处理

### RIP-relative 检测必须覆盖两种操作数呈现
- yaxpeax 对 `[rip+disp]`（disp≠0）给出 `Operand::Disp { base: RegSpec::RIP, disp }`；
- 但对 `[rip]`（disp==0）给出 `Operand::MemDeref { base: RegSpec::RIP }`（**实测发现，单测失败暴露**）。
- 二者都是 RIP-relative，都需重定位。`operand_rip_disp()` 同时处理这两种。
- **后果严重性**：漏判任一形式 → XOL slot 用原始 disp 执行 → 指向错误地址 → 静默损坏/崩溃。故对 base==RIP 的*所有*操作数变体（含掩码 / 带 index 的 EVEX 形式）显式处理：可重定位的两种返回 disp，其余返回 `Err`（fail-fast），绝不静默放过。

### disp_offset ≠ insn_len − 4
- RIP-relative 编码位移恒为 4 字节（disp32），但当指令**同时**带尾随立即数时（如 `mov dword [rip+x], imm32`、`add [rip+x], imm8`），位移位于立即数**之前**。
- 故 `disp_offset = insn_len − 4 − imm_size`，`imm_size` 由遍历操作数的立即数变体得出（含 `[rip+disp32]` 内存操作数的指令至多一个立即数）。
- 单测 `rip_relative_with_immediate`（`c7 05 ...`，len=10）验证 disp_offset=2。

### 重定位位移溢出
- `new_disp = disp + (probe_vaddr − slot_vaddr)`，若超出 i32 范围 → `UprobeInsnError::DisplacementOverflow`（uprobe 装不下，放弃该探测点）。
- 单测 `build_slot_displacement_overflow` 验证。

### 错误分类
- 空输入 → `Truncated`。
- 解码耗尽输入（如 `0xe8` 缺 imm32）→ 经 `DecodeError::data_exhausted()` 判定为 `Truncated`。
- 非法操作码（如 `0x06` push es 在 64 位无效）→ `DecodeFailed`。
- 解码长度 > 15 → `TooLong`（`==0` 理论不可达，一并归 TooLong）。

---

## 保守决策（做了哪些简化 + 为什么）

1. **本 crate 不读用户内存**。uprobe 探测对象在用户地址空间，读取需目标 mm 的页表上下文（CPL=0 切换 / page table walk），属 mm 层职责。故 `UprobeBuilder` 只持 `probe_vaddr`；原指令字节由调用方（步骤 4 断点安装）读取后交给 `analyze_insn`。

2. **`InsnAnalysis` 不内嵌进 `UprobePoint`**。`UprobePoint` 保持 4 字段的架构无关 DTO（按任务规格）。`InsnAnalysis`/`RipReloc` 是 x86 专属，放 `arch/x86`。二者由 mm 层在 per-mm 探测项中并列存储（见“开放问题”）。这样 `core` 不反向依赖 x86，耦合最低。

3. **`UprobeOps` 不含 `single_step_address`**（计划决策 1）。uprobe 单步地址 = per-mm XOL slot 用户地址（`xol_page_base + xol_slot_offset`），需 mm 上下文运行时算，不在本 crate。handler 通过 trait 取得 `old_instruction / insn_len / xol_slot_offset`，slot 真实地址由 mm 层另给。

4. **仅 x86_64 实现指令分析**。`analyze_insn`/`build_xol_slot` 在 `cfg(target_arch = "x86_64")` 下；`core` 与 arch trait 在所有架构可用。DragonOS uprobe 首期只针对 x86_64。

---

## 开放问题（留给后续步骤的接口契约）

### 步骤 3（per-mm uprobe 管理 + XOL 区）
- per-mm 探测项须**并列存储** `Arc<UprobePoint>` 与 `InsnAnalysis`（本 crate 提供二者，组合由消费方完成）。
- 负责填充 `UprobePoint.old_instruction` / `insn_len` / `xol_slot_offset`。
- XOL slot 页预分配；slot 分配/回收；维护 `xol_page_base` 以便命中时算 slot 用户地址。

### 步骤 4（断点页安装）
- 读取目标 mm 在 `probe_vaddr` 处的用户指令字节（≥16 字节）。
- 调 `analyze_insn(bytes)` 校验 + 取 `insn_len`；失败则放弃该探测点（fail-fast）。
- 将 `old_instruction[..insn_len]` 与 `insn_len` 回填 `UprobePoint`。
- 私有 COW 副本上 patch 0xcc（计划决策 3）。

### 步骤 5（异常分发）
- `do_int3`（用户态 #BP）：经 `UprobeOps` 取 `break_address`（= BPF 看到的 rip，**绝不暴露 XOL slot 地址给 BPF**，计划 F5）、`old_instruction`/`insn_len`/`xol_slot_offset`；
  算 `slot_vaddr = xol_page_base + xol_slot_offset`；调 `build_xol_slot(&analysis, probe_vaddr, slot_vaddr, old_instruction, slot_buf)` 填 XOL slot；rip→slot；设 TF；`pre_handler`/event_callback。
- `do_debug`（用户态 #DB，XOL 完成）：rip 回 `return_address()`（= break_address + insn_len），清 TF，`post_handler`。
- **`NEED_UPROBE` 位**（计划步骤 8）用于在 `do_debug` 区分“XOL 单步完成的 #DB”与 ptrace/硬件断点 #DB。

### TrapFrame 适配（ProbeArgs）
- 内核需提供一个 `TrapFrame` 适配器 `impl uprobe::ProbeArgs`（照 `kernel/src/debug/kprobe/args.rs`）：
  - `break_address()` → 探针址（probe_vaddr）；
  - `debug_address()` → XOL slot 中原指令执行后的下一条（slot_vaddr + insn_len）；
  - `as_any()` → 供回调 downcast 到具体 TrapFrame。

### CallBackFunc
- 步骤 7（perf 接入）提供 `Arc<dyn CallBackFunc>`（eBPF 入口），经 `UprobeBuilder::with_event_callback` 或 `UprobeBasic::update_event_callback` 注入。

---

## XOL RIP-relative 重定位公式（备忘）

原指令在 `probe_vaddr` 执行：`[rip+disp]` 有效地址 = `probe_vaddr + insn_len + disp`（rip 指向下一条指令）。
副本在 `slot_vaddr` 执行，欲保持同一有效地址：

```
slot_vaddr + insn_len + new_disp = probe_vaddr + insn_len + disp
→ new_disp = disp + (probe_vaddr − slot_vaddr)
```

`new_disp` 以小端 i32 写入 `slot[disp_offset .. disp_offset+4]`，`disp_offset = insn_len − 4 − imm_size`。

---

# 批次 2：mm 集成（per-mm uprobe 表 + XOL 区 + 断点页安装）

> 计划步骤 3+4。本节为工作产物，不进 git。

## 本批交付物

- **新文件** `kernel/src/mm/ucontext/uprobe.rs`（~670 行）：
  - `XolArea`：per-mm XOL 页（用户态 R-X 匿名页，256 个 16 字节 slot，位图分配/回收）。
  - `UprobeInstance`：per-mm 实例（`UprobeBasic` + `InsnAnalysis`）。
  - `UprobePageState`：per-page 断点追踪（原始页 + COW 副本 + refcount）。
  - `UprobeHandle`：注册句柄（Drop 自动注销，镜像 `KprobePerfEvent::drop`）。
  - `uprobe_register` / `uprobe_unregister`：公开 API。
  - `install_breakpoint_page` / `restore_breakpoint_page`：断点安装/恢复（复刻 do_wp_page）。
- **修改** `kernel/src/mm/ucontext/address_space.rs`：
  - `AddressSpace` 新增 3 个 irqsave SpinLock 字段（位于 `inner` 之外，F8）。
  - `AddressSpace::new()` 初始化为空。
- **修改** `kernel/src/mm/ucontext/inner.rs`：
  - `try_clone`：fork 时子进程映射原页（非 COW 副本），避免继承 0xcc。
- **修改** `kernel/src/mm/ucontext/mod.rs`：
  - 新增 `pub(crate) mod uprobe;`，导出公开 API。

验证：`make kernel` → 0 error / 0 warning。

---

## 计划偏离（与计划的差异 + 理由）

1. **uprobe 字段挂在外层 `AddressSpace`（非 `InnerAddressSpace`）** → 计划写"挂在 `AddressSpace`/`InnerAddressSpace` 上"。实际选择外层 `AddressSpace`：inner 是 `RwSem<InnerAddressSpace>`（睡眠锁），命中路径关中断不能取它。将 `uprobe_list` / `xol_area` / `uprobe_page_state` 直接放在 `AddressSpace` 上（与 `active_cpus` / `tlb_gen` 同级），由独立 `SpinLock` 保护，命中路径仅 `lock_irqsave` + 查表。

2. **XOL 页物理地址预存** → 计划未提。实际在 `XolArea` 中存 `page_paddr`，注册时 translate 一次存入。理由：batch3 #BP handler 关中断，不能取 `inner` 的 RwSem 拿 mapper 做 translate；有了 `page_paddr`，batch3 直接 `phys_2_virt(page_paddr)` + 偏移写 slot 内容。

3. **per-page refcount 而非每 uprobe 独立副本** → 计划写"阶段一可简化：每 uprobe 独立副本"。实际选择 per-page refcount：因为同一 PTE 只有一个物理页帧，第二个 uprobe COW 会替换第一个的副本，丢失其 0xcc。正确做法是共享 COW 副本 + refcount，与 Linux 一致。

4. **注销时 unpatch 0xcc（恢复原指令首字节）** → 计划写"恢复原指令页"。实际在 refcount > 0 时先在 COW 副本中恢复该 uprobe 的原指令首字节（`old_instruction[0]`），不影响同页其他 uprobe 的 0xcc。refcount == 0 时才恢复整页（set_entry 回原 paddr）。

5. **inode rmap 全量（pid==-1）未实现** → 计划步骤 4。本批仅实现单 mm（pid>=0）。`uprobe_register` 接受 `&Arc<AddressSpace>` + `probe_vaddr`（已解析）。inode rmap 全量遍历由 batch4 在 perf 层做（`collect_file_vmas` → 对每个 mm 调 `uprobe_register`）。

---

## 边界情况处理

### 同页多 uprobe
- 支持：第一个 uprobe COW 出私有副本 + patch 0xcc，后续 uprobe 在同一副本 patch 额外 0xcc + refcount++。
- 注销时 refcount > 0：仅恢复该偏移的原指令字节（从 `UprobePoint.old_instruction[0]` 读）。
- refcount == 0：恢复整页（set_entry 回原 paddr + rmap + flush_tlb）。

### fork（try_clone）
- 子进程**不继承** uprobe（uprobe_list / xol_area / uprobe_page_state 均为空——来自 `AddressSpace::new()`）。
- 子进程**不继承 0xcc**：try_clone 检查 `parent_mm.uprobe_page_state`，对断点页映射原始物理页（非 COW 副本）。
- 已知限制（stage 1）：子进程不继承 uprobe handler。若需继承，需在 try_clone 中克隆 uprobe_list 并重新安装断点（留 stage 2）。

### mmap/unmap 并发
- 若被探测页在注销前被 munmap：`restore_breakpoint_page` 检测 PTE 缺失（`get_table` 返回 None），跳过 PTE 恢复，仅清理 `uprobe_page_state`。COW 副本 Arc drop 后回收。
- 若 VMA 在注销前被 munmap 但 PTE 仍在（罕见）：rmap attach/detach 跳过（VMA 不存在），仅做 PTE 恢复。

### 指令跨页边界
- fail-fast：`read_user_insn_bytes` 只读当前页内剩余字节（`min(16, PAGE_SIZE - page_offset)`）。若指令长度超过可读字节，`analyze_insn` 返回 `Truncated`，注册失败。
- 影响：page_offset > 4081 时指令可能跨页。函数入口通常对齐，实际影响极小。

### VM_SHARED 可写映射
- 已知限制：uprobe 安装在 VM_SHARED 可写映射上时，COW 副本打破了共享语义（后续 write 会修改副本而非共享页）。agentsight 目标（.text 只读段）不受影响。

---

## 保守决策与简化

1. **XOL 仅 1 页（256 slot）** → 足够 256 个并发 uprobe/进程。超出返回 ENOMEM。Linux 用多页+树。
2. **注销时不 unmap XOL 页** → XOL 页在 mm 生命周期内常驻。slot 回收到位图但不释放物理页。简化生命周期管理。
3. **无写保护冲突处理** → 若安装 0xcc 后用户对同页做 write fault，do_wp_page 会再次 COW（丢失 0xcc）。uprobe 仍留在表中但不再触发 #BP。stage 1 接受此限制（.text 段不应被写）。
4. **pre/post handler 是函数指针（非 trait 对象）** → 镜像 kprobe 的 `KprobeBuilder::new(probe_addr, pre, post, enable)`。batch3 提供实际 handler 函数；batch4 通过 `event_callback`（`Arc<dyn CallBackFunc>`）注入 BPF。

---

## 留给 batch3/batch4 的 API 契约

### batch3（异常分发 #BP/#DB）

**命中路径（关中断，仅 SpinLock）**：
```ignore
let mm = ProcessManager::current_pcb().basic().user_vm()?;

// 1. 查 uprobe 表
let list = mm.uprobe_list.lock_irqsave();  // irqsave SpinLock
let Some(entries) = list.get(&trapframe.rip) else { return; };
drop(list);  // 缩短锁持有（或持有期间调 handler）

for entry in entries {
    let inst = entry.read();  // RwLock (spinlock-based, 安全关中断)
    if !inst.basic.is_enabled() { continue; }

    // 2. 调 pre_handler + event_callback（BPF 入口）
    inst.basic.call_pre_handler(&trapframe_adapter);
    inst.basic.call_event_callback(&trapframe_adapter);

    // 3. 算 XOL slot 地址 + 填 slot 内容
    let point = inst.basic.probe_point().unwrap();
    let offset = point.xol_slot_offset;
    let xol = mm.xol_area.lock_irqsave();
    let area = xol.as_ref().unwrap();
    let slot_vaddr = area.slot_vaddr(offset);
    let slot_paddr = area.page_paddr();
    drop(xol);

    // 通过 phys_2_virt 写 slot 内容（无需 mapper / RwSem）
    let slot_kva = unsafe { MMArch::phys_2_virt(slot_paddr) }.unwrap();
    let slot_buf = unsafe {
        core::slice::from_raw_parts_mut(
            (slot_kva.data() + offset) as *mut u8,
            UPROBE_INSN_COPY_SIZE,
        )
    };
    uprobe::build_xol_slot(
        &inst.insn_analysis,
        point.probe_vaddr,
        slot_vaddr.data(),
        &point.old_instruction,
        slot_buf,
    ).unwrap();

    // 4. rip → slot, 设 TF（XOL 单步）
    trapframe.rip = slot_vaddr.data();
    trapframe.set_tf();
}
```

**#DB 完成（XOL 单步后）**：
- 检查 NEED_UPROBE（计划步骤 8）判别 XOL 完成的 #DB；
- rip 回 `return_address()`（= probe_vaddr + insn_len）；
- 清 TF；
- 调 `post_handler`。

### batch4（perf 接入）
```ignore
// 注册 uprobe
let handle = uprobe_register(&mm, probe_vaddr, noop_handler, noop_handler)?;

// 注入 BPF 回调
if let Some(instance) = handle.instance() {
    instance.write().basic.update_event_callback(bpf_callback_arc);
}

// Drop handle → 自动注销（镜像 KprobePerfEvent::drop）
```

### TrapFrame 适配（ProbeArgs）
- 照 `kernel/src/debug/kprobe/args.rs` 实现 `impl ProbeArgs for UprobeTrapFrame`。
  - `break_address()` → `trapframe.rip`（#BP 时 = probe_vaddr）；
  - `debug_address()` → `trapframe.rip`（#DB 时 = slot_vaddr + insn_len）；
  - `as_any()` → `&self as &dyn Any`（供回调 downcast）。

### 字段访问
- `mm.uprobe_list` — `pub SpinLock<BTreeMap<usize, Vec<Arc<RwLock<UprobeInstance>>>>>`
- `mm.xol_area` — `pub SpinLock<Option<Box<XolArea>>>`
- `mm.uprobe_page_state` — `pub(crate) SpinLock<BTreeMap<usize, UprobePageState>>`（仅内部用）

---

# 批次 4：perf 接入（完成记录，2026-07-24）

> 仅改 `kernel/src/perf/`（新增 `uprobe.rs` + `mod.rs` 分发臂）。不碰 exception/trap（batch3）。

## 交付物

- **`kernel/src/perf/uprobe.rs`**（新）：`UprobePerfEvent` + `UprobePerfCallBack` +
  `perf_event_open_uprobe(args)` + `resolve_target` + `parse_path_and_offset`。
- **`kernel/src/perf/mod.rs`**：`mod uprobe;` + `perf_event_open` 的 `PERF_TYPE_MAX` 分发臂
  按 `args.name.contains('/')` 二分（F9）：含 `/` → uprobe（path:offset），否则 → kprobe（现有）。

## A. F9 分发臂

`PERF_TYPE_MAX(=6)` 处：
```ignore
perf_type_id::PERF_TYPE_MAX => {
    if args.name.contains('/') {
        let uprobe_event = uprobe::perf_event_open_uprobe(args)?;   // config2 = 文件偏移
        Box::new(uprobe_event)
    } else {
        let kprobe_event = kprobe::perf_event_open_kprobe(args);     // 现有行为不变
        Box::new(kprobe_event)
    }
}
```
判定依据：kprobe 的 config1 是内核符号名（不含 `/`）；uprobe 的 config1 是二进制路径
（必含 `/`）。无 sysfs event-source 设备（后续阶段补）。

## B. path → vaddr 解析（设计偏离说明）

**计划原文**："解析 path（name 中 `:` 前的部分）+ offset（config2）"。该描述自相矛盾
（若 offset 来自 config2，则 name 不应再含 `:offset`）。实际采用 **Linux 原生约定**：

- `config1`(name) = 二进制路径；
- `config2`(args.offset) = 文件偏移（权威）。

`parse_path_and_offset` 做**防御性兼容**：若工具把 `"path:0xOFFSET"` 编码进 config1 且
config2==0，则从 config1 的 `:` 后解析十六进制偏移；config2 非零时一律以 config2 为准。

**path → inode → VMA → probe_vaddr** 链路：
1. `ProcessManager::current_mntns().root_inode().lookup(&path)` → `Arc<dyn IndexNode>`；
2. `inode.page_cache()` → `Arc<PageCache>`（inode rmap 入口）；无 page_cache → EINVAL
   （目标不是已映射的常规文件）；
3. `page_cache.collect_file_vmas()` → `Vec<Arc<LockedVMA>>`（所有映射该 inode 的 VMA）；
4. 对每个 VMA：`probe_vaddr = region.start() + (offset - backing_pgoff*PAGE_SIZE)`，
   前提 `backing_pgoff*PAGE_SIZE <= offset < +region.size()`（`resolve_target`）。

`region.end()` 是 exclusive（`= start + size`）；覆盖判断用字节区间半开 `[start, +size)`。

## C. pid 语义（B8）

- `pid > 0`：`ProcessManager::find(RawPid::from(pid))` → `pcb.basic().user_vm()`，仅接受
  `Arc::ptr_eq` 的 VMA（单 mm）。
- `pid == 0`：当前进程（`ProcessManager::current_pcb().raw_pid()`），其余同上。
- `pid == -1`：不设 target_mm，接受 inode rmap 返回的**全部** VMA（跨所有 mm）。

对每个命中的 VMA 调 `uprobe_register(&mm, probe_vaddr, noop_handler, noop_handler)`。
非可执行映射（如只读数据段）返回 `EACCES` → 静默跳过；其余错误上抛。一条都没注册成 → EINVAL。

### pid==-1 全量的实现程度

**API 路径完整**：`collect_file_vmas` → 逐 VMA 解析 mm + probe_vaddr → 逐 mm `uprobe_register`。
即每个当前映射该文件的 mm 都装上 0xcc。**已知限制（阶段一可接受）**：
- `collect_file_vmas` 在返回时即释放 `i_mmap_read`；注册循环期间映射若并发变化，`uprobe_register`
  内部会校验 VMA 存在性（`mappings.contains` 失败 → EINVAL），不会写坏地址，但可能漏装/竞争。
  Linux 在此类操作期间持 `i_mmap_rwsem`；本批未持锁以避免与 mm 内部锁序耦合（batch2 边界）。
- 仅装"当前已映射"的 mm；后续新 mmap 该文件的进程不会自动获得探针（Linux 的 inode 级
  registration 留后续阶段）。

## D. UprobePerfEvent 多 handle 生命周期

```ignore
pub struct UprobePerfEvent {
    _args: PerfProbeArgs,
    handles: Vec<UprobeHandle>,   // pid>=0: 该 mm 的所有覆盖 VMA；pid==-1: 所有 mm
}
```

- **注册**：`perf_event_open_uprobe` 一次性建好全部 `UprobeHandle`（每个 = 一个 per-mm 探针
  + 0xcc 页 + XOL slot）。
- **BPF 注入**：`do_set_bpf_prog` JIT 出**一份** `Arc<UprobePerfCallBack>`，`clone` 进每个
  handle 的 `instance().write().basic.update_event_callback(..)`（多 mm 共用同一 JIT 产物）。
- **注销**：`UprobePerfEvent::Drop` 不写手动 unregister —— `Vec<UprobeHandle>` 析构时逐个
  drop，`UprobeHandle::Drop`（batch2）执行 `uprobe_unregister_internal`（恢复原页 → 移除表项
  → 回收 slot）。`PerfEventInode` 持 `Box<dyn PerfEventOps>`，fd 关闭 → inode drop → event
  drop → handles drop。卸载顺序由 Vec 决定，确定性可预测。

## E. BPF attach 透明性（F5）

`UprobePerfCallBack::call(&self, trap_frame: &dyn uprobe::ProbeArgs)`：
```ignore
let probe_addr = trap_frame.break_address();           // 原探针址
let tf = trap_frame.as_any().downcast_ref::<TrapFrame>()?;
let mut pt_regs = KProbeContext::from(tf);             // 复用 kprobe 的 pt_regs 布局
pt_regs.rip = probe_addr as u64;                        // 强制 F5：BPF 见到 rip = 原探针址
self.0.call(pt_regs_slice);                             // BasicPerfEbpfCallBack JIT 执行
```
**关键**：用 `break_address()` 覆写 `pt_regs.rip`，**不**暴露 XOL slot 地址、也不暴露 int3
故障点 `rip+1`。即便 batch3 传入的 TrapFrame.rip 仍是 raw `probe_vaddr+1`，BPF 也只见到
原探针址。`KProbeContext` 复用为 pt_regs 布局（F10：BPF_PROG_TYPE_KPROBE，无新枚举）。

`Box::leak` + `BasicPerfEbpfCallBack::drop`(`Box::from_raw`) 是 kprobe 模板的成对 JIT 内存
管理，本批原样照搬（非真泄漏，event drop 时回收）。

## F. 与 batch3 的接口契约（关键）

本批 perf 代码 **编译期不依赖** `impl uprobe::ProbeArgs for TrapFrame`（`&dyn uprobe::ProbeArgs`
 的 `as_any()` 对任意 `&dyn Any` 都能 downcast），**运行期依赖**：batch3 在 `#BP` handler 调
`inst.basic.call_event_callback(args)` 时，`args` 必须是 `&dyn uprobe::ProbeArgs` 且其
`as_any()` 能 downcast 出 `TrapFrame`。

**batch3 需补**（arch/x86_64/interrupt/mod.rs，紧挨现有 `impl kprobe::ProbeArgs for TrapFrame`）：
```ignore
impl uprobe::ProbeArgs for TrapFrame {
    fn as_any(&self) -> &dyn Any { self }
    fn break_address(&self) -> usize { (self.rip - 1) as usize } // #BP: rip=probe_vaddr+1 → -1
    fn debug_address(&self) -> usize { self.rip as usize }
}
```
本批 `UprobePerfCallBack` 已用 `break_address()` 覆写 rip，故只要 #BP 路径调
`call_event_callback` 时 `break_address()`=probe_vaddr，F5 即成立（与 raw rip 是否已调整无关）。

## G. 编译验证状态

- `perf/uprobe.rs` + `mod.rs` 分发臂：**0 error / 0 warning**（cargo check 已确认本文件干净）。
- 全 crate 当前剩余 4 个编译错误，**全部在 `src/exception/uprobe.rs`**（batch3 文件）：
  `TrapFrame: uprobe::ProbeArgs is not satisfied`（行 86/87/192/193）—— 即上述 F 节契约。
  batch3 的其余 3 个错（Vec 未导入 / phys_2_virt / interrupt_enable unsafe）已由 ExcDispatcher
  在并行中修复。已通过 hub 向 ExcDispatcher 同步契约，待其补 `impl uprobe::ProbeArgs for TrapFrame`。
- 本批未碰 exception/trap/mm 内部，仅调 batch2 公开 API（`uprobe_register`/`UprobeHandle`/
  `noop_handler`）与 VFS/ProcessManager 公开 API。

---

# 批次 3：异常分发（#BP/#DB 用户态分发 + XOL 单步 + SIGTRAP + NEED_UPROBE）

> 计划步骤 5（异常分发）+ 6（ptrace 协调）+ 8（NEED_UPROBE 判别位）。工作产物，不进 git。
> 仅改 exception/trap/state，不碰 mm（batch2 已定）/perf（batch4）。

## 本批交付物

- **新文件** `kernel/src/exception/uprobe.rs`：用户态 #BP/#DB 分发（`uprobe_breakpoint_handler`
  / `uprobe_debug_handler`）+ XOL slot 填充 + SIGTRAP 投递 + slot 反查辅助。
- `kernel/src/exception/mod.rs`：`pub mod uprobe;`。
- `kernel/src/arch/x86_64/interrupt/trap.rs`：`do_int3`/`do_debug` 加 `is_from_user()` 二分。
- `kernel/src/arch/x86_64/interrupt/mod.rs`：新增 `impl uprobe::ProbeArgs for TrapFrame`
  （紧挨现有 kprobe 版本，照 batch4 F 节契约）。
- `kernel/src/process/state.rs`：`ProcessFlags` 加 `NEED_UPROBE = 1 << 14`（位 14，NEED_RSEQ/
  IN_IOWAIT 之后、PID_UNHASHED 之前）。

验证：`make kernel` → **0 error / 0 warning**（含 batch4 perf/uprobe.rs 全通过）。

## 计划偏离（与计划的差异 + 理由）

### 1. F5 rip 透明性——保留 raw rip，由 BPF 回调归一化（非分发器预设）

计划 F5 原文「call_pre_handler/event_callback 入口 rip = break_address()」字面读像是「分发器
把 rip 预设成 probe_vaddr」。**实际不这样做**，理由：`break_address() = rip - 1`，若分发器先把
rip 设成 probe_vaddr，回调内 `break_address()` 会得到 `probe_vaddr - 1`（错）。

**实际契约（与 batch4 对齐，经 hub 确认）**：#BP handler 调 callback 时 **rip 保持 raw**
（= probe_vaddr + 1）。BPF 回调（`UprobePerfCallBack`）自己读 `break_address()` = rip-1 =
probe_vaddr 并覆写 rip，从而 BPF 观察到 probe_vaddr。XOL slot 用户址只在**所有回调返回后**
（Phase 4）才写入 rip，绝不暴露给 BPF。这样 `break_address()` 语义（= rip-1 = 原探针址）始终成立。

### 2. #DB 不经 NEED_UPROBE 之外的 per-task 状态反查 probe_vaddr——改用 slot 偏移反算

计划步骤 8 把 NEED_UPROBE 定为 1 位判别位（不携带 probe_vaddr）。#DB 时需恢复 probe_vaddr +
insn_len。Linux 用 per-task `uprobe_task{active_uprobe}` 存活动探针。**本批为不扩 PCB**，
改用 XOL slot 几何反算：
- NEED_UPROBE 置位 ⇒ frame.rip 落在 XOL 页内；
- slot 16 字节对齐 ⇒ `slot_offset = (rip − page_base) & !0xF`（无需 insn_len）；
- 遍历 `uprobe_list` 找 `xol_slot_offset == slot_offset` 的实例 → probe_vaddr。

代价：#DB 路径 O(活动探针数) 遍历。stage 1 探针少，可接受。若日后探针多，可加 per-task
`active_uprobe` 字段（O(1)）——属 process 模块改动，超出本批「exception + ProcessFlags」范围。

### 3. event_callback 单次触发（#BP），#DB 仅 post_handler

kprobe 在 #BP(pre) 与 #DB(post) 都调 `call_event_callback`。uprobe **只在 #BP 触发一次**
（计划步骤5：#BP=pre/BPF，#DB=post），#DB 仅调 `call_post_handler`，避免 perf event 双触发
（双触发会污染采样计数）。post_handler 入口 rip 已设为 return_address（probe_vaddr+insn_len），
F5 同样不暴露 XOL slot。

### 4. NEED_UPROBE 不进 `exit_to_user_mode_work`

明确：NEED_UPROBE **只**作 #DB 分发判别位（#BP 设、#DB 清），不加入
`ProcessFlags::exit_to_user_mode_work` 的掩码（NEED_SCHEDULE|HAS_PENDING_SIGNAL|NEED_RSEQ），
即不是 exit-to-user 延迟工作。`fork_inherited` 也不继承（子进程无活动单步）。

## XOL slot 填充的关中断写法

命中路径关中断（entry.S cli），**不能**取 `PageMapper`/`RwSem`。复刻 batch2
`patch_byte_in_phys`：`XolArea::page_paddr()` 给物理址 → `MMArch::phys_2_virt(paddr)` 得内核
direct-map 虚拟址 → `copy_nonoverlapping` 写整 16 字节 slot（`build_xol_slot` 产出重定位指令副本
+ 零填充，覆盖 slot 复用时残留字节）。XOL 页是普通 RAM，非 MMIO，非 volatile 拷贝即可。

锁顺序：uprobe_list 与 xol_area 是两个独立 irqsave SpinLock，**不嵌套**——Phase 1 取
uprobe_list（跑 callback + 取 probe_point/analysis），释放；Phase 2 取 xol_area（取
slot_vaddr/page_paddr），释放；Phase 3-4 无锁填 slot + 改 trapframe。

## ptrace 协调（步骤 6，文档化）

处理顺序：**kprobe > uprobe > ptrace**（#BP/#DB）。do_int3/do_debug 按 is_from_user 二分：
内核态走 kprobe（EBreak/DebugException），用户态走 uprobe 分支。

TF 拥有权：uprobe 单步窗口（#BP 设 TF 到 #DB 清 TF）TF 归 uprobe。DragonOS `process/ptrace.rs`
**当前未实现 PTRACE_SINGLESTEP**（无 TF/0x100/SINGLESTEP 引用），故现阶段无 uprobe↔ptrace TF
冲突。NEED_UPROBE 已为未来 ptrace #DB 留判别位：用户态 #DB 若 NEED_UPROBE 未置位 → ptrace/
硬件断点，阶段一 return Ok（不投信号、不动 TF，留给 ptrace 自有路径）。

已知限制（stage 1）：ptrace 单步（PTRACE_SINGLESTEP）的 SIGTRAP 投递未实现；硬件断点 #DB
未处理。完整 uprobe+ptrace 同进程共存留后续。

## 留给 batch4 的接口（已确认对齐）

- `impl uprobe::ProbeArgs for TrapFrame`：as_any→self / break_address→(rip-1) /
  debug_address→rip。batch4 回调经 as_any() downcast TrapFrame、break_address() 取 probe_vaddr。
- **运行时契约**：#BP 调 call_event_callback 时 rip 保持 raw（probe_vaddr+1）；XOL slot 址
  仅 Phase 4 写入。
- event_callback 单次（#BP）；若 batch4 需要 post 期回调，用 call_post_handler（#DB 触发）。

## 编译验证状态（更新）

- `make kernel`：**0 error / 0 warning**。
- batch4 perf/uprobe.rs 的 4 个「TrapFrame: uprobe::ProbeArgs」错误已由本批补的
  `impl uprobe::ProbeArgs for TrapFrame` 解决（即 batch4 G 节所述，现已闭环）。

---

# 局部 bug 修复（独立验证 P1/P2）

> 阶段一独立验证发现 2 个局部 bug，架构正确无需回退，本节记录修复方案与设计决策。

## P1：重复注册同一 probe_vaddr 读到 0xcc

**现象**：`uprobe_register` 先 `read_user_insn_bytes` 读原指令、后 `install_breakpoint_page`
装 0xcc。第二个 consumer 注册同一 `probe_vaddr` 时，PTE 已指向含 0xcc 的 COW 副本，
`read_user_insn_bytes` 读到 `0xcc` 当原指令首字节 → `old_instruction[0]=0xcc`。
若该条目成 `entries[0]`（#BP handler 用 `entries[0]` 填 XOL slot），slot 填 0xcc → 无限 #BP；
注销也写回 0xcc（永久断点）。

**修复**：在读指令**之前**先 `lock_irqsave(uprobe_list)` 查是否已有该 `probe_vaddr` 条目：
- **有**：复用其 `probe_point.old_instruction` + `insn_analysis`（二者对所有同址实例一致，
  均为 `Copy` 类型，直接拷出，跳过 `read_user_insn_bytes` + `analyze_insn`）。
- **无**：正常读取 + 分析。

**设计决策**：
- 复用的是指令**信息**（字节 + 分析结果），不是 `Arc<UprobePoint>` 本身——每个 consumer
  仍分配自己的 XOL slot、持有自己的 `UprobePoint`（含自己的 `xol_slot_offset`）。共享 Arc
  会使 slot 偏移串用，破坏注销时的 per-instance slot 回收。
- 复用的 `old_instruction` 是首个 consumer 注册时（0xcc 发布**之前**）捕获的真原指令，
  绝不可能是 0xcc。这同时修正了注销恢复路径（`restore_breakpoint_page` 写回的是真原字节，
  而非原先错误读到的 0xcc）。
- `probe_point()` 理论恒为 `Some`（`UprobeBuilder::build` 总设 `Some`）；若防御性为 `None`，
  `and_then` 链回退到正常读取路径（不劣于原行为）。

## P2：RIP-relative 位移溢出在 #BP 命中时 panic

**现象**：`build_xol_slot` 只在 `exception/uprobe.rs` 的 `fill_xol_slot`（命中时）调用，
注册时从不验证。若 `|probe_vaddr - slot_vaddr|` 超 i32（64 位地址空间常见），返回
`DisplacementOverflow` → handler `Err` → `do_int3` unwrap panic。

**修复（注册时预填 XOL slot）**：在分配 XOL slot **之后**（`slot_vaddr = xol_page_base +
slot_offset` 已知），立即用真实 `slot_vaddr` 调 `build_xol_slot`：
- **溢出**（或任何 `UprobeInsnError`）→ `free_xol_slot` + 返回 `EINVAL`（注册失败，fail-fast，
  绝不留下命中时 panic 的探针；此时尚未插入 `uprobe_list`，无需回滚表项）。
- **成功** → slot 内容（重定位后指令副本 + 零填充）经 `XolArea::page_paddr` + `phys_2_virt`
  写入 slot 物理页对应偏移（复刻 `fill_xol_slot`/`patch_byte_in_phys` 写法）。

**命中路径简化**：`uprobe_breakpoint_handler` 移除 `fill_xol_slot` 调用（连同函数本身删除）——
slot 已在注册时预填，命中时只需从 `entries[0]` 取 `xol_slot_offset`、算 `slot_vaddr`、
`rip→slot`。关中断路径不再调 `build_xol_slot`，位移溢出无从发生。`slot_vaddr` 计算保留。

**清理**：`exception/uprobe.rs` 因移除 `fill_xol_slot` 而不再使用的导入一并删除
（`build_xol_slot` / `PhysAddr` / `MMArch` / `MemoryManagementArch`——后者原在作用域内为
`MMArch::phys_2_virt` trait 方法解析所需，移除调用后即多余）。`UprobeOps` 保留（#DB handler
的 `.return_address()` 依赖）。

## P1/P2 协同与不变量

两个修复均在 `uprobe_register` 注册流程，新顺序（保持 F6 装弹不变量）：
1. 查 `uprobe_list` 同址条目（P1）→ 复用 old_instruction/insn_analysis 或新读；
2. 分配 XOL slot；
3. `build_xol_slot` 预填 slot + 验证位移（P2）→ 溢出 EINVAL；
4. 构造实体；
5. 插入 `uprobe_list`（表项在 0xcc 发布前就绪）；
6. `install_breakpoint_page`（0xcc）。

- **F6 不变量保持**：预填（步骤 3）在表项插入（步骤 5）之前，0xcc 发布（步骤 6）之前任何
  查表都能找到 slot 已就绪的表项。
- **同页多 uprobe 的 `existing_cow` 逻辑不变**：仍 refcount + patch 额外字节；对同一
  `probe_vaddr`（同 page_offset）patch 0xcc 是幂等写（字节已是 0xcc），无需额外跳过判断。
- **多 consumer 同址**：每个 consumer 分配独立 slot 并各自预填；命中用 `entries[0]` 的 slot
  （该实例注册时已预填）。`entries[0]` 因注销变动时，新首项的 slot 亦已预填。
- **无 regression**：kprobe 路径、fork（`try_clone` 用 `original_paddr`）、#DB handler
  （`find_probe_vaddr_by_slot` + `return_address`）均未触及。

## 验证（P1/P2 修复后）

- `make kernel`：**0 error / 0 warning**（dragonos_kernel 全量重编）。
- `cargo test -p uprobe`：**7 passed / 0 failed**。
- P1：同址二次注册复用已有 `old_instruction`（真原指令，非 0xcc），跳过 `read_user_insn_bytes`。
- P2：位移溢出在注册时返回 `EINVAL`；命中路径无 `build_xol_slot`、slot 已预填。
