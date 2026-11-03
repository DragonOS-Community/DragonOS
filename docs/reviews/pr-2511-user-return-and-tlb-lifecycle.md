# PR #2511：用户返回工作与终态 TLB 请求

## 基线与取舍

基于 master `357042f3b`，重新整理原分支 `c59aa2328`。

#2564 已删除临时开中断的 shootdown 修补，并提供 IRQ-off 入站服务和无锁
CSD 发布/认领。保留此协议，不恢复旧分支的 IRQ-on 发起契约、诊断计数、
延迟日志或全局锁阻塞等待。

先结合 Linux 6.6.139、问题现象和 DragonOS 调用路径研究；方案经过独立
复审后实施，再次核查并发、生命周期、错误路径与性能。修复不改变已有
COW、页表回收或 TLB 代数，不通过修改 gVisor 测试绕过问题。

## 剩余问题与方案

### 用户内存访问与 rseq 通知

master 的 `switch_finish_hook()` 在 IRQ-off 切换尾部写 `set_child_tid`，
且每次切换都取得线程写锁检查地址。用户页可能首次分配或 COW，不能把
这种访问放在切换原子区内。Linux `kernel/sched/core.c::schedule_tail()`
在结束切换、允许抢占后 best-effort `put_user()`。

fork 安装地址时同时发布 `NEED_CHILD_TID`，首次返回用户态前取走地址并清
标志，释放线程锁后开中断写入。操作先于 memory.high、信号和 ptrace 工作。
无效地址不使 clone 失败、不额外杀死子进程；vfork 父任务仍等待 exec/exit。
删除切换尾部整段检查，普通切换不再承担这次线程锁开销。

原 PR 只把 rseq resume 改为 IRQ-on，却仍在 handler 末尾清 `NEED_RSEQ`。
捕获 CPU ID 后的抢占/迁移可以重新发布通知，末尾清位会丢失该通知，导致
旧 CPU ID 留到用户态。与 Linux `include/linux/resume_user_mode.h` 一样，
在返回边界 IRQ-off 时先消费通知，再 IRQ-on 执行 faultable handler。
handler 不消费通知，含错误路径与信号递送调用；unregister/exec/fork 的
生命周期清理保留。处理中再发布的通知由返回循环再次处理，不以关闭抢占
覆盖用户访问来规避竞态。

### CPU 不可逆停机与 CSD 栈指针

master 只选 Online CPU，但 `Dying` 之后还执行 RCU teardown 和架构关闭。
另一方面，目标被选中后可能进入 `Dead`，不再处理普通 IPI，发起者永久等 ack。
原 PR 发现 Dead 后直接退出，会留下指向发起者栈帧的 CSD 指针。

x86 目标筛选仍只做一次 Acquire 状态读取，接受 Online/Dying；其他架构保留
原有 online-only/SBI 行为。等待沿用每 1024 次自旋的入站服务分支，在其后
检查 Dead，原子清空 CSD.info，再 Release 发布 done，最后结束等待。
普通 ack 每轮没有新增远端状态读，也没有增加生命周期锁。

此协议依赖现有不可逆 stop：`complete_shutdown_current_cpu()` Release 发布
Dead，`cpu_up()` 拒绝重启 Dead，`stop_this_cpu()` 不恢复普通执行或消费 CSD。
这里不声称 Dead 发布后绝对没有取指/栈访问；不能回收正在执行的内核栈与
代码仍是独立不变量。未来增加 CPU 重启必须重新设计生命周期握手。

Linux `kernel/smp.c::smpcfd_dying_cpu()` 在离线过程中 drain 同步回调。
DragonOS 缺少 Linux 完整 hotplug 同步，不能只照抄 online mask，也不能把
Dying 视作失效完成。同步返回条件是完成失效或目标已不可逆终止，并且没有
CSD 保留本次请求的栈指针。

## 回归覆盖与边界

新增 dunitest `normal/user_return_work`，进入 whitelist 和 no_skip：

- CHILD_SETTID 私有页 COW，子任务在再次系统调用之前读取 TID，父页不变。
- 空地址和 PROT_NONE 地址保持 best-effort 语义。
- 注册私有 rseq 页后反复 affinity/fork，覆盖继承区域的 COW 更新。
- 仅 sched_selftest 构建的 root-only debugfs fixture，在真实 handler 捕获
  CPU ID 后调用正常 `on_migrate()` 重新发布通知，验证通知保留和第二次处理。
  这是确定的通知重发布窗口测试，不冒称真的迁移了 CPU。

扩展现有 TLB teardown fixture：使用私有 CSD，覆盖未认领/已认领请求退役、
旧对象生命周期结束后的空槽、重新发布和迟到 IPI 认领新请求。不会伪造
运行中 CPU 的 Dead 状态，也不宣称覆盖真实停机交错。

默认构建不含 rseq fixture、原子探针或 debugfs 入口。测试构建在 guest 中
要求相关测试实际执行；默认构建缺少 fixture 时的明确 skip 不作为覆盖证据。

## 验证记录

验证数据保存在工作区 `bin/pr2511/`。使用私有 QEMU 磁盘 overlay，未修改
基础镜像。基线和候选分别保存 ELF，避免以另一版本运行结果代替目标版本。
最终构建、guest 用例、负向对照和性能结果在 PR 描述中记录。

原始生命周期问题没有通过真实 CPU 停机在基线 guest 中重现；采用调用链、
状态发布/接收协议及私有槽负向对照作为证据。旧 PR 的实测结果不代替本次
基于最新 master 的验证。

### 已完成的 guest 与负向对照

- x86_64 默认内核和 mm_selftest+sched_selftest 内核构建成功。
- LoongArch64 构建通过；RISC-V 编译、链接及 DragonStub 安装通过。
- `FMT_CHECK=1 make fmt` 通过，含 x86 全特性 Clippy。
- 测试内核 4 vCPU：用户返回 5、TLB 4、clone3 18、affinity 17，共 44 项
  全部执行、无失败和跳过。
- 测试内核 2 vCPU：用户返回 5、TLB 4、进程信号/fork 12、ptrace 生命周期 5，
  共 26 项全部执行、无失败和跳过。
- 默认内核 4 vCPU：42 项执行通过；仅两项要求专门 feature 的 fixture 明确
  跳过，不把这两项计入默认内核覆盖。默认 ELF 不含 rseq fixture 符号。
- 同时恢复 handler 末尾清 NEED_RSEQ 和 CSD 退役不清 info 的负向内核中，
  两个对应测试分别失败，报告 notification_preserved=false 和
  csd_terminal_retirement=fail；其他相关用例仍通过。源代码随后恢复。

### 性能测量与限制

单个样本包含 10,000 次同 CPU pipe 往返和 1,000 次 fork/wait；每个配置
以基线、候选、候选、基线的顺序各启动两个私有 VM，每 VM 七个样本。
以下为每版本 14 个样本的中位数，单位 ns/操作：

| 配置 | 指标 | 基线 | 候选 | 变化 |
|---|---|---:|---:|---:|
| 4 vCPU | pipe 往返 | 18,070.5 | 17,932 | -0.77% |
| 4 vCPU | fork/wait | 524,324 | 512,827 | -2.19% |
| 2 vCPU | pipe 往返 | 18,805.5 | 18,593 | -1.13% |
| 2 vCPU | fork/wait | 606,357 | 626,847 | +3.38% |
| 2 vCPU，固定宿主 CPU、启动后等待 5 秒 | pipe 往返 | 18,673 | 20,525 | +9.92% |
| 同上 | fork/wait | 528,859 | 630,975.5 | +19.31% |

用户确认宿主同时有其他任务运行。4 核样本未观察到回退，但不把这些小幅差异
解释为可靠性能提升。2 核结果
明显不稳定：未固定宿主 CPU 的基线 fork/wait 样本为 0.385–2.534 ms，
候选为 0.412–1.368 ms；固定 CPU 后也没有消除波动。保留所有结果，不能
据此宣称所有配置均无回退；2 核 fork/wait 仍需在稳定环境中复验。

源代码层面的开销约束已经落实：普通返回仍是一次 flags load/mask；
child-TID 只在首次返回处理，移除了全部切换上的线程写锁；正常 CSD ack
每轮不增加状态读取，仅在既有 1024 次自旋慢路径检查 Dead；所有通知
探针都在默认构建中剔除。没有添加正常路径日志、全局诊断计数或热插拔锁。
