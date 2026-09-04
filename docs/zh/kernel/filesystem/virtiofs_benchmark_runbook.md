# Virtiofs 基准测试运行手册

`virtiofs_bench` 是用于 DragonOS virtiofs 性能分析的客户机侧基准测试工具。它由 `user/apps/virtiofs_bench` 下的 DADK app 安装到 `/bin/virtiofs_bench`。

默认情况下，基准测试会把 virtiofs tag `hostshare` 挂载到 `/tmp/virtiofs_bench_mount_<pid>`，运行指定 workload，然后自动卸载并删除临时目录。只有在需要测试一个已经挂载好的 virtiofs 目录时，才使用 `--mount PATH`。

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
首次读取 `/tmp/dbg/fuse/stats` 会为本次启动后续操作开启这些详细统计。因此必须在目标 workload 前读取
一次；`virtiofs_bench` 设置 `VIRTIOFS_STATS_PATH` 后会自动完成这次基线读取。首次读取前发生的挂载或
请求不会计入详细字段，原有 aggregate 计数器不受影响。

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

```sh
mkdir -p /mnt/hostshare
mount -t virtiofs hostshare /mnt/hostshare
c++ -O2 -std=c++17 -pthread virtiofs_bench.cc -o virtiofs_bench
./virtiofs_bench --mount /mnt/hostshare --workload all \
  --files 256 --file-size 4194304 --block-size 4096 \
  --iterations 4096 --workers 4
```

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

清零优化必须在同一个手工挂载 session 内测量，避免自动卸载清空 response pool。第一次运行用于启用
detailed stats 并预热各响应尺寸，第二次相同运行才是 measurement：

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
