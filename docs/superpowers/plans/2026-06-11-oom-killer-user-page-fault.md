# OOM Killer For User Page Fault Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 DragonOS 的用户态 page-fault `VM_FAULT_OOM` 路径补齐第一版 OOM killer，避免 livelock，并提供可复现测试。

**Architecture:** 先补 per-mm `resident_user_pages`，再新增 `mm::oom` 统一处理 victim 选择、TGID 级 `SIGKILL` 提交和等待通知，最后接入 x86_64 fault 路径。第一版只覆盖用户态缺页 OOM，`NoVictim` 统一走 fatal OOM。

**Tech Stack:** Rust 内核代码、DragonOS 进程/信号/MM 子系统、`user/apps/c_unitest`。

---

### Task 1: Per-mm 驻留页统计

**Files:**
- Modify: `kernel/src/mm/ucontext.rs`
- Modify: `kernel/src/mm/fault.rs`
- Modify: `kernel/src/filesystem/procfs/pid/stat.rs`
- Modify: `kernel/src/filesystem/procfs/pid/statm.rs`

- [ ] **Step 1: 给 `AddressSpace` 增加原子 RSS 统计与快照接口**

实现内容：

```rust
pub struct AddressSpace {
    // ...
    resident_user_pages: AtomicUsize,
}

impl AddressSpace {
    pub fn resident_pages(&self) -> usize { /* load */ }
    pub fn account_present_page_add(&self) { /* fetch_add */ }
    pub fn account_present_page_sub(&self) { /* saturating sub */ }
}
```

- [ ] **Step 2: 在缺页建立新 present PTE 的路径计数**

最小覆盖这些入口：

```text
kernel/src/mm/fault.rs
- do_anonymous_page()
- filemap_map_pages()
- finish_fault()
- zero_fault()
- zero_map_pages()
```

规则：

- 只有“原先无映射，成功安装 present PTE”才加 1
- COW 替换旧页不加减

- [ ] **Step 3: 在拆映射与地址空间回收路径减计数**

最小覆盖这些入口：

```text
kernel/src/mm/ucontext.rs
- LockedVMA::unmap()
- LockedVMA::unmap_range()
- InnerAddressSpace::unmap_all()
```

规则：

- 只有真的拆掉 present PTE 才减 1
- 统一用饱和减法

- [ ] **Step 4: 让 `/proc/<pid>/stat` 与 `statm` 使用 resident 统计**

实现目标：

- `stat` 的 `rss_pages` 不再由 `vma_usage_bytes()` 近似
- `statm` 第二列 `resident` 改为真实 resident 页数

- [ ] **Step 5: 编译验证**

Run: `make kernel`

Expected: 内核编译通过，无新增语法错误。

### Task 2: 新增 `mm::oom` 核心

**Files:**
- Create: `kernel/src/mm/oom.rs`
- Modify: `kernel/src/mm/mod.rs`
- Modify: `kernel/src/process/mod.rs`
- Modify: `kernel/src/ipc/kill.rs` 或 `kernel/src/ipc/signal.rs`（仅在需要补接口时）

- [ ] **Step 1: 定义 OOM 核心数据结构**

实现内容：

```rust
pub struct OomContext { /* trigger pid/tgid, addr, ip, order */ }
pub enum OomOutcome { Retry, CurrentTaskKilled, NoVictim }
struct OomVictimState { generation: u64, tgid: RawPid, mm: Weak<AddressSpace> }
```

- [ ] **Step 2: 实现全局串行化与等待状态**

实现目标：

- 一个全局状态锁
- 一个 generation 计数器
- 一个等待队列
- 任意时刻只允许一个 in-flight victim

- [ ] **Step 3: 实现候选扫描、过滤和评分**

候选来源：

```text
ProcessManager::get_all_processes()
-> 归一化为线程组组长
-> 按 TGID 去重
-> 读取 user_vm + resident_pages()
```

过滤条件：

- `PID 0`
- 全局 `PID 1`
- `KTHREAD`
- `EXITING`
- `user_vm == None`
- `resident_pages == 0`

评分公式：

```text
badness = resident_user_pages
tie-break: resident pages desc, then TGID desc
```

- [ ] **Step 4: 实现 victim 提交与等待裁决**

实现目标：

- 重新验证候选仍存活且 `mm` 未切换
- 对 TGID 投递 `SIGKILL`
- 当前线程组若被选中，返回 `CurrentTaskKilled`
- 其他 victim 成功提交后等待 `mm` 释放/`resident_pages == 0`，然后返回 `Retry`
- 无候选或提交失败返回 `NoVictim`

- [ ] **Step 5: 编译验证**

Run: `make kernel`

Expected: 新模块已接入构建，编译通过。

### Task 3: 退出通知与 x86_64 fault 接入

**Files:**
- Modify: `kernel/src/process/mod.rs`
- Modify: `kernel/src/arch/x86_64/mm/fault.rs`
- Modify: `kernel/src/arch/x86_64/ipc/signal.rs`（仅在需要暴露显式信号处理入口时）

- [ ] **Step 1: 在 `user_vm` 解绑时通知 OOM 核心**

目标入口：

```text
ProcessManager::exit()
```

要求：

- 在当前 PCB `set_user_vm(None)` 前后，向 `mm::oom` 报告该 `mm` 已进入释放阶段
- 若该 `mm` 是当前 victim，则唤醒等待者

- [ ] **Step 2: 改造 x86_64 `VM_FAULT_OOM` 分支**

目标逻辑：

```text
drop mm/vma/page guards
outcome = mm::oom::pagefault_out_of_memory(ctx)
Retry             -> 重新进入 fault loop
CurrentTaskKilled -> 显式执行当前线程信号/退出处理
NoVictim          -> 打印摘要并 panic
```

- [ ] **Step 3: 保持其他 fault 语义不变**

重点回归：

- `SIGSEGV`
- `SIGBUS`
- kernel access fixup

- [ ] **Step 4: 编译验证**

Run: `make kernel`

Expected: fault 路径改动后编译通过。

### Task 4: 故障注入与用户态回归测试

**Files:**
- Modify: `kernel/src/mm/oom.rs` 或新增同目录辅助测试配置文件
- Create: `user/apps/c_unitest/test_oom_killer.c`
- Modify: `user/apps/c_unitest/Makefile`（若需要显式控制）

- [ ] **Step 1: 提供只用于测试的用户缺页 OOM 注入入口**

最小要求：

- 默认关闭
- 能限定当前测试触发者
- 能在用户 page-fault 分配路径制造一次或持续失败
- 不影响内核自身关键分配

- [ ] **Step 2: 编写 `c_unitest` 回归程序**

至少覆盖：

- 单进程自杀：只有当前任务可杀时，以 `SIGKILL` 退出，不 livelock
- 双进程竞争：更高 resident 的对手被杀，当前进程重试成功

- [ ] **Step 3: 构建测试程序**

Run: `make -C user/apps/c_unitest`

Expected: `test_oom_killer` 成功编译。

### Task 5: 全量验证

**Files:**
- Verify only

- [ ] **Step 1: 代码格式与静态检查**

Run: `make fmt`

Expected: 格式化与 clippy 通过。

- [ ] **Step 2: 内核编译**

Run: `make kernel`

Expected: 编译通过。

- [ ] **Step 3: 运行最小回归**

Run: 选择当前仓库已有的 DragonOS 测试启动方式，至少执行 `test_oom_killer`

Expected:

- 不出现同一 RIP 无限 fault
- `waitpid` 观察到 victim 因 `SIGKILL` 退出
- 非 OOM fault 回归不被破坏
