# Virtiofs 基准测试运行手册

`virtiofs_bench` 是用于 DragonOS virtiofs 性能分析的客户机侧基准测试工具。它由 `user/apps/virtiofs_bench` 下的 DADK app 安装到 `/bin/virtiofs_bench`。

默认情况下，基准测试会把 virtiofs tag `hostshare` 挂载到 `/tmp/virtiofs_bench_mount_<pid>`，运行指定 workload，然后自动卸载并删除临时目录。只有在需要测试一个已经挂载好的 virtiofs 目录时，才使用 `--mount PATH`。

## 非 DAX P0 证据 runner

`tools/virtiofs/non_dax_bench_runner.sh` 是宿主机侧的证据编排器。它不会假装能够可靠控制当前的
QEMU 串口，而是生成只读并带哈希封印的 case matrix、manifest 和客户机命令，再显式收集每个 case 的串口、
QEMU/virtiofsd argv、stats 和 GDB 现场。runner 不启动 DAX，也不把 block size 当作实际 FUSE READ
大小；实际请求大小必须由诊断统计证明。

宿主机必须提供 `jq`、`sha256sum`、`realpath`、`od`、`find` 和 iproute2 的 `ss`。其中 `ss` 用来
把 QEMU 与所选 virtiofsd 的 Unix `ESTAB` peer inode 直接绑定；为保证输出可无歧义解析，
`SOCKET_PATH` 必须是无空白字符的绝对路径。

runner 要求新版 helper 提供以下稳定接口：

```text
--workload prepare|sequential_read|sequential_write|cleanup
--mount MOUNT
--path RELATIVE_DATASET
```

生成的 guest preflight 会检查 `sequential_read` 和 `--path`。旧 helper 不支持时直接失败，禁止降级到
把 write/fsync/close/read 混在一起的 `sequential` workload。

### 两阶段准备数据

先创建共享目录，并生成 prepare 计划：

```sh
mkdir -p bin/virtiofs-share
tools/virtiofs/non_dax_bench_runner.sh plan \
  --phase prepare --mode light --profile quick \
  --share-dir "$PWD/bin/virtiofs-share" \
  --dataset non-dax-p0
```

按 runner 输出的 `MANUAL-STAGE.txt` 启动 virtiofsd 和 DragonOS，再逐行执行 `guest-commands.sh`。
prepare 成功后，共享目录中必须同时存在
`.virtiofs_bench_<dataset>/seq.dat` 和 `.virtiofs_bench_<dataset>/manifest.v1`。`dataset` 必须是单个
安全路径分量，与 helper 的 `--path` 协议一致。正式 read 计划在创建前会检查二者为普通文件且不是
符号链接；不存在时立即失败，不会在测量 VM 中临时生成数据：

```sh
tools/virtiofs/non_dax_bench_runner.sh plan \
  --phase read --mode performance --profile quick \
  --share-dir "$PWD/bin/virtiofs-share" \
  --dataset non-dax-p0 \
  --guest-cache cold --host-cache unknown
```

`quick` 默认是 1 MiB/4 KiB 单 case，`full` 默认是 16 MiB/128 KiB 单 case。默认单 case 是刻意的：
`guest-cache=cold` 要求每个 case 使用一台 fresh VM，同一 VM 顺序测多个 block size 时只有第一个可能是
cold。比较 4 KiB 和 128 KiB cold-read 时必须创建两个 run 并分别启动 fresh VM；只有明确标成
`--guest-cache warm` 时才允许 `--block-sizes` 传入多值。一个 evidence run 也只接受一个文件尺寸，因为
manifest 与 dataset 尺寸必须一一对应；测试 1/16/256 MiB 时应分别 prepare 并使用不同的 `--dataset`
名称，禁止让同一文件冒充多个尺寸。256 MiB warm 稳态确认示例：

```sh
tools/virtiofs/non_dax_bench_runner.sh plan \
  --phase read --mode performance --profile quick \
  --share-dir "$PWD/bin/virtiofs-share" \
  --dataset non-dax-p0-256m \
  --file-sizes 268435456 --block-sizes 4096,131072 \
  --guest-cache warm \
  --host-cache warm
```

### 缓存标签和观测模式

runner 把缓存态拆成 `guest-cache` 和 `host-cache`。`guest-cache=cold` 表示该 case 必须使用 fresh VM；
它不是由 runner 自动实现的 cache drop。当前流程不能可靠证明宿主页缓存为 cold，因此 host 只接受
`warm` 或 `unknown`，传入 `cold` 会失败。报告中必须使用类似 `guest-cold/host-unknown` 的完整标签。

`performance` 显式使用 `stats_mode=off`；默认的 `light` 只增加 READ 请求数、字节和大小桶，并保存
stats；`diagnostic` 才启用逐 opcode、copy 和 allocation 分项。off/light/detailed 必须在独立 fresh VM
中采样。light 只有通过同 artifact 的 off/light A/B/A 开销门禁后才能作为吞吐结论，否则 light 只用于
请求结构归因、吞吐采用 off。
runner manifest 会记录自身版本和完整 argv、git commit、kernel/disk/QEMU/virtiofsd 哈希及版本、dataset
哈希、缓存标签、宿主 boot ID 和环境。因为 QEMU 由人工启动，manifest 将其 argv 状态标成
`manual-capture-required`，每个非 skipped case 都必须收集实际 `/proc/PID/cmdline`，不能拿计划命令冒充
真实命令。runner v4 的 collect 只接受仍存活的 `/proc/<数字PID>/cmdline`，把所有 artifact 复制到同目录
临时 case 后复核源 fingerprint/hash 未变化，再从 live `/proc` 生成 collector process context。副本通过
transcript/stats/argv 语义验证后才原子 rename 发布；因此不会出现“验证的是源、封存的是后来变化内容”。
process context 记录 exe、cwd、start ticks 和 host boot ID，使 finalize 在进程退出后仍能重放静态语义。
进程启动时间必须不早于 run 创建时间；cold run 必须在 plan 之后新启 virtiofsd 与 QEMU，不能复用旧 VM。
每个 manifest 还生成唯一 `run_id`；guest 命令把它传给 helper，并写入 stats-mode、case begin/end、
quiescence、全部 phase、result 和 io_summary。collect 对每条记录强制匹配该值，因此相同矩阵的旧串口日志
不能冒充本轮结果。

### 人工 watchdog 与证据收集

当前串口输入在长命令和自动 timeout 下会丢字符，所以 runner 只记录 watchdog 阈值，不自动发送命令或
signal。超时后必须按以下顺序操作：

1. 不改动现场，先保存 GDB 全 CPU backtrace、FUSE/page-cache 原子快照和 stats；
2. 保存当前 `serial_opt.txt`、virtiofsd log 和两个进程的 `/proc/PID/cmdline`；
3. 然后才向 guest 发送 signal；
4. 验证同一 mount 的最小 metadata/read，并另存 signal 后证据。

完成一个 case 后从宿主机登记证据。目标 case 必须来自该 run 的 `case-matrix.tsv`，artifact 名不可重复，
已有 status 不可覆盖：

```sh
tools/virtiofs/non_dax_bench_runner.sh collect \
  --run-dir /absolute/path/to/run \
  --case read-f1048576-b4096 --status completed \
  --artifact serial=/absolute/path/to/serial_opt.txt \
  --artifact stats=/absolute/path/to/stats.txt \
  --artifact qemu_cmdline=/proc/QEMU_PID/cmdline \
  --artifact virtiofsd_cmdline=/proc/VIRTIOFSD_PID/cmdline
```

`completed` 不是人工信任标签：runner 会在 serial artifact 中按顺序强制验证唯一的 case begin、
before-quiescence、read 的 open/data-loop/close/verify（prepare 则为 open/data-loop/fsync/close/manifest）
完整 phase 序列、与 matrix 完全匹配且唯一的 `result status=ok`、`io_summary`、
after-quiescence 和 `rc=0` end，以及 `P0_STATS_MODE:off|light|detailed`。helper 的 quiescence 会覆盖 FUSE
queued、dequeue-to-submit dispatch、processing、background、page-cache READ reservation，以及 virtiofs
transport/reply owner；所有 current 为零且生命周期 totals 连续稳定才通过，超时即令 case 失败。runner
还会把两个 NUL 分隔 cmdline 解析成真实 argv，校验
可执行文件 realpath/哈希、QEMU 的 kernel/drive/SMP/non-DAX vhost-user-fs 参数、chardev 与 daemon socket
绑定，以及 virtiofsd argv 与
manifest 计划完全一致；普通文本不能冒充 `/proc/PID/cmdline`。light/diagnostic case 还必须传入 workload 后
由 guest 命令现场生成的
`.virtiofs_bench_stats_<run_id>_<case_id>.txt` 作为 `--artifact stats=...`；其首行必须精确为
`P0_STATS_RUN:<run_id>:<case_id>`，后续 mode 必须与 case 一致。read case 还会校验 direct-DMA、READ
大小桶和 bridge 守恒，以及完成后的 owner gauge 全部归零。
prepare case 会记录新生成的宿主 dataset/manifest 哈希；read case 会重新计算并绑定 plan 时的哈希，防止
测量前后数据被替换。当前没有 host 侧运行期 watcher，因此这属于端点完整性校验，不应宣称能抵御同权限
进程在测量中替换后再恢复文件。缺少任一证据时 collect 直接失败。

`timeout` 强制要求非空的 serial、GDB 和 stats，并仍要求进程 argv。源文件在复制期间或封存前变化会让
collect 失败且不会发布半成品 case；临时目录由退出清理。runner 不清理已发布 run，失败或未完成的现场
也会保留。所有 case 登记后才能 finalize：

```sh
tools/virtiofs/non_dax_bench_runner.sh finalize --run-dir /absolute/path/to/run
```

缺 case 时 finalize 失败但不删除证据；finalize 先验证 artifact/index hash，再对封存副本重放
transcript、stats、argv、二进制、cwd、进程启动时刻和 non-DAX 绑定语义。即使有人同时重算 artifact
与 index hash，语义错误的证据仍会被拒绝。存在 timeout/failed/interrupted/skipped 时会生成
`final.json` 并返回非零。这个返回值表示样本集未全部完成，不等于内核性能结论。

封印、只读权限和自洽哈希用于发现误改和拒绝直接复用旧证据，不是对同权限/root 主动攻击者的密码学
签名。正式性能结论还必须由独立汇总报告验证重复次数、A/B/A、median/IQR/CI 与 Linux reference；单个
`finalize` 只证明该 run 满足采集协议。

finalize 后必须再次独立重放，不能只信任已经存在的 `final.json`：

```sh
tools/virtiofs/non_dax_bench_runner.sh verify --run-dir /absolute/path/to/run
```

### CPU/MiB 与 A1/B/A2 汇总

宿主 CPU 窗口使用进程累计 ticks；thread 明细仅用于交叉校验。`before` 必须在 guest workload 启动前，
`after` 必须在 workload 返回后立即采集，两个快照的 PID/starttime 必须一致。virtiofsd 若另有独立 worker
进程，用重复的 `--virtiofsd-worker-pid` 全部登记：

```sh
python3 tools/virtiofs/non_dax_study.py cpu-snapshot \
  --phase before --run-id RUN_ID --case-id CASE_ID \
  --qemu-pid QEMU_PID --virtiofsd-pid VIRTIOFSD_PID \
  --output cpu-before.json

# 在 guest 中执行且只执行该 case

python3 tools/virtiofs/non_dax_study.py cpu-snapshot \
  --phase after --run-id RUN_ID --case-id CASE_ID \
  --qemu-pid QEMU_PID --virtiofsd-pid VIRTIOFSD_PID \
  --output cpu-after.json
python3 tools/virtiofs/non_dax_study.py cpu-delta \
  --before cpu-before.json --after cpu-after.json --bytes COMPLETED_BYTES \
  --output cpu-delta.json
```

`cpu-delta.json` 必须随同 case 一次性封存：

```sh
tools/virtiofs/non_dax_bench_runner.sh collect ... \
  --artifact cpu-delta.json=/absolute/path/to/cpu-delta.json
```

研究计划在采样前生成。每层默认至少 9 个样本；A1/A2 都是 baseline，B 是 candidate：

```sh
python3 tools/virtiofs/non_dax_study.py study-plan \
  --baseline-revision BASE_SHA --candidate-revision CANDIDATE_SHA \
  --samples-per-stratum 9 --seed 2019 \
  --workload read-f16777216-b4096 --cache guest-cold-host-unknown \
  --output study-plan.json
```

每个 light/diagnostic case 先完成 runner `finalize` 和 `verify`，再从封存证据派生 study case；工具拒绝
手填 elapsed、READ count 或 CPU/MiB。performance/off 没有 READ 大小证据，不能与另一个 light 窗口拼接，
因此吞吐开销研究和 READ 合并研究必须分别报告：

```sh
python3 tools/virtiofs/non_dax_study.py pack-case \
  --plan study-plan.json --sample-id A1-001 \
  --verified-run-dir /absolute/path/to/run --runner-case-id CASE_ID \
  --output packed/A1-001.json --index-entry-output packed/A1-001.index.json

python3 tools/virtiofs/non_dax_study.py aggregate \
  --plan study-plan.json --results-index results-index.json \
  --acceptance acceptance.json --report acceptance.md
```

warm case 由 runner 在每个测量 case 前执行一次同 dataset 的完整校验读；该预热也会温热 backend 路径，
所以 `guest-cache=warm` 必须同时声明 `host-cache=warm`。没有预热成功 marker 的 warm 样本无效。

## 构建

在 DragonOS 仓库根目录执行：

```sh
make user
SKIP_GRUB=1 make write_diskimage
```

快速做一次宿主机编译检查：

```sh
make -C user/apps/virtiofs_bench clean all
make -C user/apps/virtiofs_bench clean
```

## 启动 Virtiofs

创建本地环境配置文件：

```sh
cp tools/virtiofs/env.sh.example tools/virtiofs/env.sh
```

默认共享目录是：

```text
bin/virtiofs-share
```

准备 virtiofs smoke test 需要的文件：

```sh
mkdir -p bin/virtiofs-share
printf 'virtiofs-host-file\n' > bin/virtiofs-share/hello.txt
cp /bin/busybox bin/virtiofs-share/busybox
chmod 755 bin/virtiofs-share/busybox
```

启动后端和客户机：

```sh
make virtiofsd
make qemu-virtiofs-nographic AUTO_TEST=none
```

这两个命令需要在两个终端分别运行。QEMU 命令会暴露 tag `hostshare`。

验证不同 virtqueue 深度时，可以给 QEMU 设备传入显式 queue size：

```sh
DRAGONOS_VIRTIOFS_QUEUE_SIZE=8 make qemu-virtiofs-nographic AUTO_TEST=none
DRAGONOS_VIRTIOFS_QUEUE_SIZE=128 make qemu-virtiofs-nographic AUTO_TEST=none
```

需要测试多个普通请求队列时，还可以设置，最大值为 64：

```sh
DRAGONOS_VIRTIOFS_NUM_REQUEST_QUEUES=2 make qemu-virtiofs-nographic AUTO_TEST=none
```

## 在 DragonOS 中运行

进入 DragonOS 后，先挂载 debugfs：

```sh
mkdir -p /tmp/dbg
mount -t debugfs debugfs /tmp/dbg
```

逐 opcode、response reuse/zero 和 pool 详细统计默认关闭，避免正常热路径承担额外原子读改写开销。
读取 `/tmp/dbg/fuse/stats` 不会隐式改变观测模式。诊断轮次必须在 workload 前显式启用并核对模式：

```sh
printf 'detailed\n' 1<> /tmp/dbg/fuse/stats_mode
test "$(cat /tmp/dbg/fuse/stats_mode)" = detailed
```

`virtiofs_bench` 设置 `VIRTIOFS_STATS_PATH` 后只负责读取前后快照，不负责改变全局模式。启用 detailed 前
发生的挂载或请求不会计入详细字段。当前 off 模式并不保留全部 direct-DMA 与 queue/reservation owner
计数，因此正式 quiescence 门禁尚不能依赖这些恒零字段；P0 必须先把最低限度 owner gauge 做成真正
always-on，或证明覆盖所有终态的 totals 守恒式，并同步修正 debugfs 控制面说明。

默认完整运行：

```sh
VIRTIOFS_STATS_PATH=/tmp/dbg/fuse/stats /bin/virtiofs_bench
mount | grep virtiofs || echo no_virtiofs_mount
```

性能验收和指标归因必须分开运行。纯性能轮次不要设置 `VIRTIOFS_STATS_PATH`，此时 benchmark 不读取
debugfs，也不会开启逐 opcode 等详细统计：

```sh
VIRTIOFS_STATS_PATH= /bin/virtiofs_bench --workload metadata --files 64
VIRTIOFS_STATS_PATH= /bin/virtiofs_bench --workload sequential --file-size 4194304
```

每个版本预热后至少运行 5 轮，采用 baseline/optimized/baseline 的交替顺序并比较中位数及范围。另起
设置 `VIRTIOFS_STATS_PATH=/tmp/dbg/fuse/stats` 的诊断轮次验证请求数量、复制和分配变化，不能把诊断
轮次耗时当作无观测开销的端到端性能。

小规模 smoke 运行：

```sh
VIRTIOFS_STATS_PATH=/tmp/dbg/fuse/stats \
/bin/virtiofs_bench --workload metadata --files 2

VIRTIOFS_STATS_PATH=/tmp/dbg/fuse/stats \
/bin/virtiofs_bench --workload sequential --file-size 65536
```

显式指定完整参数：

```sh
VIRTIOFS_STATS_PATH=/tmp/dbg/fuse/stats \
/bin/virtiofs_bench --tag hostshare --workload all \
  --files 256 --file-size 4194304 --block-size 4096 \
  --iterations 4096 --workers 4
```

在已有挂载点上运行：

```sh
mkdir -p /tmp/host
mount -t virtiofs hostshare /tmp/host
VIRTIOFS_STATS_PATH=/tmp/dbg/fuse/stats \
/bin/virtiofs_bench --mount /tmp/host --workload all
```

使用 `--mount PATH` 时，基准测试不会卸载 `PATH`。

## 在 Linux 中运行

对照测试时，DragonOS 和 Linux 应使用相同的宿主机、`virtiofsd`、共享目录、缓存策略和基准测试参数。

下面命令只用于功能 smoke，不属于正式 Linux 性能 reference：

```sh
mkdir -p /mnt/hostshare
mount -t virtiofs hostshare /mnt/hostshare
c++ -O2 -std=c++17 -pthread virtiofs_bench.cc -o virtiofs_bench
./virtiofs_bench --mount /mnt/hostshare --workload all \
  --files 256 --file-size 4194304 --block-size 4096 \
  --iterations 4096 --workers 4
```

正式 reference 必须使用与 DragonOS runner 相同的 split `prepare`/`sequential_read`、dataset manifest、
case id、run id、cache tuple、QEMU/virtiofsd argv 和 CPU snapshot schema；设备 argv 或 guest capability
必须证明 non-DAX。Linux 侧以 tracefs FUSE tracepoint、eBPF 或能封存原始事件的等价来源生成
`read_requests`、`requested_bytes` 和 request-size buckets，再由 Linux adapter 生成 runner v4 等价的
`case-result.json`。adapter、build attestation 和负测完成前，smoke 输出不得进入 A/B/A aggregator。

## 在 CubeSandbox 中运行

CubeSandbox 必须使用单实例隔离并记录 guest kernel/image SHA、busybox 配置、backend/cache、sandbox ID
和容器内的精确命令。测试机流程为：

```sh
ssh root@192.168.122.233
cubecli multirun /root/cubecli-busybox.json
# 保存输出的 sandbox ID，在容器中执行与本地相同的 split workload/case 参数
# 无论成功、失败或 timeout，采集完成后都执行：
cubecli unsafe destroy SANDBOX_ID
```

每个样本至少封存以下日志和清理结果：

```text
/data/log/CubeShim/
/data/log/CubeVmm/vmm.log
/data/log/Cubelet/Cubelet-req.log
/usr/local/services/cubetoolbox/cube-kernel-scf/vmlinux
```

Cube adapter 必须把容器 workload 的 result、sandbox/kernel/image 身份、CubeVmm/virtiofs backend argv、
CPU/IRQ/wakeup 指标和日志哈希写入与本地 case 等价的封存目录。`destroy` 返回码和实例消失检查是样本的
强制 cleanup artifact。若同一宿主存在无法区分的其他实例，或 DragonOS 全局 stats 不能绑定该实例，
样本无效；不得用多租户噪声推断本地 QEMU 根因。

## 输出

每个 workload 会输出一行 `result`：

```text
result workload=... status=ok errno=0 elapsed_us=... bytes=... ops=... mount=...
```

在 DragonOS 中设置 `VIRTIOFS_STATS_PATH=/tmp/dbg/fuse/stats` 后，还会输出：

```text
stats_delta workload=... key=virtiofs.bridge_submitted_total delta=...
stats_delta workload=... key=virtiofs.bridge_completed_total delta=...
stats_delta workload=... key=virtiofs.bytes_completed_total delta=...
```

优先关注这些计数器：

```text
virtiofs.bridge_submitted_total
virtiofs.bridge_completed_total
virtiofs.bytes_submitted_total
virtiofs.bytes_completed_total
virtiofs.bridge_poll_sleep_ns_total
virtiofs.response_buffer_waste_bytes
virtiofs.response_buffer_alloc_count
virtiofs.response_buffer_reuse_count
virtiofs.response_buffer_zero_bytes
virtiofs.response_pool_dropped_count
virtiofs.virtqueue_full_total
virtiofs.device_queue_depth_max
virtiofs.hiprio_vring_size_configured
virtiofs.request_queue_count_configured
virtiofs.request_vring_size_min_configured
virtiofs.sg_limit_pages_configured
virtiofs.inflight_peak
virtiofs.queue_full_blocked_current
```

`[virtiofs_opcode]` 段按 FUSE opcode 输出同口径细分指标，例如 lookup 为 opcode 1、read 为
opcode 15、write 为 opcode 16：

```text
opcode_1_request_bridge_copy_bytes
opcode_1_response_buffer_alloc_count
opcode_1_response_buffer_reuse_count
opcode_15_requests_total
opcode_16_requests_total
```

比较优化前后时，先确认目标 opcode 的 `requests_total` 在 workload 中确实增加。request bridge copy
下降和 response allocation/reuse 应分别判断；`response_buffer_zero_bytes` 只表示新建 backing 的一次
初始化，复用不再产生清零写入。pool 的容量边界由实现常量和单元测试验证；状态型 retained gauge
不做 opt-in 输出，以免首次观测前已有 buffer 导致欠计。

清零优化必须在同一个手工挂载 session 内测量，避免自动卸载清空 response pool。运行前需已按上文
显式启用 `detailed`；第一次运行用于预热各响应尺寸，第二次相同运行才是 measurement：

```sh
mkdir -p /tmp/host
mount -t virtiofs hostshare /tmp/host
VIRTIOFS_STATS_PATH=/tmp/dbg/fuse/stats /bin/virtiofs_bench --mount /tmp/host \
  --workload metadata --files 64
VIRTIOFS_STATS_PATH=/tmp/dbg/fuse/stats /bin/virtiofs_bench --mount /tmp/host \
  --workload metadata --files 64
umount /tmp/host
```

工具会为全局字段和本轮活跃 opcode 显式输出 `alloc/reuse/zero_bytes` 的零增量。measurement 阶段应
满足 `response_buffer_reuse_bytes > 0`、`response_buffer_alloc_bytes == 0`、
`response_buffer_zero_bytes == 0`；同时检查 submitted capacity、used 与 unused tail 仍保持恒等关系。

其中 `*_configured` 是配置快照，benchmark 的 `stats_delta` 通常为 0；判断队列深度是否生效时应看
`/tmp/dbg/fuse/stats` 中的绝对值。

## 对比结果

对比 DragonOS 和 Linux 时，应保持这些条件一致：

- 宿主机
- QEMU CPU 和内存配置
- `virtiofsd` 二进制及其参数
- `bin/virtiofs-share` 所在的宿主机文件系统
- workload 参数
- 冷缓存或热缓存策略

不要把缓存读结果当成 virtqueue 吞吐量。如果 DragonOS 的请求数或字节计数器没有在读取 workload 中增加，那么结果主要测到的是客户机页缓存。

不要对 `.` 或其他 rootfs 目录做 virtiofs 基准测试。应使用默认的自动挂载，或通过 `--mount` 传入明确的 virtiofs 挂载点。
