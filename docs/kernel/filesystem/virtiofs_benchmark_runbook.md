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

默认完整运行：

```sh
VIRTIOFS_STATS_PATH=/tmp/dbg/fuse/stats /bin/virtiofs_bench
mount | grep virtiofs || echo no_virtiofs_mount
```

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
virtiofs.virtqueue_full_total
virtiofs.device_queue_depth_max
virtiofs.hiprio_vring_size_configured
virtiofs.request_queue_count_configured
virtiofs.request_vring_size_min_configured
virtiofs.sg_limit_pages_configured
virtiofs.inflight_peak
virtiofs.queue_full_blocked_current
```

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
