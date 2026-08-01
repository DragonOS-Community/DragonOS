# Plan: uprobe 断点探针支持（issue #2150 阶段一）

> YOLO-dev 模式：agent 自主推进，subagent 对抗门控。本文档不进 git（见 Git 纪律）。
> 盲区扫描：glm-5.2 scout（BlindSpotScanner-2）。对抗评审：glm-5.2 reviewer（PlanReviewer），10 条 findings 全采纳。

## 任务
为 DragonOS 实现 uprobe 断点探针，使 agentsight 能在用户态函数 `SSL_read`/`SSL_write` 入口挂探针捕获参数。对应 issue #2150 **阶段一**（issue 明确建议"阶段一完成后验证再进入阶段二，不要一口吞"）。**uretprobe（阶段二）不在本计划范围**——它是独立重型机制（栈返回地址改写 + trampoline 页）。

## kprobe 复用基础（已验证）
- 断点机制：`kernel/crates/kprobe/src/arch/x86/mod.rs` `KprobeBuilder::replace_inst()` 写 0xcc；`KprobeOps::single_step_address()` 返回**内核缓冲区**指针（对 uprobe 不可用）
- 异常分发：`#BP`→`ebreak.rs` `EBreak::handle()`（持 `KPROBE_MANAGER` 全局锁跨 callback）；`#DB`→`debug.rs`
- eBPF attach：`perf/kprobe.rs` `perf_event_open_kprobe`→`KprobePerfEvent`→`do_set_bpf_prog` JIT
- **指令解码已就绪**：`kernel/crates/kprobe/Cargo.toml` 依赖 `yaxpeax-x86="2"`+`yaxpeax-arch="0"`，`arch/x86/mod.rs` 用 `InstDecoder::default().decode_slice()` 算指令长度——**uprobe 直接复用**

## 盲区扫描结论（BlindSpotScanner-2）

### 架构级盲区（已处理，见决策）
- **B1[高]** kprobe 单步把 rip 指向内核缓冲区执行——CPL=3 时内核页 supervisor-only + NX，绝无可能。→ XOL。
- **B2[高]** `#BP`/`#DB` handler 关中断（entry.S `cli`），`page_table_edit()` `debug_assert!(is_irq_enabled())`。→ 命中路径零页表改动，XOL/断点页注册时预建。
- **B3[高]** EBreak 持全局 `KPROBE_MANAGER` 锁跑 BPF。→ 独立 per-mm 分发。
- **B4[高]** `do_int3`/`do_debug` 无 `is_from_user()` 分支；未匹配 #BP 静默吞，无 SIGTRAP。→ 新增用户态分发 + SIGTRAP。

### 设计接入点盲区（已处理）
- **B5[中]** 与 ptrace/SIGTRAP/TF 单步冲突（ptrace 活跃，`PTRACE_SINGLESTEP`→TF）。→ 定义处理顺序。
- **B6[中·正向]** inode→VMA 反向映射已存在：`page_cache.rs` `i_mmap_rwsem`+`file_vmas`+`register_file_vma`/`collect_file_vmas`。→ 用于**定位**目标 VMA/mm。
- **B7/B8/B9** perf type 约定；pid 语义；ProcessFlags 位。

### 已覆盖（别重造）
- TLB shootdown 成熟：`mm/tlb.rs`+`mmu_gather.rs`。直接用 `flush_tlb_range`。
- per-mm 隔离：`AddressSpace`(user_vm)。
- offset→vaddr：VMA `vm_file`+`backing_pgoff`+`address_space()`。
- 内核内 ELF 符号解析不是依赖（offset 由用户态工具算，config2）。

## 高风险决策（评审修正后）

1. **XOL 执行原指令（非复用 kprobe 内核缓冲区单步）** — [B1/B2] uprobe 不复用 `KprobeOps::single_step_address`。每个 mm 注册时预分配 XOL slot 页；命中时 rip→slot（原指令副本，RIP-relative 重定位），设 TF，iretq 后用户态执行，TF 触发 #DB，handler rip 回原址+insn_len。**指令解码直接复用 yaxpeax-x86**（kprobe 已依赖，完整 x86-64 解码器含 RIP-relative 操作数语义），非新工作（评审 F3）。

2. **独立 per-mm 分发（irqsave SpinLock，非 RwSem）** — [B3/B10, 评审 F8] uprobe 表 `uprobe_list: BTreeMap<vaddr, LockUprobe>` 挂在 `AddressSpace`/`InnerAddressSpace` 上，由**独立 irqsave SpinLock** 保护（镜像 `KPROBE_MANAGER: SpinLock`，**不用** `RwSem`，因命中路径关中断不可睡眠）。`do_int3`/`do_debug` 按 `is_from_user()` 二分。命中路径仅 lock+查表+改 trapframe+跑 BPF。

3. **断点页安装复刻 do_wp_page 私有 COW 路径（非 unmap+map_phys）** — [评审 F1/F2/F7] 复刻 `fault.rs:957-979` 私有文件 COW：`copy_page_as_normal`（源 File 页→私有 Normal 副本）+ patch 0xcc + **单次** `get_table().set_entry(PageEntry::new(new_paddr, new_flags))` 原子帧替换（**绝不** unmap+map_phys 制造瞬时空 PTE）+ `detach_fault_mapped_page(old)`/`attach_fault_mapped_page(new)` rmap 账簿 + `flush_tlb_range`。**无论 pid>=0 还是 -1，都为每个目标 mm 生成私有 COW 副本，绝不修改共享 page-cache 页**（否则 writeback 回写 0xcc 损坏 .so / 双重释放）。

4. **inode rmap 仅用于定位目标 VMA/mm** — [B6] inode rmap（`file_vmas`+`i_mmap_rwsem`）用于**定位**哪些 mm/VMA 映射目标文件偏移；实际页替换按决策3。

5. **uretprobe 不在本阶段** — [B11] issue 明确分两阶段。uretprobe 的栈返回地址改写 + trampoline 留阶段二。

6. **pid 语义** — [B8] `pid>=0` 单 mm；`pid==-1` 经 inode rmap 全量 mm。**两者都为每个 mm 私有 COW**（见决策3）。

## 实现步骤（概要）

1. **uprobe crate 骨架**（`kernel/crates/uprobe/`，新建） → 验证: `make kernel` 编译通过。`lib.rs`+core（`UprobeBuilder`/`UprobeBasic`/`UprobePoint`，含原指令副本 + XOL slot 偏移 + 回调）；`arch/mod.rs` 定义 `UprobeOps`/独立 `CallBackFunc`。**不复用 kprobe 的 `single_step_address`**。

2. **x86 指令分析模块**（`kernel/crates/uprobe/src/arch/x86/`） → 验证: 算出指令长度 + 识别 RIP-relative。**直接复用 yaxpeax-x86**（kprobe 已依赖）算长度 + RIP-relative 检测；生成 XOL slot 副本（RIP-relative 重定位，其余 fail-fast）。

3. **per-mm uprobe 管理 + XOL 区**（`kernel/src/mm/ucontext/`） → 验证: 注册/注销 uprobe；每 mm 有 XOL VMA。`AddressSpace`/`InnerAddressSpace` 加 `uprobe_list`，由**独立 irqsave SpinLock** 保护（评审 F8）；XOL slot 页注册时预分配、slot 分配/回收。

4. **断点页安装**（`kernel/src/mm/`） → 验证: 装 0xcc 后目标 CPU flush_tlb 后执行到该地址立即 #BP；fork/mmap/unmap 并发不崩；writeback 不回写 0xcc。**复刻 do_wp_page 私有 COW**（决策3）：copy_page_as_normal + 0xcc + 单次 set_entry + rmap detach/attach + flush_tlb_range；每目标 mm 私有副本，不改共享 page-cache。

5. **异常分发分支**（`arch/x86_64/interrupt/trap.rs` + `exception/`） → 验证: 用户态执行到 uprobe 触发 #BP，handler 收正确 pt_regs；XOL 单步后正确返回原址继续；未消费 #BP 投递 SIGTRAP(TRAP_BRKPT)。`do_int3`/`do_debug` 加 `is_from_user()`：用户态 #BP→per-mm 查表→**pre_handler/BPF 入口 rip = break_address()（原探针址，XOL slot 绝不暴露给 BPF，评审 F5）**→BPF 返回后 rip→XOL slot→设 TF；用户态 #DB（XOL 完成）→rip 回原址+insn_len→清 TF→post_handler。

6. **ptrace 协调**（`process/ptrace.rs`） → 验证: 处理顺序文档化；被 ptrace 进程挂 uprobe 行为可预测（kprobe > uprobe > ptrace）。TF 拥有权在 uprobe 单步窗口归 uprobe。

7. **perf 接入**（`perf/`） → 验证: eBPF 经 `perf_event_open`+`PERF_EVENT_IOC_SET_BPF` attach 成功。**复用 PERF_TYPE_MAX(=6)**，按 config1 name 含 `/` 区分 uprobe/kprobe（评审 F9，最小 Linux 兼容路径）；`UprobePerfEvent`（照 `KprobePerfEvent`）；解析 path+config2(offset)，**消费 pid**；无 sysfs event-source 设备（后续阶段）。

8. **NEED_UPROBE = #DB 分发判别位（非 exit-loop 延迟工作）**（`process/state.rs`+`exception/`） → 验证: do_debug 正确识别 XOL 完成 #DB。**用途**（评审 F4）：#BP handler 设 NEED_UPROBE；`do_debug` 检查并清之以识别「XOL 单步完成的 #DB」（区别 ptrace/硬件断点 #DB），rip 改回 probe+insn_len、清 TF。

9. **bindings**（`include/bindings/linux_bpf.rs`） → 验证: 与 Linux 对齐。**确定** uprobe 复用 `BPF_PROG_TYPE_KPROBE(=2)`（评审 F10）；`perf_type_id` 无需新增。

**装弹顺序不变量（评审 F6）**：perf_event_open 内严格按 XOL slot 分配 → 表项注册 → 0xcc 页发布 顺序；0xcc 发布前任何路径查该 vaddr 必须能找到就绪 uprobe 表项。

## 显式假设（替代 Interview）
- [假设A] ~~无解码器~~ → **已确认 kprobe 依赖 yaxpeax-x86，可直接复用**（评审 F3 核实）。
- [假设B] `AddressSpace` 可挂 uprobe 表 + 加 XOL VMA — 不确定性: 低 — 已验证有 user_vm/page_table_edit。
- [假设C] XOL 用 RIP-relative 重定位覆盖常见指令 — 不确定性: 中 — 不支持的指令 fail-fast，首期覆盖率受限 — 可接受（agentsight 探函数入口）。

## 已知风险
- 指令解码风险**大幅降级**（yaxpeax-x86 已存在，评审 F3）；主要剩余：RIP-relative 重定位正确性 + fail-fast 覆盖率。接受部分。
- COW 页并发：复用 do_wp_page 模板 + i_mmap_rwsem + page_table_edit_lock + MmuGather shootdown-before-free。接受（基础成熟）。
- ptrace + uprobe TF 冲突：定义处理顺序，阶段一不保证同进程 ptrace+uprobe 完美共存。接受（先正名）。
- 共享 page-cache 绝不直接 patch（决策3 私有 COW 保证 writeback 不损坏）。

## 评审记录（PlanReviewer，glm-5.2，10 findings 全采纳）

| # | finding | priority | 处理 |
|---|---|---|---|
| F1 | 断点页替换用错原语（unmap+map_phys 瞬时空 PTE） | 必须 | 决策3 改为复刻 do_wp_page 单次 set_entry 原子写。采纳。 |
| F2 | 断点页替换遗漏 page-type 转换 + rmap 账簿 | 必须 | 决策3 加 copy_page_as_normal + detach/attach。采纳。 |
| F3 | 假设A 错误，kprobe 已依赖 yaxpeax-x86 | 必须 | 步骤2 改复用 yaxpeax；假设A 删除；风险降级。采纳。 |
| F4 | 步骤8 误述 NEED_UPROBE（应是 #DB 判别位） | 必须 | 步骤8 重写为 #DB 分发判别位。采纳。 |
| F5 | 步骤5 未写死 BPF 透明性 | 建议 | 步骤5 补 pre_handler rip=break_address() 不变量。采纳。 |
| F6 | 装弹顺序不变量缺失 | 建议 | 加 XOL slot→表项→0xcc 发布顺序约束。采纳。 |
| F7 | pid 语义与页策略矛盾（共享页） | 建议 | 决策3/6 明确每 mm 私有 COW，不改共享 page-cache。采纳。 |
| F8 | per-mm 表锁类型未指定（IRQ-off 不可 RwSem） | 建议 | 决策2 改 irqsave SpinLock。采纳。 |
| F9 | perf type 约定未定 | 建议 | 步骤7 拍板复用 PERF_TYPE_MAX 按 name 含 / 区分。采纳。 |
| F10 | 步骤9 bindings 未确定 | 建议 | 步骤9 确定 BPF_PROG_TYPE_KPROBE，perf_type_id 无需新增。采纳。 |

无"评审错了"项——10 条均为真实问题/合理建议。架构方向正确，未回退 Phase 0。
