# DragonOS virtiofs 非 DAX 性能优化与验证计划

## 0. 当前状态（2026-07-14）

用户已经批准实施 P0、P1；授权实施不等于验收通过。当前状态是“P0 工具与 P1 candidate 已实现，等待
最终 clean artifact 已完成，正在补齐 guest correctness 和正式性能验收”。下文原 2026-07-13 的授权措辞保留为设计
历史，不能再解释为当前仍未授权。

| 项目 | 当前状态 |
| --- | --- |
| P0/P1 实施授权 | 已批准 |
| P0 工具实现 | 已实现，待正式验收 |
| P1 candidate | 已实现，待 correctness/performance 验收 |
| host tests、Clippy、kernel/user/image build | 已通过（已纳入 `origin/master=772fa84f`） |
| clean sealed artifact | 已完成；最终文档变更后重新封印 |
| focused guest stats dunitest | 已通过 |
| clean candidate smoke | 已通过 sequential/random/concurrent；见 0.1 |
| mode-overhead A1/B/A2 | 未完成 |
| candidate-effect A1/B/A2 | 未完成 |
| `FUSE_ASYNC_READ=0` guest runtime | 未完成 |
| Linux guest reference | 未完成 |
| mmap | correctness blocker |
| CubeSandbox | candidate smoke 已完成；正式 baseline/candidate 未完成 |
| 总体 non-DAX 验收 | 未通过 |

当前 branch-cut 为 `14606afc`；它只能作为历史探索性基线。最终 candidate 已 rebase 到
`origin/master=772fa84f` 并重新构建。正式 A1/A2 仍必须使用最终冻结的 master，不得把
旧数字冒充更新后的 baseline。

### 0.1 探索性结果（不得替代 A/B/A）

| 阶段 | 1 MiB/4 KiB data-loop | READ 结构 |
| --- | ---: | --- |
| branch-cut baseline | 约 190.975 ms | 131 READ，123 个单页 |
| planner 中间候选 | 约 186 ms | 10 READ |
| 去除重复前端 metadata 路径 | 约 95.626 ms | 未封印诊断样本 |
| dirty 中间候选 off | 50.206 ms | 未封印，随后恢复 Linux AUTO_INVAL 语义 |
| dirty 中间候选 light | 50.925 ms | 10 READ，最大 258048 bytes |
| clean 最终候选 off | 95.051--95.520 ms | checksum 正确，257 次 4 KiB syscall |
| clean 最终候选 light | 97.747 ms | 6 batch、9 direct-DMA READ、1 MiB 守恒 |
| clean 最终候选 detailed | 95.520 ms | 17 个总 FUSE 请求，无 GETATTR；9 个 READ 全部 direct DMA |
| clean 最终候选 off，16 MiB | 1545.585 ms | 约 10.35 MiB/s，checksum 正确 |
| clean 最终候选 off，1 MiB 单次大 read | 8.031 ms | 2 个 syscall（含 EOF read） |
| DragonOS 本地文件系统，1 MiB/4 KiB | 5.010 ms | 257 个 syscall；用于拆分 syscall 固定开销 |
| CubeSandbox `/bin/busybox`，约 1.0 MiB/4 KiB | 2.6--2.9 MB/s | overlayfs root；第二次读取仍慢，端到端约 0.85--0.88 s |

dirty 中间数字只能证明根因方向，不能作严格单变量归因，也不能再称为“当前候选性能”。clean smoke 已
证明：257 次 GETATTR 和 READ reply staging/copy 都不是约 95 ms 的原因；9 个 READ 恰好完成 1 MiB
direct DMA，且本地文件系统同 syscall 形状仅约 5 ms。1 MiB 单次大 read 约 8 ms 又表明剩余时间主要
落在多个渐进 readahead batch 的 submit/completion/wakeup，而非介质带宽或用户复制。该归因仍需 READ
逐请求时间线闭环。正式结论必须等待 A1>=9/B>=9/A2>=9、CPU/MiB、payload 分位数和 Linux reference。

CubeSandbox smoke 使用 kernel SHA256
`5d4cf38fcbfa8eacfe84e7ab7713ddbae296d0fd1c2ca88f6ab3e4ca716eaf8f`，sandbox
`f28e83446fd64ab8a81c5e61d3308739`。根挂载为 overlayfs，Cubelet `cube.fs` 配置 `cache=2`；guest 协商
`max_read=262128`、`max_pages=256`、request vring 1024。light 诊断区间因包含 BusyBox applet exec 和
stats 快照辅助命令，不能当作纯 dd 计数，但观测到 91 个 direct-DMA 请求/3,193,040 bytes 与 391 个
总 READ/6,761,618 bytes，表明容器命令路径仍有大量小读/多次 open 的碎片化，且不是本地 QEMU 的
vring=128 queue-full 模型。该 sandbox 已销毁，测试机原 kernel SHA256
`ae73725b8d77dd7875e55316972796dac474b405fd78417662fa41cb70b4997d` 已恢复；正式
CubeSandbox A/B 仍需把 helper/schema 放进 guest 并隔离 exec/overlay 控制面。

### 0.2 2026-07-13 设计终审历史

本计划已经过三类独立复核：Linux 6.6.139 语义对照、DragonOS 请求碎片化根因复核、测量证据链
对抗复核。结论是：当前可以请求批准实施 P0 证据闭环；P0 完成、动态根因唯一化并再次独立终审后，
必须第二次请求用户批准，才可实施 P1 单变量预读状态机修复。不得跳过任一道门禁，也不得把当前探索性
代码或单次数据称为正式性能结论。本文不包含、也不依赖 DAX。

本地 fresh VM 已确认 non-DAX（`cache_region_len=None`），协商值为 `max_read=262128`、
`max_pages=124`、`max_readahead=524288`、`ASYNC_READ=1`，有效 payload 上限为 262128 bytes。
探索性结果如下：

- 1 MiB/4 KiB off：191.881 ms，约 5.21 MiB/s；
- 1 MiB/4 KiB light：194.363 ms，121 个 FUSE READ，其中 110 个单页，最大请求 257942 bytes；
- 16 MiB/4 KiB off：3159.663 ms，约 5.06 MiB/s；
- requested/completed bytes 均为文件大小，说明主要问题是事务碎片化，而不是读放大；已经出现接近
  有效上限的大请求，排除了 max_read/max_pages/SG 把所有 READ 硬限制为 4 KiB 的解释。

上述每项只有一个样本，不能用于宣称统计收益；它们只证明问题能够在本地复现，并为正式实验冻结
数量级。用户观察到的 CubeSandbox 1--2 MiB/s 尚未在相同证据协议下复现，不能直接套用本地根因。

### 0.3 首选根因模型（历史设计，已据此形成 candidate）

Linux 6.6.139 的普通 cache hit 不重新计算预读窗口；demand miss 或命中 readahead marker/异步边界时
才进入预读规划。命中异步边界，或同步 miss 恰好位于 expected/旧窗口末端时，从旧窗口末端推进；
无法证明顺序性的独立随机 miss 只覆盖 demand，且不污染现有顺序预读状态。DragonOS FUSE 当前关闭通用 VFS readahead，
`read_cached_with_open()` 却在每次用户 read 上把 `state.start` 重置为当前页并调用 fill；`async_size`
被写入但未作为触发边界读取。窗口饱和后，相邻窗口只新增一个尾页，因此每次 4 KiB syscall 稳定产生
一个单页 FUSE READ。256 次用户 read 对应 256 次 readahead batch、121 个 daemon READ 中 110 个
单页，与该模型一致。whole-run reservation 单变量试验没有改变 READ 数，进一步排除了它是主因。

实施 P1 前仍要增加低扰动确认计数：`old_window_end`、`new_window_end`、饱和窗口新增一页次数、
reservation 冲突次数和 speculative suppression 次数。只有“饱和新增一页”接近单页 READ，且后两项
显著更少时，才把动态根因标为确认；否则停止并回到取证。

### 0.4 P0 实施前硬门禁（现作为验收清单）

测量复核发现以下缺口，均已纳入 P0，而不是留作结果解释：

1. off 模式当前关闭部分 queue/processing/reservation gauge，不能用恒零值证明 quiescence。应把最低限度
   owner current 变为真正 always-on，并用同 revision 的 off/light/off 重复实验验证成本；若成本不可接受，
   必须设计并证明覆盖 abort/disconnect 的 totals 守恒式。控制面文字必须与实际计数口径一致。
2. 正式 A/B/A 必须拒绝 dirty tree，并通过可信 build manifest 把 commit 与 kernel、disk image、helper、
   QEMU、virtiofsd SHA256 绑定。只记录 Git commit 不构成 artifact 身份证明。
3. READ 结构研究只允许 `guest-cold/host-warm|unknown`；完整 guest warmup 会让后续 light READ 计数为零。
   `guest-warm` 只用于 performance/off page-cache 热读，并且预热结果必须被 case marker 隔离。
4. stats 观测开销使用同 revision、同 artifact 的 mode A1/B/A2 专项，不得与 baseline/candidate 性能研究
   混在同一个 study。当前单次 off/light 的约 1.29% 差异只是假设相容，不是低扰动证明。
5. Linux guest 和 CubeSandbox 必须进入同一 workload/result schema。Linux reference 记录 tracefs/eBPF
   或等价 READ size/count；CubeSandbox 记录 sandbox ID、guest kernel/image SHA、三类服务日志并无条件
   destroy。无法隔离目标实例时样本无效。

runner/study 的 warm case marker、runner v4 case-result schema、case/cache/mode 预注册，以及 CPU delta 对
collector QEMU/virtiofsd/peer worker PID+starttime 的绑定已经加入 host 侧协议并通过正向测试；正式运行
前仍须增加上述 build attestation、dirty-tree、off quiescence 和跨环境集成负测。

### 0.5 总体实施顺序与历史授权边界

1. 只完成 0.2 的 P0 门禁与破坏性负测，不改变 I/O 决策。
2. 在 latest master 上封存本地 QEMU、Linux guest、CubeSandbox 的 fresh-guest baseline；先跑 pilot，
   再冻结 A1/B/A2 顺序、样本数、timeout 和 artifact。
3. 用 0.1 的确认计数完成动态证伪；不满足条件就停止，禁止实现首选根因模型。
4. 完成第二次独立终审并取得用户对 P1 的单独确认后，P1 只修改 FUSE 私有 readahead planner：
   demand miss 保证同步覆盖；普通 ready hit 不发 I/O；async trigger 或位于 expected/旧窗口末端的同步
   miss 才推进到旧窗口末端并批量填下一窗口；独立随机 miss 只覆盖 demand，不重建、扩大或污染顺序
   状态。保留现有 open pin、DMA reservation、completion、effective payload/SG 切分，不同时叠加 signal、
   reservation 或 pipeline 改动。预读规划与传输方式分开：协商 `FUSE_ASYNC_READ` 才能后台提交；未协商
   时同一批次必须同步完成。
5. 先通过 EOF/short read/random/concurrent/truncate/invalidate/disconnect/mmap correctness，并用拒绝
   `FUSE_ASYNC_READ` 的 daemon 验证无遗留 speculative background request、demand/signal/close/release/
   disconnect 生命周期正确，再执行
   master A1>=9、candidate B>=9、master A2>=9 的 cold-guest 验收。
6. 只有 P1 后仍证明多个独立大 batch 受 RTT 串行限制，才讨论 P2 pipeline；否则跳过。

该段是 2026-07-13 的历史门禁；2026-07-14 用户已批准 P0、P1。写回、IRQ/polling、多队列及 DAX 仍不
在本次授权范围内。

## 1. 范围与结论

本文只处理普通 virtiofs/FUSE request-response 路径。测试通过 QEMU 设备无 cache window（启动日志
`cache_region_len=None`）证明 non-DAX；当前 DragonOS mount 输出不打印 `dax=never`，不得伪造该
字符串。本文不研究共享内存窗口、`SETUPMAPPING`、`REMOVEMAPPING`、DAX fault 或 DAX mount mode。
Issue #2019 的原始主题是 DAX；本文是用户明确要求先行开展的独立 non-DAX 性能专项，不改变该 issue
的 DAX acceptance criteria，也不以 request/response 优化冒充 DAX 已完成。

截至 2026-07-13，fresh DragonOS VM 的未封印 exploratory 样本表明普通 virtiofs 很可能存在请求单页化：

- latest `origin/master` 基线的 1 MiB/4 KiB light 样本为 190.975 ms、131 个 READ，其中 123 个单页；
- 实验性 whole-run reservation 与 manifest 计数隔离后，fresh VM 为 191.877 ms、130 个 READ，其中
  122 个单页；direct requested/completed 均精确为 1,048,576 bytes；
- 组合样本中 direct bytes 恰好等于文件大小，符合 manifest 污染被移出窗口的预期；单次 sanity 样本没有观察到 READ 数、单页化或耗时的方向性改善，足以
  拒绝“根因已修复”，但不能用于估计统计意义上的真实性能效应；
- 这些轮次早于 runner v3 的原子封存协议，当前没有可由 finalize 重放的 case artifact；数字只能作为
  下一轮冻结实验的复现目标，不得称为正式 baseline 或可重复性能结论；
- latest-master 的另一轮样本曾出现 1 次 queue-full，候选轮为 0；单次事件不能证明 queue saturation。
  当前只确认提交给 daemon 的 READ payload 仍高度单页化，具体拆分层尚未确定；
- 本地 guest 聚焦测试中 short-read、stats control 与候选 signal 状态机的 interrupt 用例通过；候选
  interrupt 约 1.045 秒完成，而 master 约 1.065 秒返回 `EINTR`；后者符合 pre-fix 失败断言但不符合
  目标 Linux 语义，二者都没有 hang。
  mmap readaround 在 master 与候选上均超过 10 秒，属于既有 correctness blocker；暂未发现候选独有
  的失败，但右删失样本不能排除候选改变内部状态或加重等待，严重度差异尚未测定。

这些观测只适用于本地 QEMU 配置，尚未证明 CubeSandbox 具有相同根因。计划必须在本地 QEMU 与
CubeSandbox 分别取证，不能从一方直接外推另一方，也不能继续把本机 4 KiB 数字描述成 virtqueue
带宽上限。

## 2. 开发与评审原则

第一道授权门禁：本计划通过独立终审后立即停止；用户确认前不得开始或继续 P0 实施。P0 全部门禁完成、
根因唯一化且 P1 最小变更包通过第二次独立终审后，再次停止并取得用户对 hot-path/P1 的单独确认。
当前工作树中的 hot-path 改动只作为未批准实验候选保留，不代表 P0/P1 已获授权或已经完成。

先结合 Linux 6.6.139 代码、问题现象和 DragonOS 代码深入研究，再制定具体实现 plan；制定后先审查 plan 是否符合 Linux 语义、DragonOS 架构、并发/生命周期不变量、错误路径和边界条件，确认无 workaround、无测试特化、无隐藏坑点后才实施代码变更。

代码变更后，必须再次结合 Linux 6.6.139 审查 DragonOS 实现。如果发现语义不一致、架构不合理、边界条件遗漏、并发/生命周期风险或 workaround，必须回到 plan 阶段重新制定修复计划。

Linux 参考代码位于 `~/code/linux-6.6.139/`。普通读取重点对照 `fs/fuse/file.c` 的 generic page cache、async readahead、`max_read/max_pages`；普通写入重点对照 buffered/writeback、writepages、flush/fsync 和错误回报路径。DragonOS 可以采用适合自身 page cache、MM 和 virtqueue 的结构，但必须保持等价的用户可见语义。

禁止以下“优化”：

- 强制启用 DAX；
- 通过永久缓存、跳过 GETATTR/OPEN/FLUSH/FSYNC 破坏一致性；
- 只缩短 polling sleep 或改成 busy polling；
- 缩小 block size 绕过大请求 hang；
- 删除 fsync、数据校验或错误检查来提升数字；
- 为单一 benchmark 写测试特化分支。

## 3. 验证目标

### 3.1 主要目标

1. 验证 sequential read 已能把 readahead window 合并为受协商值和 SG 上限约束的大 READ，并量化小用户 read 留在 syscall/page-cache/VFS 层的成本。
2. 确定 sequential write 的 4096 个 WRITE 来自用户 syscall 粒度、FUSE max-write 协商、writethrough 策略还是内核拆分。
3. 确定 128 KiB benchmark block-size hang 的准确阻塞点和中断后连接状态。
4. 定位 CubeSandbox 命令交互延迟属于数据读取、metadata RTT、exec/page fault、IRQ/polling、bridge 调度还是 backend 限制。
5. 在保持 Linux coherency、错误语义和生命周期安全的前提下，使 DragonOS 接近同宿主、同 QEMU/virtiofsd 配置下的 Linux virtiofs reference。

### 3.2 最终成功标准

最终验收使用相对指标，避免硬件差异污染结论：

- 顺序读写吞吐达到同宿主 Linux virtiofs reference 的至少 70%，或对剩余差距给出经 trace/perf 证明的非 DragonOS 瓶颈；
- 冻结表中的 16 MiB/4 KiB sequential-read data-loop MiB/s 至少达到 master 的 4 倍，且不能以缓存
  命中或语义放宽冒充 transport 提升；
- sequential read 的平均 FUSE READ payload 接近协商限制：目标不少于 `min(max_read, max_pages * PAGE_SIZE, SG limit)` 的 50%，或证明文件尾/缺页分布导致无法合并；
- 对存在多个独立 read batch 的 workload，能够用 overlap 次数、并发 inflight 时间占比和 time-weighted inflight 证明有效 pipeline；单个大请求足以覆盖需求时不强制 inflight 大于 1；
- metadata、small-file 和 exec 的 p95 延迟不超过 Linux reference 的 2 倍，或每项都有明确、可复现的剩余内核缺口；
- QEMU/virtiofsd/guest CPU per MiB 不出现超过 20% 的无解释退化；
- 所有 correctness、故障注入和生命周期测试通过，无新增 hang、数据损坏、late completion 发布或资源泄漏。

70%、2 倍和 20% 是阶段性工程门槛，不是 Linux ABI 要求。若 Linux reference 本身受宿主噪声影响，必须先修复实验设计，不能调整门槛来掩盖结果。

## 4. 可重复测试基础设施

### 4.1 固定环境

每个结果必须记录：

- DragonOS/Linux guest commit 和 kernel build ID；
- QEMU、virtiofsd、benchmark 的路径、SHA256 和完整 argv；
- vCPU、RAM、CPU affinity、KVM 状态；
- queue size、request queue 数、virtiofsd cache mode；
- host share 所在 filesystem、mount options 和可用空间；
- dataset 大小、布局和 SHA256；
- guest mount 输出，以及 QEMU 设备参数/启动日志；non-DAX 必须由设备无 cache window 证明，不能依赖
  当前 DragonOS 未输出的 `dax=never` 文本；
- 测试顺序、cache protocol、预热次数、测量次数和异常退出状态。

primary 对照固定一个 backend cache mode；`auto`、`always`、`never` 作为独立实验维度，不允许在 DragonOS/Linux 对照中混用。

### 4.2 改造 benchmark

预计修改：

- `user/apps/virtiofs_bench/virtiofs_bench.cc`
- `tools/virtiofs/` 下的 host runner 与结果汇总工具
- `docs/kernel/filesystem/virtiofs_benchmark_runbook.md`

必须增加：

1. 使用 `CLOCK_MONOTONIC`。
2. 支持稳定外部 dataset、`--prepare-only`、`--reuse`，准备和清理不进入 timed region。
3. 拆开 seq-read/seq-write、create/stat/unlink、readdir、random-read/write 和 mmap fault scan。
4. 输出真实 syscall ops、bytes、ops/s、MiB/s、p50/p95/p99、checksum/hash 和 short-I/O 数量。
5. mmap 分别输出 pages faulted、bytes touched、logical span，禁止按逻辑跨度伪报复制带宽。
6. 支持 small-file、深目录、宽目录和 command/exec workload。
7. 支持 machine-readable JSON/CSV，host runner 自动生成配对报告。
8. 支持 watchdog；超时时保存 guest console、FUSE pending/inflight unique、实际 request size、descriptor count、queue token 和进程状态。

性能轮次不得读取 detailed stats。诊断轮次单独开启 stats/trace，并记录观测开销和 trace drop。

### 4.3 数据集与冷热态

- 吞吐数据集：256 MiB 和 1 GiB；主机预先生成并校验，测试过程不 truncate/recreate。
- small-file：4 KiB、16 KiB、64 KiB，各至少 10k 文件。
- metadata：1k/10k 空文件，深目录与宽目录分开。
- command：从 virtiofs 重复执行 BusyBox `true`、`ls`、`cat`，以及 `find`、`ls -lR`、批量 `stat`。
- cold：新 boot 或明确 guest/host cache drop 后重新 mount；记录 cache drop 是否真的可用。
- warm：同 mount、同 inode、同 dataset 预热后连续测量，不重建文件。

每组使用 1 次预热和 9 次 measurement。预注册平衡随机顺序，报告 median、p10/p90、IQR、min/max、配对差和首尾漂移。

## 5. 诊断指标与归因规则

### 5.1 FUSE/virtiofs 指标

- 每 opcode request count、request/response bytes 和 submit-to-complete RTT；
- READ/WRITE requested bytes、actual bytes、short-I/O；
- readahead window、连续 missing-page run、每批 page 数；
- pump/complete batch 分布；
- request inflight 的 time-weighted average、overlap 时间和峰值；
- queue full、blocked/retry、queue depth；
- bridge wait/wake reason、poll sleep time；
- request clone/copy bytes、response alloc/reuse/zero/waste；
- page cache 的 LOADING/UPTODATE/ERROR、waiter 和 late completion 丢弃计数。

### 5.2 Host/guest 指标

- QEMU、virtiofsd 和 guest 的 CPU time、cycles、instructions、context switches、page faults；
- virtiofsd `/proc/<pid>/io` 和线程池利用率；
- guest scheduler wait、软中断/硬中断和 bridge kthread 运行时间；
- end-to-end throughput、IOPS、ops/s 和 latency percentiles。

### 5.3 归因规则

- READ 数量接近页数、平均 payload 约 4 KiB：优先检查 page-cache run 构造、max limits 和同步 wait。
- READ 已合并但仍受 RTT 限制：才考虑多个独立 batch pipeline。
- WRITE 数量等于用户 4 KiB write 次数：先区分预期 writethrough 与缺失 writeback，不直接归咎 bridge。
- queue full 且 inflight 接近 capacity：检查 backend 或 queue depth；queue 从未满且 inflight 为 1：检查上层串行化。
- 本地 IRQ 正常、CubeSandbox polling 很高：先修复 CubeSandbox MSI/MSI-X/IRQ wake 根因，不能靠调小 sleep。
- command 慢但大文件吞吐正常：优先 metadata/exec/page-fault RTT。
- DragonOS 和 Linux 都慢：检查 host filesystem、virtiofsd/cache mode、CPU pinning，不修改 DragonOS 热路径掩盖宿主瓶颈。

## 6. 分阶段实施与验证门禁

### P0：测量工具与稳定基线

只改 benchmark、runner、文档和必要的低开销计数器，不改 I/O 决策。

门禁：

- DragonOS QEMU、CubeSandbox、Linux reference 能执行相同 dataset 和参数；
- 性能轮次无 detailed-stats 开销；
- 结果包含 hash、环境指纹、重复统计和 timeout artifacts；
- 4 KiB 低吞吐与 128 KiB block-size hang 都可稳定复现或被可信证伪。
- runner 具备 revision/artifact 切换、预注册随机区组、`A1/B/A2` 汇总和 bootstrap CI；Linux reference
  与 CubeSandbox 都有与 DragonOS 相同 workload/schema 的可执行 runbook；
- 能采集 QEMU、virtiofsd（含 worker）和 guest CPU time，并按已完成字节计算 CPU/MiB；能封存并验证
  `max_read/max_pages/max_readahead/FUSE_ASYNC_READ` 与实际 SG payload limit。

上述条目任一未满足，P0 都不得标记完成，也不得启动正式性能 A/B/A。

### P1：普通读取请求合并

重点代码：

- `kernel/src/mm/readahead.rs`
- `kernel/src/filesystem/page_cache.rs`
- `kernel/src/filesystem/fuse/inode/file.rs`
- `kernel/src/filesystem/fuse/conn/request.rs`
- `kernel/src/filesystem/fuse/virtiofs/bridge.rs`

先证明连续页在哪一层被拆分。根因确认后，把连续 missing pages 合并到不超过 `min(max_read, max_pages * PAGE_SIZE, SG limit)` 的 READ，并保持 direct page-cache DMA ownership。

必须覆盖：

- EOF、非页对齐 EOF、hole 和 short read；
- LOADING/UPTODATE/ERROR 发布顺序及尾页清零；
- signal interruption 和所有 waiter 唤醒；
- truncate/invalidation 与晚到 completion；
- unmount、disconnect、reset 时 reservation/descriptor 生命周期；
- 4 KiB、64 KiB、1 MiB 用户 read 以及 mmap fault/readaround。

门禁：READ 数和平均 payload 达到第 3 节目标；内容校验和上述 race 测试全部通过；A/B/A 显示吞吐提升且 CPU per MiB 无异常退化。

### P2：读取 pipeline（条件阶段）

只有 P1 后仍存在多个独立 batch 且 RTT 明确成为瓶颈时才实施。允许多个 background READ inflight 前，必须证明 page reservation、unique、completion、short read、truncate generation 和 teardown 可以独立回滚。

门禁：不是简单让峰值大于 1，而是 overlap 时间、time-weighted inflight、端到端吞吐与 CPU 指标共同改善；若单个大 READ 已满足性能目标，本阶段跳过。

### P3：普通写入路径

先取证：

- `max_write/max_pages` 与 `FUSE_WRITEBACK_CACHE` 协商；
- 用户 write 粒度与实际 FUSE WRITE 粒度；
- writethrough、buffered/writeback、writepages 路径；
- fsync/fdatasync/flush/release RTT；
- dirty page、writeback error 和 truncate/invalidation 交互。

候选优化只能由证据选择：write 聚合、writepages batching、direct SG pages 或安全并发 writeback。不得删除同步操作或放宽 error reporting。

门禁：

- sequential/random/concurrent write 吞吐接近 Linux reference；
- fsync/fdatasync/close 后 host 内容和 hash 正确；
- ENOSPC/EIO/short write/writeback error 按 Linux 语义报告；
- truncate、append、并发写、unmount/disconnect 无数据丢失或死锁。

### P4：metadata 与命令交互

分别测量 LOOKUP、GETATTR、OPEN、RELEASE、READDIR/READDIRPLUS、权限检查和 exec 文件读取。先验证 entry/attr timeout、negative dentry、readdirplus 和 notify invalidation 是否按 Linux 语义生效，再决定缓存或请求合并方案。

禁止永久缓存、跳过 daemon 请求或忽略 invalidation。优化必须保持 rename/unlink、host-side change、权限/所有者变更和负 dentry 一致性。

门禁：small-file、exec、`find`、`ls -lR`、批量 stat 的 p50/p95/p99 达到第 3 节目标，并通过对应 coherency dunitest。

### P5：bridge/IRQ/queue 调度

在本地 QEMU 和 CubeSandbox 分别确认 IRQ wake。如果 CubeSandbox 使用 polling fallback，先修复 IRQ 注册/回调根因。只有 queue/inflight 数据证明单队列成为瓶颈时才研究多 request queue。

多队列设计必须覆盖：

- 请求分流与同 inode 顺序性；
- FORGET/INTERRUPT hiprio；
- unique completion；
- DESTROY/barrier；
- queue full retry；
- reset/disconnect 时跨队列 drain。

门禁：调度优化与 bridge wait/RTT/CPU 指标变化相符；无 busy polling、无空转 CPU 回归、无 teardown 泄漏。

## 7. Correctness 与故障注入矩阵

性能结果只有在以下测试全部通过时有效：

- 内容：全文件 SHA256、随机块 pattern、mmap/pread 一致；
- 边界：空文件、短文件、非页对齐 EOF、跨 request limit、稀疏文件；
- 并发：多个 reader/writer、read-truncate、write-truncate、rename/unlink、append；
- 错误：EINTR、EIO、ENOSPC、short read/write、daemon 明确错误；
- 生命周期：请求未提交、已提交未完成、completion 与 signal/truncate/unmount 竞态；
- transport：queue full、virtiofsd 退出、socket 断开、device reset；
- page cache：loading page 被 invalidate、late completion、多个 waiter、writeback error；
- metadata：entry/attr timeout、negative lookup、host notify、rename/unlink coherency。

新回归测试优先加入 dunitest。影响用户可见行为的变更必须 `make kernel`，启动 DragonOS，并在 guest 内执行 focused tests 和同一 pre-fix workload。

## 8. 对照矩阵

| 目标 | 用途 | 约束 |
| --- | --- | --- |
| DragonOS QEMU non-DAX | 可控开发基线 | 与 Linux reference 同 QEMU/virtiofsd/host share |
| DragonOS CubeSandbox non-DAX | 真实交互问题 | 单独记录 IRQ、queue、cache、CPU 和容器层差异 |
| Linux guest virtiofs non-DAX | 参考实现 | 相同 host、backend、dataset 和 workload |
| DragonOS virtio-blk/pmem | 存储路径上界 | 仅作参考，不据此推导 virtiofs 语义或直接等价性能 |

不把 host page-cache 热读、rootfs、virtio-blk raw device 或 pmem raw bandwidth 当作 virtiofs transport 成绩。

## 9. 建议 PR 顺序

1. `test(virtiofs): make non-dax benchmarks reproducible`
2. `test(virtiofs): capture request size and inflight timelines`
3. `perf(fuse): coalesce page-cache readahead requests`
4. `perf(fuse): pipeline safe background reads`（仅 P1 后证据需要）
5. `fix(virtiofs): preserve interruptible large-request lifecycle`（若 signal 生命周期被证实为独立根因）
6. `perf(fuse): batch ordinary writeback requests`（仅 P3 证据需要）
7. `perf(fuse): reduce metadata round trips`（按 P4 根因拆分）
8. `fix(virtiofs): restore interrupt-driven bridge wakeups`（仅 CubeSandbox 证实 polling 时）
9. `perf(virtiofs): distribute ordinary requests across queues`（仅单队列被证实为瓶颈时）

所有 PR 都需要 pre-fix 证据、Linux 6.6.139 对照、并发/生命周期审查、focused dunitest、`make kernel`、DragonOS guest 验证和残余风险。改变热路径或声称性能收益的 PR 额外强制 A/B/A 报告。

## 10. 当前实施起点与下一轮冻结实验

候选实现已在实验工作树中，但未获保留或合并批准。下一轮只做归因取证，按以下硬门禁顺序执行，
不并行叠加优化：

1. runner v3 已完成 copy-to-temp、源稳定性复核、副本验证、原子发布与 finalize 语义重放；已验证
   collect 期间源变化会被拒绝，且重算 hash 也不能让错误 QEMU/non-DAX argv 通过。后续新增 transcript/
   stats 字段时必须同步增加同类破坏性负测。
2. 对 commit `14606afc429df342bc04f28d7b428fc42931199d`、仅 manifest 隔离、仅 reservation、仅 signal
   状态机和组合版本做单变量 fresh-VM 二分。每个版本执行相同 guest focused tests 与 1 MiB case；若
   hang 只在候选出现，先修复或回退该候选。
3. 将 guest interrupt 的 master/candidate 语义差异封存为回归证据，并继续定位既有 mmap hang。watchdog
   固定 10 秒；超时必须保存最后 syscall、unique、pending/processing/transport owner、stats、串口和
   GDB 全 CPU 快照。
4. 仅在 detailed 诊断模式为每次 fill 分配稳定 `fill_id`，以 `(connection_epoch, cache_id, inode, generation, fill_id)` 关联
   demand/planned window、逐页初态、连续 Missing run、reservation 结果、FUSE unique、submitted
   payload/SG 和 completion；诊断轮使用有界事件槽，light 性能轮只保留桶。同步记录协商的
   `max_read/max_pages/max_readahead/FUSE_ASYNC_READ` 和实际 SG payload limit。
5. 只对同一 `fill_id` 作归因：`planned > missing` 才归因 window/cache state，`missing > reserved` 才
   归因 reservation，`reserved > submitted` 才归因 FUSE/SG/transport；各层相等但仍单页则检查 fill
   调用频率和 per-open readaround 推进。
6. 用相同 QEMU、virtiofsd、dataset、vCPU/RAM 和 non-DAX 配置运行 Linux 6.6.139 guest reference，采集
   READ size/数量与吞吐。Linux 只作性能参照；DragonOS 的 EOF/signal/teardown 正确性仍按 Linux 语义
   单独验证。
7. 根因唯一化后只修改命中的一层，并执行独立 cold-guest A/B/A：`A1>=9、B>=9、A2>=9`，报告
   median/IQR、READ 页数桶、direct bytes 守恒、owner 归零、QEMU/virtiofsd/guest CPU per MiB。
8. P1 前先在 CubeSandbox 做最小同 workload baseline，以判断其 1--2 MiB/s 是否具有相同 READ 单页化；
   P1 达标后再跑完整 256 MiB throughput、small-file/metadata/exec 与 CubeSandbox 矩阵。不得从本地
   1 MiB 样本直接外推容器交互卡顿的根因。
9. 在任何 hot-path 修改前，把唯一化根因记录、Linux 6.6.139 对照、owner/终态状态表、最小变更文件与
   测试矩阵交给独立终审；终审无 blocker/major 后立即停止并等待用户确认。确认前不得继续叠加或批准
   page-cache、FUSE request lifecycle、MM fault、signal lifecycle 或 bridge 热路径候选。

冻结门槛：正常 correctness case 每版本 20 次、每次 10 秒；任一 hang/hash 错/非法 owner 非零零容忍；
reset-failure 故障注入允许状态表定义的安全 quarantine，但必须证明 owner 仍被隔离且不会发布旧数据；startup/
EOF 裕量最多 2 个 READ；稳态 payload p10 不低于有效上限 25%、median 不低于 50%；读放大量不超过
文件大小 1.25 倍；A/B/A 固定 `A1>=9、B>=9、A2>=9` 个独立 cold-guest 样本，bootstrap 95% CI，
目标为 READ
数和 median latency 均至少改善 25%；p90 不得退化超过 10%，CPU/MiB 不得无解释退化超过 20%。pilot
若证明门槛不适用，只能在正式采样前记录理由并重新冻结，不能看结果后调整。

停线条件：READ 数或 median latency 未达到上述 25% 门槛、吞吐 CI 无收益、CPU per MiB 无解释退化、内容/EOF/signal/mmap/
teardown 任一回归，或只能依赖 busy polling、重试、扩大缓存/窗口掩盖问题。满足停线条件即回到取证，
不启动 P2 pipeline、多队列或 metadata 缓存实现。

### 10.1 正式冻结的 workload 与层级门槛

| 层级 | workload | dataset/cache state | primary metric | 准入或成功值 |
| --- | --- | --- | --- | --- |
| P1 correctness | sequential read，4 KiB syscall | 1 MiB；guest-cold；host-warm/unknown 原样记录 | hash、hang、owner | 20 次、每次 10 秒；零错误、零 hang、owner 合法退休 |
| P1 合并准入 | 同上 | 同上 | READ count、稳态 payload、读放大量 | 相对 master READ count 至少改善 25%；payload p10/median 分别达到 effective limit 的 25%/50%；放大量不超过 1.25 倍 |
| P1 性能准入 | 同上 | 同一 host-cache 分类内比较 | data-loop latency median、p90、CPU/MiB | median latency 至少改善 25%；p90 退化不超过 10%；CPU/MiB 无解释退化不超过 20% |
| 本地小块最终目标 | sequential read，4 KiB syscall | 16 MiB；guest-cold；固定并记录 host-cache 分类 | data-loop MiB/s | 相对同配置 master 至少 4 倍 |
| 大文件最终目标 | sequential read/write | 256 MiB 和 1 GiB；冷热态分别报告 | data-loop MiB/s | 达同配置 Linux virtiofs reference 至少 70%，或 trace/perf 证明剩余差距不在 DragonOS |
| 交互最终目标 | small-file/metadata/exec | 第 4.3 节冻结数据集；冷热态分别报告 | 端到端 p95 | 不超过同配置 Linux reference 的 2 倍 |

P1 准入只允许保留读取候选并进入后续 non-DAX 阶段，不代表总体性能目标完成；最终完成还必须满足
本地小块、大文件、交互目标和全部 correctness 门禁。“单页桶达标”不再作为独立模糊条件，统一由
READ count、payload p10/median 和读放大量三项裁决。

A/B/A 的两个 A 时段各自至少 9 个样本，不能合计。先分别估计 A1、A2 median；相对差超过 10% 或
bootstrap 95% CI 显示显著首尾漂移时整轮作废。无漂移后以时段为 strata 合并 A，并对 B 相对分层 A
做 bootstrap 95% CI；规则在看正式结果前冻结。

### 10.2 mmap blocker 的冻结诊断顺序

未封印 master 与候选运行中均出现 mmap 首次访问超时，exploratory GDB 快照的两个 vCPU 均停在 idle；
该现象仍须由 runner v3 artifact 正式复现。测试二进制必须先从
virtiofs 顺序复制到 guest 本地块盘后执行，避免 ELF 自身按需分页污染 FUSE fault 观测；BusyBox `cp`
会因当前 virtiofs `lseek` 语义失败，诊断流程使用 `dd` 顺序复制，且复制阶段不进入测量窗口。
必须封存 `dd` 实际 byte count、源/目标 SHA256 和最终本地执行路径；复制完成后等待 quiescent，并 reset
stats/run epoch，避免复制测试 ELF 的 READ 污染 mmap fault 现场。

exploratory 低扰动原子状态点提示超时发生在 generic read fault 已进入后、FUSE `fault()`/目标 READ 到达 daemon
之前。一次 VMA owner 探针把最后一次成功加锁定位到 `LockedVMA::is_anonymous()`；fault PID 与记录的
owner PID 相同，是 VMA 自锁的重要候选。但是显式收窄并 `drop` 该 guard 的单变量试验仍稳定挂起，且该测例同时存在
同地址空间的 FUSE daemon 线程；仅凭“最后 owner/最后调用点”无法区分当前 fault、daemon 的并发 fault
以及 unlock/wakeup 交错，不能据此认定 VMA 自锁。探针已全部撤回，不进入候选实现。

代码与 Linux 6.6.139 对照已经确认一个更高优先级的架构差异：x86 fault 入口持有
`AddressSpace.write()` 覆盖整个 FUSE enqueue 和 pending wait；Linux 在可能 readahead、等待 folio 或同步
I/O 前通过 `maybe_unlock_mmap_for_io()` 释放 mmap 锁，并以 `VM_FAULT_RETRY` 重新查 VMA。DragonOS 的
`VM_FAULT_RETRY/FaultRetryWait` 外层协议已经存在，但普通 non-DAX FUSE cold fault 尚未使用。对于线程型
FUSE daemon，当前路径存在可证伪的闭环：fault 线程持 mm write lock 等 daemon，daemon 在 `/dev/fuse`
read 的 `copy_to_user` 发生缺页时又等待同一 mm lock。它是已确认的结构性缺陷候选，尚未被证明为本次
稳定 hang 的唯一动态触发点。

下一轮严格按以下顺序执行，完成前不启动正式性能 A/B/A：

1. 为单次 fault 分配 `fault_id`，记录 VMA id、mm id、backing pgoff，并分别标记 VMA 锁、page-table-edit、
   `map_pages` 入口/出口；全局“最后阶段”会被其他文件 fault 覆盖，不作为正式证据。
2. 记录每个休眠锁的 owner task、waiter 和 acquisition/release generation；PID 只作辅助。建立同一
   `fault_id` 的完整 wait-for 链：fault 当前持有什么、等待哪个 pending/entry/lock，daemon 是否正等待
   同一 mm/VMA owner。必须以最小单变量解除该边后稳定通过、恢复后稳定复现，才允许宣布动态根因唯一化。
3. 对同一 cold mmap workload 做三个单变量证伪实验：daemon 请求 buffer 全页 prefault、daemon 改为
   独立进程/独立 AddressSpace、目标文件先 `pread` 预热。只有线程 daemon 失败而独立 mm 通过，或
   prefault 明确改变状态链，才把 mm-lock/FUSE-daemon 闭环认定为当前动态根因；这些实验只用于归因，
   prefault 和改测试进程模型都不得作为修复。三组实验只是 supporting discriminator，必须同时满足
   第 2 步的完整 wait-for 链和解除/恢复最小边，才允许唯一化。
4. 若闭环命中，设计四段式 `FaultRetryWait` 协议：先创建精确 reservation/pending identity；在不得等待
   daemon、不得在 queue-full 路径睡眠的前提下有界发布；返回 `VM_FAULT_RETRY` 让 x86 外层释放
   `AddressSpace` guard 后等待；retry 重新查 VMA，并复核 mm/VMA/pgoff/cache entry/reservation
   generation 后才安装 PTE。发布失败必须按 owner 状态精确回滚，不得保存裸 VMA guard 或跨 retry 信任
   旧 mapper/page identity。
5. 单变量验证 Linux 6.6.139 invalidate 锁边界：generic `filemap_fault()` 先触发 mmap readahead；readahead
   自己在预分配并提交 `read_pages()` 的窗口持一段 shared invalidate lock，提交完成后释放；随后 demand
   folio lookup/create、folio lock/identity/uptodate 与 truncate 复核，以及必要的同步 `read_folio` 和错误
   处理再持另一段 shared invalidate lock，完成、错误或 retry 出口才成对释放。两段互不嵌套。DragonOS 当前把 shared
   invalidate 覆盖整个 `do_read_fault`。验证版本
   必须同时给 generic `filemap_fault/pagecache_fault_zero` 和 FUSE demand-page 路径定义明确的锁 owner，
   不得只删除 `reserve_read_dma` 内锁。
6. 专门覆盖 writer-preference 下递归 reader，以及 pending short-read truncate 的 read-to-write upgrade；
   任一验证改动只有在 mmap 测例、truncate/invalidate race 和 ext4/FAT/tmpfs 文件 fault 回归同时通过后
   才能保留。

一次“移除 read fault 宽锁、在 FUSE demand page 前局部加锁”的探索性试验没有解除该 hang，已撤回；
因此 invalidate 锁边界是已确认的结构性风险，但当前不得宣称它就是唯一 hang 根因或性能根因。
