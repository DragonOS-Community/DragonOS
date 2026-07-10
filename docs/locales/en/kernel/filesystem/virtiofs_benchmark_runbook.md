# Virtiofs Benchmark Runbook

`virtiofs_bench` is a guest-side benchmark for DragonOS virtiofs performance work. It is installed at `/bin/virtiofs_bench` by the DADK app in `user/apps/virtiofs_bench`.

By default, the benchmark mounts virtiofs tag `hostshare` on `/tmp/virtiofs_bench_mount_<pid>`, runs the workload, then unmounts and removes the temporary directory. Use `--mount PATH` only when you want to benchmark an already mounted virtiofs directory.

## Build

From the DragonOS repository root:

```sh
make user
SKIP_GRUB=1 make write_diskimage
```

Quick host compile check:

```sh
make -C user/apps/virtiofs_bench clean all
make -C user/apps/virtiofs_bench clean
```

## Start Virtiofs

Create the local environment file:

```sh
cp tools/virtiofs/env.sh.example tools/virtiofs/env.sh
```

The default shared directory is:

```text
bin/virtiofs-share
```

Prepare the smoke-test files:

```sh
mkdir -p bin/virtiofs-share
printf 'virtiofs-host-file\n' > bin/virtiofs-share/hello.txt
cp /bin/busybox bin/virtiofs-share/busybox
chmod 755 bin/virtiofs-share/busybox
```

Start the backend and guest:

```sh
make virtiofsd
make qemu-virtiofs-nographic AUTO_TEST=none
```

Run the two commands in separate terminals. The QEMU command exposes tag `hostshare`.

To validate different virtqueue depths, pass an explicit queue size to the QEMU device:

```sh
DRAGONOS_VIRTIOFS_QUEUE_SIZE=8 make qemu-virtiofs-nographic AUTO_TEST=none
DRAGONOS_VIRTIOFS_QUEUE_SIZE=128 make qemu-virtiofs-nographic AUTO_TEST=none
```

To test multiple ordinary request queues, also set it up to 64:

```sh
DRAGONOS_VIRTIOFS_NUM_REQUEST_QUEUES=2 make qemu-virtiofs-nographic AUTO_TEST=none
```

## Run On DragonOS

Inside DragonOS:

```sh
mkdir -p /tmp/dbg
mount -t debugfs debugfs /tmp/dbg
```

Default full run:

```sh
VIRTIOFS_STATS_PATH=/tmp/dbg/fuse/stats /bin/virtiofs_bench
mount | grep virtiofs || echo no_virtiofs_mount
```

Small smoke run:

```sh
VIRTIOFS_STATS_PATH=/tmp/dbg/fuse/stats \
/bin/virtiofs_bench --workload metadata --files 2

VIRTIOFS_STATS_PATH=/tmp/dbg/fuse/stats \
/bin/virtiofs_bench --workload sequential --file-size 65536
```

Explicit full run:

```sh
VIRTIOFS_STATS_PATH=/tmp/dbg/fuse/stats \
/bin/virtiofs_bench --tag hostshare --workload all \
  --files 256 --file-size 4194304 --block-size 4096 \
  --iterations 4096 --workers 4
```

Run on an existing mount:

```sh
mkdir -p /tmp/host
mount -t virtiofs hostshare /tmp/host
VIRTIOFS_STATS_PATH=/tmp/dbg/fuse/stats \
/bin/virtiofs_bench --mount /tmp/host --workload all
```

When `--mount PATH` is used, the benchmark does not unmount `PATH`.

## Run On Linux

Use the same host, `virtiofsd`, shared directory, cache policy, and benchmark parameters.

```sh
mkdir -p /mnt/hostshare
mount -t virtiofs hostshare /mnt/hostshare
c++ -O2 -std=c++17 -pthread virtiofs_bench.cc -o virtiofs_bench
./virtiofs_bench --mount /mnt/hostshare --workload all \
  --files 256 --file-size 4194304 --block-size 4096 \
  --iterations 4096 --workers 4
```

## Output

Each workload prints one `result` line:

```text
result workload=... status=ok errno=0 elapsed_us=... bytes=... ops=... mount=...
```

On DragonOS, set `VIRTIOFS_STATS_PATH=/tmp/dbg/fuse/stats` to also print:

```text
stats_delta workload=... key=virtiofs.bridge_submitted_total delta=...
stats_delta workload=... key=virtiofs.bridge_completed_total delta=...
stats_delta workload=... key=virtiofs.bytes_completed_total delta=...
```

Counters to watch first:

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

The `*_configured` fields are configuration snapshots, so their `stats_delta` is usually 0.
Check their absolute values in `/tmp/dbg/fuse/stats` when verifying whether queue depth took effect.

## Compare Results

Keep these identical between DragonOS and Linux:

- host machine
- QEMU CPU and memory
- `virtiofsd` binary and options
- backing filesystem for `bin/virtiofs-share`
- workload parameters
- cold or warm cache policy

Do not treat cached reads as virtqueue throughput. If DragonOS request or byte counters do not increase during a read workload, the result is mostly guest page cache.

Do not benchmark `.` or another rootfs directory. Use the default automatic mount or pass an explicit virtiofs mount with `--mount`.
