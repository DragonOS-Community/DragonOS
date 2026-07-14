# Virtiofs 非 DAX P0/P1 实施计划

## 1. 范围和当前判断

本文只规划普通 virtiofs 请求/响应路径，不讨论或依赖 DAX。第一阶段先建立可复现、可归因的性能基线（P0），再根据证据完成普通缓存读的正确性修复与请求合并验证（P1）。写回批处理、元数据缓存、多 request queue、bridge 轮询策略和 DAX 均不属于本阶段。

基线固定为 `origin/master` 的 `14606afc429df342bc04f28d7b428fc42931199d`。该版本的 `63506c1f`（#2065）已经实现连续缺页 run、background READ、direct DMA into page cache、background/congestion credit 和有界流水线。因此 P1 不是重写 readahead，而是回答：

1. 为什么 4 KiB syscall 的 16 MiB 顺序读曾观测到 3954 个 FUSE READ，128 页窗口没有稳定转化为大请求；
2. 一次启用 detailed stats 的 128 KiB/1 MiB 样本为何在 transport 归零后没有及时返回，它是 workload、stats snapshot、串口输出还是上层等待造成；
3. signal 后复用 mount 是否稳定留下 pending/page-cache 污染，还是早期样本中的偶发相关性；
4. 现有实现是否正确处理未协商 `FUSE_ASYNC_READ` 和 `max_readahead=0`。

在证据命中具体 hot path 前不修改它，不通过减小 block size、关闭 readahead、跳过 fsync/close、忙轮询、无界 retry 或自动重启 mount 掩盖问题。如果 P0 证明超时只来自 benchmark/stats，P1.1 可以以“无需 hot-path 修复”结束。

## 2. 已确认事实与尚未确认的推论

### 2.1 本地环境

- DragonOS `14606afc`，x86_64，2 vCPU，2 GiB；
- QEMU 8.2.2/KVM，virtiofsd 1.10.0，`cache=auto`；
- QEMU 未配置 cache window、guest 报告 `cache_region_len=None`（non-DAX），1 个 request queue，
  vring 128，SG 上限 124 页；
- bridge 使用中断唤醒；latest-master 的一个样本曾有 1 次 queue-full、候选样本为 0，均不足以证明
  queue saturation，也没有 polling fallback 证据。

每个正式样本必须重新记录这些值；它们不代表 CubeSandbox 环境与本机相同。

### 2.2 4 KiB/16 MiB 基线

三个未封印 exploratory 观测的顺序写约 3.08--3.20 MiB/s，顺序读约 5.06--5.17 MiB/s。一次 detailed 样本包含：

- FUSE READ 3954 次，FUSE WRITE 4096 次；
- readahead batch 4096 次，request 3954 次，window peak 128 页；
- request inflight peak 1；queue-full 为零。

这些观测提示本地吞吐低且该样本的 READ 接近页数，但尚不能证明分裂发生在 window、reservation、协商限制还是等待时序；detailed stats 本身可能影响结果，且没有 runner v3 artifact 时不得作为正式 baseline。

### 2.3 128 KiB/1 MiB 非稳定超时

一次 fresh boot、启用 `VIRTIOFS_STATS_PATH` 的样本超过 10 秒没有返回。GDB 显示两个 CPU 均 idle；8 个 WRITE、9 个 READ 和总计 27 个请求已回复，transport/background inflight 均为零，READ response used bytes 合计为 1 MiB 加 reply headers。这只能排除“virtiofsd 仍在处理已计数请求”，不能证明睡眠点是 cached READ；也可能是 stats snapshot、fsync、close/release 或其他上层等待。

两个反例降低了该推论的置信度：

- fresh boot、不设置 stats path 的相同 workload 正常完成：写 80.1 ms、读 83.3 ms；
- 同一 fresh mount 随后启用 stats 也正常完成：写 23.9 ms、读 22.1 ms，但 after snapshot 仍看到 1 个异步 request/inflight，证明现有 delta 没有 quiescence 边界。

一次 `Ctrl-C` 后复用 mount 的后续样本也曾卡住，但在 phase marker、stats 模式和独立重跑完成前只视为候选相关性。

### 2.4 已确认的观测基础设施缺陷

1. `stats::format_snapshot()` 每次读取都永久把 `DETAILED_STATS_ENABLED` 置为 true，没有关闭/reset；正式性能轮会被共享原子 RMW 污染。
2. benchmark 的 READ zero-copy 断言要求 reply-vector ownership transfer，但 direct page-cache DMA 正常不会增加该 counter，正确数据会被误报失败。
3. after snapshot 不等待 speculative READ/async RELEASE 稳定，delta 会漏计或串入下一样本。
4. benchmark 把 write/fsync/close/open/read/close 合在一个 workload，缓存态、阶段和失败点不清楚。

### 2.5 P0/P1 fresh-VM 复测（2026-07-13）

P0 helper、显式 stats mode、direct-DMA 守恒计数和 evidence runner 已能给出 exploratory 输出，但早期
“120 READ 已证明合并完成”的结论已被后续 fresh VM 观测否定。当前只列出以下同配置复现目标：

| 版本 | 文件/用户块 | data-loop | 用户 syscall | FUSE READ | 单页 READ | direct bytes |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| `14606afc429df342bc04f28d7b428fc42931199d` 基线 | 1 MiB/4 KiB | 190.975 ms | 257 | 131 | 123 | 1,048,682 |
| 未封印实验 worktree：reservation + manifest + signal | 1 MiB/4 KiB | 191.877 ms | 257 | 130 | 122 | 1,048,576 |

第二轮未封印组合样本中，direct requested/completed 恰好等于文件大小，符合 manifest 控制面字节已被
移出窗口的预期，且
owner 最终归零；但它把三个候选改动混在同一未封印 worktree，只能说明未观察到数量级改善并拒绝
“根因已修复”，不能单独估计 reservation 的效应。当前只确认 daemon 收到的 READ 高度单页化，具体
拆分层仍未知，P1 不得标记完成。后续每个样本必须保存候选 tree/diff SHA。上述轮次没有 runner v3
finalize artifact，只能作为 exploratory 复现目标，不能作为正式 baseline。

另外，Linux host reference 的 sync/async、mmap、short EOF、sent-interrupt 聚焦用例通过；DragonOS
guest 中 short EOF、stats control 和候选 sent-interrupt 通过。master 的 interrupt 约 1.065 秒返回
`EINTR`，符合 pre-fix 失败断言但不符合目标 Linux 语义；候选约 1.045 秒通过，均未 hang。mmap
readaround 在 master 与候选上均
超过 10 秒，是独立的既有 correctness blocker，不能用 host 结果替代 guest 验证。

## 3. Linux 6.6.139 语义与分层不变量

Linux `fs/fuse/file.c:918-1037` 用 generic readahead 提供锁定页批次，单个 READ 最多包含 `min(max_pages, max_read / PAGE_SIZE)` 页；协商 `FUSE_ASYNC_READ` 才 background send，否则同步发送同一合并请求。`fs/fuse/dev.c:280-408` 定义请求 pending/sent/interrupted/finished 的中断与退休关系。

DragonOS 可以保留 reservation、direct DMA 和 open lifetime pin，但必须保持：

1. 数据和未写尾部初始化后，demand 页才能从 Loading 发布为 UpToDate。
2. logical pending result/waiter、background credit、open pin、DMA owner 和 page-cache marker 是不同生命周期，分别有唯一终结责任。
3. Submitted DMA 只有 used-ring pop 或 reset 后 exact detach 才能退休；disconnect 的逻辑完成不能提前释放或读取它。reset timeout 允许安全 quarantine，不能强求“全部归零”。
4. speculative READ 的失败或延迟不能污染不依赖它的 foreground read。
5. short read/EOF、truncate/invalidate generation 和晚到 reply 不能发布过期页。
6. signal 返回后，未提交请求可以取消；已提交原 READ 仍由 transport owner 退休。FUSE_INTERRUPT reply 不等于原 READ 已退休。
7. request 不超过 `min(max_read, max_pages * PAGE_SIZE, reservation capacity, SG payload limit)`。
8. `max_readahead=0` 禁止 speculative window，但 demand read 仍至少覆盖所需页，不能把有效零解释为默认 128 页。

强制对照：

- Linux：`fs/fuse/file.c:918-1037`、`fs/fuse/inode.c:1230-1320`、`fs/fuse/dev.c:280-408`；
- DragonOS：`fuse/inode/file.rs:1126-1558`、`fuse/conn.rs:244-299,481-610`、`fuse/conn/request.rs:259-311`、`page_cache.rs:394-680,1907-1912,3116-3144`、`virtiofs/bridge.rs:1230-1310,1643-1711`。

## 4. P0：可复现与可归因基础设施

### P0.1 拆分 benchmark 生命周期

修改 `user/apps/virtiofs_bench/virtiofs_bench.cc`：

1. 用 `WorkloadSpec`/enum 增加 `prepare`、`sequential_write`、`sequential_read`、`cleanup`；兼容的 `sequential` 复用同一实现。path/zero-copy 断言不再依赖易漏分支的字符串比较。
2. dataset 只能是 mount 下受约束的相对名；用 `openat`/`O_NOFOLLOW`，prepare 写临时文件、fsync 后 rename，cleanup 只删除带 ownership manifest 的对象。
3. dataset 由 seed+绝对 offset 生成非恒定内容；manifest 保存 magic/version/size/block/seed/hash。read 逐块校验 offset pattern、总长、EOF 和摘要。
4. 正式轮只在 phase begin/end 和最终汇总输出 run id、pid、monotonic timestamp、结果并 flush；逐 syscall 只更新预分配的低扰动 `last_syscall={ordinal,offset,requested,returned,errno,state}`，不读时钟、不刷串口。逐 syscall stderr/ring 仅诊断轮启用且不用于吞吐。
5. 用 `CLOCK_MONOTONIC`；open、data loop、fsync、close、校验分别计时。内容校验和 prepare 不进入 data-loop 吞吐。
6. 记录真实 syscall/short-I/O 次数；普通 EINTR 重试与 signal-pollution case 分开。拒绝 `block_size > SSIZE_MAX`、零值和算术溢出；partial I/O 必须保证进度。
7. 为 direct page-cache DMA 增加独立且守恒的 request/bytes counter；READ 验证接受 direct DMA，不再错误要求 reply-vector transfer 大于零。
8. parser/dispatch/path/assertion 增加 host 单测。

### P0.2 显式统计模式和低扰动快照

FUSE 专用统计放 `kernel/src/filesystem/fuse/stats.rs`；通用 Loading wait 放 `kernel/src/mm/page_cache_stats.rs`，禁止 page cache 反向依赖 FUSE。

1. 增加显式 `off/light/detailed` control；snapshot read 不改变模式。always-on 只含 current 和关键生命周期守恒；light 增加全局 READ count/requested/completed bytes 与大小桶；detailed 再增加逐 opcode、copy 和 allocation 分项。当前计数器不是 connection/session scoped，所以 P0 限定 fresh VM、单 virtiofs device、单 mount、无其他 guest workload；不满足隔离条件的样本无效。多租户 CubeSandbox 归因若需要并行观测，再单独设计 connection epoch，不能在本文假称已有。
2. 同一代码比较 off/on 开销；若吞吐差超过 2% 或 p90 延迟差超过 3%，性能结论只采用 off 数据。
3. 记录 cached-read calls、demand/planned window pages，以及 READ requested bytes/pages 的 total/max 和 1、2--4、5--16、17--32、33--64、65+ 页桶。
4. 记录 foreground pending wait、通用 Loading wait 的 enter/wake/recheck/interrupt/current/peak。
5. 建立 sleep 到 producer 的关联：session/connection epoch、pid、unique、nodeid、稳定 cache id、page/entry、reservation id、DMA state、queue kind/slot/token；runner 只聚合 manifest 指定的目标 session/cache。
6. reservation 分别统计 prepared/submitted/publish、rollback attempt/result、Submitted `EBUSY`、identity mismatch、reset-retired、quarantine；credit 统计 acquire/release/underflow。
7. signal 后分项统计 original READ 正常退休、disconnect 逻辑完成、reset exact-detach 和 quarantine。

并发现场使用有 CAS claim/唯一 owner 的固定槽；identity payload 全部使用原子字段，writer 填充后 Release 发布 generation，reader Acquire 双读校验，冲突只增加 dropped-snapshot，不能覆盖活跃槽。裸字段加 seqcount 不满足 Rust 并发安全。关键守恒计数 always-on；mode 只能在目标 session quiescent 时 Release 切换并开启新 run epoch，已开始 owner 固定其采样 epoch 直到退休；活跃 writer 期间禁止 reset。统计读取不持 FUSE/page-cache hot lock，不改变等待条件。

### P0.3 runner、缓存协议和统计边界

在 `tools/virtiofs/` 增加非 DAX runner：

1. 输出 kernel build id/SHA、disk image SHA、QEMU/virtiofsd 版本和 SHA、完整 argv、host kernel、vCPU/RAM、KVM/TCG、queue/SG、host FS、cache mode、CPU governor/frequency/load、NUMA/affinity。
2. 缓存态记录为二维 `guest_cache={cold,warm}`、`host_cache={cold,warm,unknown}`。read dataset 在测量 VM 启动前由 host 原子生成+fsync，或由专用 prepare VM 生成并关机；正式 read VM 只读，禁止在同一 boot prepare/write。manifest 记录 inode/size/hash/mtime并在 read 前后复核。write case 使用不同 dataset。每个正式 cold-guest 样本 fresh boot；无法 drop host cache 时写 `guest-cold/host-warm|unknown`，不伪称全冷。
3. 每个 cold-guest read 必须看到 READ opcode 增长和首个 demand miss；9 次样本必须是 9 个独立冷态，不能同 VM 连读刷 page-cache hit。
4. workload 停止产生新请求后、cleanup/after snapshot 前有界 quiesce：在上述隔离 VM 中检查全局 foreground/background/request/bridge inflight 与 Loading/reservation；counters 在稳定窗口内不变。超时则样本失败并留现场，before 同样在 quiescent 后采。
5. timeout 先抓 GDB CPU backtrace、原子快照和 stats，再发 signal；随后验证同 mount 最小 metadata/read。污染复用只作独立 correctness case。
6. 保存 stdout/stderr、stats before/after/delta、serial、daemon log 和 manifest；失败不删证据。
7. pilot 先估 CV 并冻结 timeout、目标效应、样本数和回退门限。1 MiB 高重复测正确性，16 MiB 测全部 block，256 MiB 只确认 4 KiB 和触达上限的代表 block。报告原始值、median/IQR 和 bootstrap CI。
8. A/B/A 固定为时间分层 `A1>=9、B>=9、A2>=9`，revision 顺序不伪称随机；每个时段内随机 workload/
   dataset 次序，并预注册 sample id/seed。A1/A2 漂移超过 10% 或 bootstrap CI 显著即整轮作废；无漂移
   才按时段分层合并 A。候选改善的 bootstrap 95% CI 下界必须达到冻结门槛，而非只看点估计。
9. 增加同 QEMU/virtiofsd/host share/cache/CPU/RAM 的 Linux 6.6.x guest 与 host backend read 对照，并记录 Linux 请求大小/数量；仅用于性能归因，不是 DragonOS 正确性门禁。
10. `collect` 先把 serial/stats/cmdline 等源文件复制到同目录临时 case，复核源 fingerprint/hash 未变，
    再对副本执行全部静态语义验证并原子 rename 发布；生成的 collector process context 让 `finalize`
    在进程退出后仍能重放 transcript/stats/argv、二进制、cwd、启动时刻和 non-DAX 绑定语义。该门禁已
    通过 skipped/failed 正向封存、collect 中途源变化拒绝、以及重算 hash 后 argv 语义篡改拒绝测试。

### P0.4 三环境执行与资源指标闭环

1. DragonOS QEMU orchestrator 负责 revision/artifact 切换、fresh boot、sample id、预注册顺序、失败留档和
   27 个以上 case 的聚合；aggregator 输出原始样本、stratified bootstrap CI、漂移判定和机器可读验收报告。
2. CPU/MiB 以 workload 前后差值计算：QEMU 与 virtiofsd 读取 `/proc/<pid>/stat` 的 user+system ticks，
   virtiofsd 必须包含 worker 线程；guest 使用同一固定来源的 user/system/idle 计数。分别报告
   `cpu_seconds / completed_MiB`，guest 按所有 vCPU 的测量窗口 `delta(user+system)` 求和并排除 idle；
   不得把进程启动时刻或 host load 当 CPU time。
3. Linux guest 使用与 DragonOS 相同的 dataset manifest、workload、block size、fresh-guest/cache 分类、
   QEMU/virtiofsd/host share/vCPU/RAM 和 artifact schema；必须用 QEMU 设备参数或 guest 设备能力证明无
   cache window，并通过 tracepoint/tracefs 或等价可核验来源记录 READ size/count。旧的 `--workload all`
   手工示例不属于正式 reference。
4. CubeSandbox 固定 guest kernel/image SHA、busybox 配置、share/backend/cache、单实例隔离与容器内命令
   workload。按专用测试流程执行 `cubecli multirun /root/cubecli-busybox.json`，记录 sandbox ID，采集
   CubeShim、CubeVmm `vmm.log`、Cubelet request log、virtiofs backend、IRQ/wakeup 与 CPU 指标；成功或
   失败都执行 `cubecli unsafe destroy <sandboxID>` 并记录 cleanup。若全局 counter 无法隔离该实例，样本
   无效，先增加 connection/session epoch，不能用多租户噪声归因。
5. 三环境共用一个 schema 和 workload id，但结果分层报告；本地 QEMU 根因不得直接外推 CubeSandbox。
   每个 runbook 必须给出 prepare/run/timeout-collect/finalize/cleanup 的实际命令和失败退出码约定。

P0 门禁：phase/syscall/wait identity 可定位；fresh 和 signal-pollution 可独立重复；stats 模式、quiescence 和二维缓存态可验证；direct DMA 断言正确；READ 数/大小可解释；内容校验通过；A/B/A aggregator、
CPU/MiB、协商上限、Linux reference 与 CubeSandbox runbook 均可执行并通过破坏性负测。

## 5. P1：普通缓存读正确性和请求合并

P1 串行执行：先按证据判断 P1.1 是否需要修，再评估 P1.2；不能把多个候选一起改。

### P1.1 completion、signal 与 DMA 生命周期

候选点仅包括：`wait_read_pages_once()` 的 signal/completion 竞态、reservation 是否遗漏 Loading 发布、`commit_page_with()` 是否等待 orphan marker、open pin/pending 消费/interrupt 的 owner 顺序。P0 未命中的候选不改。

实施前写分层状态表，分别列 PendingCompletion/waiter、pending/processing queue、credit、open pin、DMA target、Loading marker 的 owner、允许终态和先后关系，并映射 Linux `PENDING/SENT/INTERRUPTED/FINISHED`。覆盖：original reply 与 signal claim 竞态；interrupt 未排队/成功/ENOSYS/EAGAIN；never-submitted/submitted；disconnect；used-ring pop；reset exact detach；quarantine。

修复优先复用 `WaitQueue::wait_until[_interruptible]` 的注册后重查原语。禁止 polling/sleep/unbounded retry/遇 Loading 丢页；禁止把通用 Loading wait 改为 interruptible 后由非 owner 回滚他人的 DMA；不得持 `FuseConn.inner` 进入 page-cache completion，不得持 page-cache inner 睡眠。发布顺序为：数据/尾部初始化、identity validate、Loading->UpToDate、wake。

测试分层：

- 内核状态机/WaitQueue 单测确定性覆盖注册—重查—sleep 与 completion 交错，不靠 sleep 碰撞或生产 hot-path test hook；
- guest dunitest 用 daemon barrier 覆盖 signal-before-reply、reply-before-signal、unsupported interrupt、复用 mount、short read/EOF、daemon error、disconnect/unmount/reset；
- DMA 用例拆成 Prepared/never-submitted、Submitted/normal pop、Submitted/reset exact-detach、reset失败 quarantine；守恒式断言无非法 pending/credit/pin/Loading 泄漏，合法 quarantine 断言 owner仍被隔离。

### P1.2 证明并完善合并

P1.1 通过或判定无需修改后，用大小桶选择唯一证据命中的路径：

- planned window 大而 reservation run 小：检查 ready/Loading 冲突、readaround state 和 demand/speculative 边界；
- run 大而 FUSE request 小：检查 `max_read`、`max_pages`、SG limit 的协商和取整传播；
- 大 syscall 能合并而 4 KiB 不能：修 per-open window 推进和 speculative ownership，不建第二套 readahead；
- 合并已经正确：不为了 inflight 指标人为拆请求；只有多个独立且受上限截断的 run 才调整现有 bounded pipeline；
- 协商 `FUSE_ASYNC_READ` 才 background pipeline；未协商时同一合并 run 同步发送，并保持 reservation/尾部清零/错误语义；
- `max_readahead=0` 时 speculative 为零而 demand 正常；覆盖零、小于页、非页对齐和未协商 `FUSE_MAX_PAGES`。

P1 修改的是 syscall read 与 mmap 共用的 fill engine，必须同时回归 mmap。

### P1 门禁

1. 1 MiB correctness case 每版本 20 次、每次 watchdog 10 秒，零 hang/零内容错；signal 后同 open、重新 open、复用 mount 都符合状态表。
2. 每轮计算 `effective_pages = min(max_read / PAGE_SIZE, max_pages, sg_payload_pages)`；startup/EOF 裕量最多 2 个 READ，稳态 payload p10 不低于有效上限 25%、median 不低于 50%。
3. READ reply/daemon/host bytes 相对 file size 的放大量受门限约束，不能靠过量 speculative I/O 降低 request 数；同时报告 foreground completion latency。
4. 4/64/128/256 KiB block 均无 request-complete 后睡死；守恒计数满足，quarantine 与真正泄漏区分。
5. A/B/A 固定 `A1>=9、B>=9、A2>=9` 个 cold-guest 样本，按 performance plan 第 10.1 节的漂移
   与分层合并规则，以 bootstrap 95% CI 证明 READ 数和 median latency 至少改善 25%；p90 退化不超过
   10%、CPU/MiB 无解释退化不超过 20%。
6. 通过内核单测、focused dunitest、完整 FuseExtended、普通 read 与 mmap（顺序/随机 fault、munmap/close with speculative、truncate/invalidate race、daemon error/disconnect）、`make kernel` 和 DragonOS guest 验证。

## 6. 实施顺序和停线条件

第一道授权门禁：本计划终审通过后立即停止，用户确认前不得开始或继续第 1 步 P0。当前工作树中的
hot-path 改动仅为未批准实验候选。P0 全部门禁完成、根因唯一化并形成 P1 最小变更包后，执行第二次
独立终审并再次等待用户对 P1/hot-path 的单独确认。

1. 实施 P0 benchmark、stats control/direct-DMA counter、runner 和低扰动关联快照；
2. fresh boot 重跑矩阵，冻结 P1 根因记录；
3. 只实施证据命中的 P1.1，或记录无需 hot-path 修复；
4. 重采大小桶，必要时实施 P1.2；
5. 重新对照 Linux 6.6.139，复审用户语义、错误路径、锁序和生命周期；
6. 执行格式化、focused tests、`make kernel`、QEMU correctness 和 A/B/A；
7. 独立对抗评审架构、并发安全、性能归因、测试充分性和 workaround 风险，修复有效发现。

遇到任一情况立即停线回到取证：root cause 与快照矛盾；需要改变通用 WaitQueue 但无法证明所有调用者安全；只能靠 polling/timeout/业务层无界或定时 retry；性能提升伴随内容、EOF、truncate、interrupt、mmap 或 teardown 回归；fresh VM 不能重复结果。符合 Linux 语义、释放 MM guard 后等待的 `VM_FAULT_RETRY` 不属于此处禁止的 workaround。

## 7. 2026-07-13 实施与复核状态

已实现的 P0 子集包括 split-workload helper、pattern/hash、manifest 预读、目录 fsync、固定
`virtiofs_bench_last_syscall` seqcount 快照、off/light/detailed、direct-DMA 与 owner 守恒，以及带固定计划
封印和 case artifact 哈希的 runner。runner 能拒绝旧 run、错误 artifact、自相矛盾的 transcript、错误
QEMU/virtiofsd/kernel/disk/CPU/RAM/accelerator 和非目标 mount。它是误改可检测的采集协议，不是抵御
同权限/root 主动伪造的签名系统；单次 finalize 也不替代 A/B/A 汇总器。

第一轮 latest-master guest 的 1 MiB/4 KiB light 样本仍为 190.975 ms、257 个用户 read、131 个 direct
READ，其中 123 个为单页；requested/completed 为 1,048,682 bytes。额外 106 bytes 来自测量阶段再次
读取 manifest。该未封印观测足以否定此前“合并已经完成”的结论，并形成了两个待验证假设：

1. 区间 reservation 在后页冲突时回滚已分配前缀，恢复路径却错误等待 `run_start`，会退化到逐页读取；
2. manifest 控制面读取没有真正从计数窗口排除。

实验性修改为：先分配 unlinked pages，再在单次 page-cache 锁内验证并发布整个 run；`EEXIST` 后从真实
cache state 重建而不调用 `commit_page(run_start)`；preflight 得到的 manifest 直接传入 read phase。同步
READ 另按 Linux 6.6.139 `request_wait_answer()` 增加 Pending/Submitting/Sent 屏障：普通信号只标记
interrupted，`virtqueue.add` 成功后才允许排 INTERRUPT，fatal signal 才取消 never-submitted 请求，
请求一旦被 transport 从 conn pending 取走，QueueFull 回退也不再允许 fatal waiter 删除，submitted DMA
始终等待终态。ASYNC_READ foreground wait 使用同一 Sent 门禁。

未封印 fresh DragonOS VM 组合样本符合 manifest 隔离有效的预期，但 whole-run reservation 没有降低 130 个 READ/122 个
单页 READ，也没有改善约 191 ms 的耗时。因此该假设未通过性能门禁，暂不批准落地。Linux host
reference 的五个聚焦测试和 `make kernel` 通过；DragonOS guest 的 short EOF 与 stats control 通过，
sent-interrupt 在候选 guest 通过；mmap readaround 仍挂起，因此整体仍未通过 correctness 门禁。

runner 的 copy-then-validate/atomic-publish 和 finalize 语义重放已经完成正负验证。下一步顺序固定为：
完成 master、仅 manifest、仅 reservation、仅 signal、组合版本的单变量 fresh-VM 二分；按 performance
plan 第 10.2 节用 per-fault owner 证据定位既有 mmap hang；最后才做关联式四层取证。mmap blocker 与
单变量矩阵未通过时，所有数字只标 exploratory。

Linux 6.6.139 锁序复核还确认：其 generic `do_read_fault` 不在 fault-around 外层持 invalidate lock。
mmap readahead 自己在预分配并提交 `read_pages()` 的窗口持一段 shared invalidate，提交完成后释放；
随后 demand folio lookup/create、folio lock/identity/uptodate 与 truncate 复核，以及必要的同步
`read_folio`/错误处理再持另一段，完成、错误或 retry 出口才成对释放；二者互不嵌套。DragonOS 当前宽锁会在
writer-preference 下产生“已持 reader、writer 排队、递归 reader 睡眠”的结构性风险，pending short-read
truncate 还可能形成 read-to-write 自升级。该问题进入独立单变量验证项；一次仅下沉 read-fault 锁的
探索性试验未解除现有 mmap hang且已撤回，故不得把锁边界风险直接等同为已证明的 hang 根因。

同一轮 MM 审计还确认 x86 fault 入口把 `AddressSpace.write()` 持有到普通 FUSE cold fault 的同步等待结束，
而 Linux 会在可能 I/O 前释放 mmap lock 并以 `VM_FAULT_RETRY` 重试。线程型测试 daemon 因此可能形成
“fault 持 mm write lock 等 daemon；daemon copy-to-user 缺页等 mm write lock”的闭环。该闭环必须先用
daemon-buffer prefault、独立 AddressSpace daemon 和 hot-cache/cold-cache 三组单变量实验动态证伪；
prefault/改 daemon 模型只能用于诊断，绝不能作为修复。若命中，正确候选应使用精确 pending/entry
identity 的 `FaultRetryWait`，在释放 AddressSpace guard 后等待，并在 retry 时重新验证 VMA、pgoff、
cache entry 和 reservation generation。mmap correctness 门禁未解除前不执行正式性能 A/B/A。

四层取证不能只靠无关联聚合桶。仅在 detailed 诊断模式为每次 fill 分配稳定 `fill_id`，以 `(connection_epoch, cache_id, inode,
generation, fill_id)` 贯穿 planned window、逐页初态、Missing run、reservation、FUSE unique、submitted
payload/SG 和 completion，并同时记录 `max_read/max_pages/max_readahead/FUSE_ASYNC_READ` 与实际 SG
上限。只有同一 `fill_id` 的相邻层守恒后才允许归因；具体决策规则和冻结数值门槛统一采用
`virtiofs_non_dax_performance_validation_plan.md` 第 10 节。
light/off 模式不得执行逐 fill identity 写入，只保留预先证明低开销的聚合计数。

只有 direct bytes 等于 file size、READ count/payload p10/median/读放大量达到冻结门槛、内容/hash/
owner 守恒和全部 focused guest
dunitest 同时通过，才执行独立 off/light A/B/A、Linux guest performance reference 与 CubeSandbox
复现。Linux host FUSE 聚焦测试只验证语义，不作为性能 reference。未满足任一项即回到取证，不以
扩大窗口、重试或 polling 掩盖问题。

正式实施授权门禁：在根因由完整 wait-for 链和最小 A/B 证伪唯一化、Linux 6.6.139 锁/生命周期对照、
owner 状态表、最小变更范围与测试矩阵通过独立终审后，本计划立即停下并等待用户确认。确认前不得批准
或继续叠加 FUSE、page-cache、MM fault、signal lifecycle 或 bridge 热路径候选。

## 8. 2026-07-14 P0/P1 实施状态

用户已批准实施 P0、P1；第 6、7 节的“未授权/停止”文字是历史记录。candidate 已通过直接 QEMU guest
correctness/performance 验收，并已重放到验收时最新 `origin/master`。早期 branch-cut 只称探索性基线；
为保证 baseline/candidate 使用相同 helper，synthetic P0-only baseline 与 candidate 均基于同一 master
构造。期间发现的 proc namespace magic-link mount projection correctness 修复同时进入双方，避免把
“baseline 无法启动”冒充性能收益。

P0 已实现 split prepare/read/write/cleanup、确定性 pattern/hash manifest、失败可回滚且并发串行化的
dataset 发布、off/light/detailed、off 模式 always-on owner/quiescence、direct-DMA 计数、clean-tree build
manifest、runner v4、study v3 candidate-effect/mode-overhead A1/B/A2 和原始 CPU 快照重算。runner/study
还会拒绝被篡改的 sealed `runner.sh/common.sh`，并把 kernel、disk、helper 的稳定双哈希绑定到 build/run
manifest。host transcript、runner 负测和 study 单测已经通过；P0 baseline 与 candidate 的 CubeSandbox
对照已完成，latest-master candidate 的直接 QEMU correctness/performance 验收也已通过。
benchmark 的 split `sequential_read` 自动门禁也已收紧为：light 下全部已解析 READ（detailed 下额外
核对全部 opcode 15）必须与 direct-DMA request/completion 一一对应、DMA 请求/完成字节守恒且不超过
workload，detailed 模式下 reply transfer/copy 增量为零；warm-cache 的零 READ 或部分命中均合法。
runner 只允许 warm-cache 使用不采集 stats 的 performance 模式；light/diagnostic 结构样本因此必为 cold，
并由 collect 门禁强制 DMA 字节等于完整 workload。

P1 candidate 包含：

1. 普通 cache hit 不滑动窗口；只有 async boundary 或 expected miss 推进非重叠窗口；random miss 只修
   demand，严格前向的相邻 miss 才重启顺序窗口。
2. `FUSE_ASYNC_READ=0` 对同一合并 run 同步逐 chunk 提交；后续 chunk 错误时返回已 Ready 的连续前缀，
   零字节才传播延迟错误。
3. DMA range 先批量预检、完整分配，再在一次 page-cache 锁内原子发布 Loading；reservation 冲突等待
   实际冲突 entry。只允许 `floor(max_read/PAGE_SIZE)` 个完整 DMA 页，禁止把未传输页尾清零后发布。
4. fill 操作在调用方只持一次 invalidate read guard；reservation 不再递归获取 writer-preferring
   semaphore，避免 truncate writer 插队形成读锁自死锁。
5. per-open snapshot 缓存协商后不变的 read flags/limits，readahead state 用 SpinLock 保护短临界区。
6. buffered read 按 Linux 6.6.139 在 `AUTO_INVAL_DATA` 或跨 cached EOF 时刷新属性，使用当前 fh 和
   `FUSE_GETATTR_FH`，且不重复 open 时已完成的 mount permission 检查；copy 后复核 attr version/EOF。

两轮各 8 角色 bug-hunter 和最终定向复核发现的问题已逐项修复，其中 DMA reservation 是并发正确性修复，
不能冒充已证明的性能根因。dirty 中间候选曾达到 off 50.206 ms，但恢复 Linux AUTO_INVAL 语义后的
clean 最终候选为 off 约 95.1--95.5 ms、light 97.747 ms；详细样本为 6 个逻辑 batch、9 个 direct-DMA
READ、1 MiB 字节守恒、无 GETATTR。16 MiB/4 KiB clean smoke 为 1545.585 ms（约 10.35 MiB/s）。本地
文件系统同形状为 5.010 ms，而 1 MiB 单次大 read 为 8.031 ms，说明剩余差距主要在渐进预读批次的
请求完成/唤醒链，仍需逐请求时间线唯一化。这些 smoke 不替代正式 A/B/A。

focused guest stats dunitest、random、concurrent、EOF/error、拒绝 `FUSE_ASYNC_READ` 的严格正负测和
mmap fault 回归已通过；完整 `FuseExtended` 60/60、`FuseCore` 5/5 也已通过。mmap 根因是 fault 线程持
`AddressSpace.write()` 等待同 AddressSpace 内的 daemon，而 daemon 在 `/dev/fuse` copy-to-user 缺页时
等待同一锁；修复使用 Linux 等价的 lock-free `VM_FAULT_RETRY`，retry 后重新验证 VMA/backing identity，
并将 fault-before-around 限定到 FUSE，避免改变 ext4/FAT/tmpfs 快路径。

仍未通过的门禁：每版本 20 次正式 read correctness；off/light/off 与 P0/P1/P0；本地 CPU/MiB。
Linux 6.6.139 fresh KVM reference 已严格 pack+verify：1 MiB/4 KiB 为 1.793 ms、11 个 FUSE READ、
requested bytes 精确为 1 MiB。CubeSandbox 正式同 workload A/B 也已双包 verify：P0 baseline
902.540 ms（1.108 MiB/s），P1 candidate 198.165 ms（5.046 MiB/s），吞吐 4.554 倍、延迟降低
78.044%；两组 image/helper/request/config/checksum 相同。远端 kernel/image 已恢复且 running=0。
这些跨环境结果不替代尚待执行的本地 94 fresh-VM correctness/mode/effect 矩阵。串口批量输入造成的
TTY spinlock stall 是独立非 FUSE 事件。

本轮完全排除 DAX。即使 non-DAX 最终通过，也不能据此宣布 Issue #2019 的共享内存 cache window、mapping
生命周期或 DAX 性能目标完成。
