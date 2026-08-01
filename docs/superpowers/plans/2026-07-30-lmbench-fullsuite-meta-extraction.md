# LMbench 全量测例 .meta 提取 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 host 用 Docker ubuntu:24.04 容器跑 guest 同款 lmbench archive 二进制，为 48 个测例提取/复核 `.meta`，扩 whitelist 全量，产出探查报告。

**Architecture:** 一次性手操探查——下载 lmbench archive 解压，docker 24.04 容器内直接跑 lmbench 二进制（参数参考各 `.sh`），按"延迟类/带宽类/ops 表类"三类映射规则把输出格式写成 `.meta`。不启 QEMU、不改任何 guest 文件（`env.sh`/`.sh`/`init.sh`）、不做 QEMU 验证。

**Tech Stack:** Docker ubuntu:24.04 容器、lmbench 3.0-a9 预编译二进制（Ubuntu 24.04 glibc 2.39）、BusyBox `sh`（host 侧 `.meta` 抽取验证用 `runner/run.sh` 的 `extract_value`）。

## Global Constraints

- host glibc 2.35 跑不了 24.04 二进制 → 必须用 `docker run ubuntu:24.04` 容器。
- lmbench 二进制来源：`user/dadk/config/all/lmbench_bin_ubuntu2404.toml` 的 `source-path`（`https://mirrors.dragonos.org.cn/pub/third_party/lmbench/lmbench-ubuntu2404-202511301014-36c2fb2d084343e098b5e343576d4fc0.tar.xz`），解压后含 `lib/lmbench/bin/x86_64-linux-gnu/*` + `lib/x86_64-linux-gnu/*` glibc 2.39。
- `env.sh`/`.sh`/`init.sh` 一律不改（`git diff` 最终无 guest 文件改动）。
- QEMU 验证不纳入本轮。
- 代码注释用英文；commit 用 `feat(lmbench):` / `docs(lmbench):` 前缀。
- `.meta` 路径：`user/apps/tests/benchmark/lmbench/runner/test_cases/<name>.meta`。

---

## File Structure

- **Create/Modify**: `runner/test_cases/<name>.meta` × 40 个新写 + 8 个复核（`mem_copy_bw`/`process_fork_lat`/`pipe_lat`/`unix_lat`/`vfs_open_lat`/`tcp_loopback_lat`/`signal_catch_lat`/`ext4_create_delete_files_10k_ops`）。
- **Modify**: `whitelist.txt`（扩到全量 48）。
- **Create**: `docs/superpowers/specs/2026-07-30-lmbench-fullsuite-survey.md`（探查报告）。
- **不改**: `env.sh` / `*.sh` / `init.sh` / `run.sh` / `collect_results.py`。

### `.meta` 通用模板

```
# Extraction/metadata for <name> (KEY=VALUE; parsed by run.sh)
CATEGORY=<category>
BINARY=<lmbench tool>
METRIC_TYPE=latency|bandwidth|ops
UNIT=microseconds|MB/s|ops/sec
BIGGER_IS_BETTER=0|1
SEARCH_PATTERN=<锚定词，跑后从输出填>
RESULT_INDEX=NF-1|NF|<数字>
NTH_OCCURRENCE=1
DESCRIPTION=<一句描述>
```

### 映射规则（三类）

- **延迟类**（输出 `<描述>: <N> microseconds`）：`SEARCH_PATTERN=<描述特征词>`, `RESULT_INDEX=NF-1`, `BIGGER_IS_BETTER=0`, `UNIT=microseconds`, `METRIC_TYPE=latency`。
- **带宽类**（输出 `<size> <bw>` 或 `... <N> MB/sec`）：`SEARCH_PATTERN=^[0-9]` 或锚定词, `RESULT_INDEX=NF`, `BIGGER_IS_BETTER=1`, `UNIT=MB/s`, `METRIC_TYPE=bandwidth`。
- **ops/表类**（`lat_fs` 多 size 表）：`SEARCH_PATTERN=^<size>`, `RESULT_INDEX=<列号>`, `BIGGER_IS_BETTER=1`, `UNIT=ops/sec`, `METRIC_TYPE=ops`。

### banner 规避

`SEARCH_PATTERN` 锚定 lmbench 二进制输出的特征词（如 `Pipe latency`/`Signal handler`/`open/close`），不要用可能撞 `.sh` banner `=== Running XXX ===` 的宽泛词。TCP 那个 bug 的教训：原 `TCP latency` 撞 banner `=== Running TCP latency test ===` 取了 `test`，修成 `^TCP latency using`。

### host 侧 `.meta` 抽取验证（每组 Task 用）

`runner/run.sh` 支持 `LMBENCH_RUNNER_NO_MAIN=1` source 复用 `kv_get`/`extract_value`。验证一个 `.meta` 抽取正确：
```sh
LMBENCH_RUNNER_NO_MAIN=1 . runner/run.sh
META=runner/test_cases/<name>.meta
PAT=$(kv_get "$META" SEARCH_PATTERN)
IDX=$(kv_get "$META" RESULT_INDEX)
NTH=$(kv_get "$META" NTH_OCCURRENCE)
# 用容器观察到的真实输出行构造 tmp 文件，extract_value 应返回数值
printf '<真实输出行>\n' > /tmp/probe.out
extract_value /tmp/probe.out   # 应打印数值，非空
```

---

### Task 1: 环境准备——下载 archive + 启动 24.04 容器 + 验证一个二进制能跑

**Files:** 无（仅环境准备）

- [ ] **Step 1: 下载 lmbench archive**

```sh
mkdir -p /tmp/lmbench-survey
cd /tmp/lmbench-survey
curl -fL -o lmbench.tar.xz "https://mirrors.dragonos.org.cn/pub/third_party/lmbench/lmbench-ubuntu2404-202511301014-36c2fb2d084343e098b5e343576d4fc0.tar.xz"
tar xf lmbench.tar.xz
ls lib/lmbench/bin/x86_64-linux-gnu/ | head
```
Expected: 解压后 `lib/lmbench/bin/x86_64-linux-gnu/` 下有 `lat_fifo`/`lat_pipe`/`bw_mem` 等二进制。

- [ ] **Step 2: 启动 24.04 容器并验证一个二进制能跑**

```sh
docker run --rm -v /tmp/lmbench-survey:/srv -it ubuntu:24.04 bash
# 容器内：
/srv/lib/lmbench/bin/x86_64-linux-gnu/lat_pipe -P 1
```
Expected: 打印类似 `Pipe latency: <N> microseconds`（证明 glibc 2.39 兼容、二进制可跑）。记录输出格式。

- [ ] **Step 3: 在容器内建等价环境变量与 dummy 文件**

容器内执行（后续 Task 复用）：
```sh
export BIN=/srv/lib/lmbench/bin/x86_64-linux-gnu
export WORK=/srv/work
mkdir -p $WORK
dd if=/dev/zero of=$WORK/test_file bs=1M count=512 2>/dev/null
dd if=/dev/zero of=$WORK/zero_file bs=1M count=512 2>/dev/null
```
Expected: `$WORK/test_file`、`$WORK/zero_file` 存在。

- [ ] **Step 4: 保持容器开启，进入 Task 2**

容器保持运行（用 `-it` 交互）。后续 Task 在同一容器内跑。若退出需重启：`docker run --rm -v /tmp/lmbench-survey:/srv -it ubuntu:24.04 bash` + 重做 Step 3 的 export/建文件。

---

### Task 2: memory 组提取 .meta

**测例**（6）：`mem_copy_bw`（已有，复核）、`mem_read_bw`、`mem_write_bw`、`mem_mmap_bw`、`mem_mmap_lat`、`mem_pagefault_lat`。

**lmbench 调用**（容器内，`$BIN` 已 export）：
| 测例 | 容器命令 |
|---|---|
| mem_copy_bw | `$BIN/bw_mem -P 1 -N 3 64m fcp` |
| mem_read_bw | `$BIN/bw_mem -P 1 -N 50 512m frd` |
| mem_write_bw | `$BIN/bw_mem -P 1 -N 50 512m fwr` |
| mem_mmap_bw | `$BIN/bw_mmap_rd -W 30 -N 300 256m mmap_only $WORK/test_file` |
| mem_mmap_lat | `sudo $BIN/lat_mmap 4m $WORK/test_file`（容器内可能无 sudo，改 `$BIN/lat_mmap 4m $WORK/test_file`） |
| mem_pagefault_lat | `$BIN/lat_pagefault -P 1 $WORK/test_file` |

- [ ] **Step 1: 逐个跑并记录输出**

容器内依次跑上表命令，记录每个的输出行格式。`bw_mem`/`bw_mmap_rd` 属带宽类（`<size> <bw>`），`lat_mmap`/`lat_pagefault` 属延迟类（`...: <N> microseconds`）。

- [ ] **Step 2: 写/复核 6 个 .meta**

按映射规则写 `runner/test_cases/<name>.meta`。带宽类 `RESULT_INDEX=NF`，延迟类 `RESULT_INDEX=NF-1`，`SEARCH_PATTERN` 用观察到的特征词。`mem_copy_bw.meta` 复核现有规则与容器输出一致。

- [ ] **Step 3: host 侧抽样验证抽取**

退出容器或新开 host shell：
```sh
cd /home/yuming/project_ongoing/DragonOSDev/DragonOS/.worktrees/feat-lmbench
LMBENCH_RUNNER_NO_MAIN=1 . runner/run.sh
for m in mem_read_bw mem_mmap_lat mem_pagefault_lat; do
  PAT=$(kv_get runner/test_cases/$m.meta SEARCH_PATTERN)
  IDX=$(kv_get runner/test_cases/$m.meta RESULT_INDEX)
  NTH=$(kv_get runner/test_cases/$m.meta NTH_OCCURRENCE)
  printf '<该测例容器输出行>\n' > /tmp/probe.out
  printf '%s -> %s\n' "$m" "$(extract_value /tmp/probe.out)"
done
```
Expected: 每个 `extract_value` 打印数值（非空），证明 `.meta` 规则正确。

- [ ] **Step 4: Commit**

```sh
git add runner/test_cases/mem_read_bw.meta runner/test_cases/mem_write_bw.meta runner/test_cases/mem_mmap_bw.meta runner/test_cases/mem_mmap_lat.meta runner/test_cases/mem_pagefault_lat.meta
# mem_copy_bw.meta 若复核有改动也 add
git commit -m "feat(lmbench): add .meta for memory bandwidth/latency tests"
```

---

### Task 3: process 组提取 .meta

**测例**（5）：`process_fork_lat`（已有，复核）、`process_exec_lat`、`process_ctx_lat`、`process_getppid_lat`、`process_shell_lat`。

**lmbench 调用**：
| 测例 | 容器命令 |
|---|---|
| process_fork_lat | `$BIN/lat_proc -P 1 fork` |
| process_exec_lat | `$BIN/lat_proc -P 1 exec` |
| process_shell_lat | `$BIN/lat_proc -P 1 shell` |
| process_ctx_lat | `$BIN/lat_ctx -P 1 18` |
| process_getppid_lat | `$BIN/lat_syscall -P 1 null` |

- [ ] **Step 1: 逐个跑并记录输出**

`lat_proc fork/exec/shell` 输出 `... fork/exec/shell latency: <N> microseconds`（延迟类）。`lat_ctx` 输出 `Context switch latency: ...`（延迟类）。`lat_syscall null` 输出 `Null syscall latency: ...`（延迟类，但测例名是 getppid——记录实际输出，DESCRIPTION 按实际写）。

- [ ] **Step 2: 写/复核 5 个 .meta**

全部延迟类：`RESULT_INDEX=NF-1`，`SEARCH_PATTERN` 锚定输出特征词（如 `fork latency`/`exec latency`/`shell latency`/`Context switch`/`Null syscall`）。

- [ ] **Step 3: host 侧抽样验证**

```sh
LMBENCH_RUNNER_NO_MAIN=1 . runner/run.sh
for m in process_exec_lat process_ctx_lat process_getppid_lat; do
  PAT=$(kv_get runner/test_cases/$m.meta SEARCH_PATTERN)
  IDX=$(kv_get runner/test_cases/$m.meta RESULT_INDEX)
  NTH=$(kv_get runner/test_cases/$m.meta NTH_OCCURRENCE)
  printf '<容器输出行>\n' > /tmp/probe.out
  printf '%s -> %s\n' "$m" "$(extract_value /tmp/probe.out)"
done
```
Expected: 每个打印数值。

- [ ] **Step 4: Commit**

```sh
git add runner/test_cases/process_exec_lat.meta runner/test_cases/process_ctx_lat.meta runner/test_cases/process_getppid_lat.meta runner/test_cases/process_shell_lat.meta
git commit -m "feat(lmbench): add .meta for process latency tests"
```

---

### Task 4: ipc 组提取 .meta

**测例**（7）：`pipe_lat`（已有，复核）、`pipe_bw`、`fifo_lat`、`unix_lat`（已有，复核）、`unix_bw`、`unix_connect_lat`、`semaphore_lat`。

**lmbench 调用**：
| 测例 | 容器命令 |
|---|---|
| pipe_lat | `$BIN/lat_pipe -P 1` |
| pipe_bw | `$BIN/bw_pipe -P 1` |
| fifo_lat | `$BIN/lat_fifo -P 1` |
| unix_lat | `$BIN/lat_unix -P 1` |
| unix_bw | `$BIN/bw_unix -P 1` |
| unix_connect_lat | `$BIN/lat_unix_connect -s &`（server 后台），然后 `$BIN/lat_unix_connect 127.0.0.1`（client；实际参数跑后确认） |
| semaphore_lat | `$BIN/lat_sem -P 1 -N 21` |

- [ ] **Step 1: 逐个跑并记录输出**

`lat_pipe`/`lat_fifo`/`lat_unix`/`lat_sem` 延迟类（`...: <N> microseconds`）。`bw_pipe`/`bw_unix` 带宽类（`<size> <bw>`）。`lat_unix_connect` 需起 server 后台再跑 client，记录 client 输出。

- [ ] **Step 2: 写/复核 7 个 .meta**

延迟类 `RESULT_INDEX=NF-1`，带宽类 `RESULT_INDEX=NF`。`SEARCH_PATTERN` 锚定特征词（如 `Pipe latency`/`FIFO latency`/`Unix socket latency`/`Semaphore latency`）。复核 `pipe_lat.meta`/`unix_lat.meta`。

- [ ] **Step 3: host 侧抽样验证**

```sh
LMBENCH_RUNNER_NO_MAIN=1 . runner/run.sh
for m in pipe_bw fifo_lat unix_bw unix_connect_lat semaphore_lat; do
  PAT=$(kv_get runner/test_cases/$m.meta SEARCH_PATTERN)
  IDX=$(kv_get runner/test_cases/$m.meta RESULT_INDEX)
  NTH=$(kv_get runner/test_cases/$m.meta NTH_OCCURRENCE)
  printf '<容器输出行>\n' > /tmp/probe.out
  printf '%s -> %s\n' "$m" "$(extract_value /tmp/probe.out)"
done
```
Expected: 每个打印数值。

- [ ] **Step 4: Commit**

```sh
git add runner/test_cases/pipe_bw.meta runner/test_cases/fifo_lat.meta runner/test_cases/unix_bw.meta runner/test_cases/unix_connect_lat.meta runner/test_cases/semaphore_lat.meta
git commit -m "feat(lmbench): add .meta for ipc tests"
```

---

### Task 5: signal 组提取 .meta

**测例**（3）：`signal_catch_lat`（已有，复核）、`signal_install_lat`、`signal_prot_lat`。

**lmbench 调用**：
| 测例 | 容器命令 |
|---|---|
| signal_catch_lat | `$BIN/lat_sig -P 1 catch` |
| signal_install_lat | `$BIN/lat_sig -P 1 install` |
| signal_prot_lat | `$BIN/lat_sig -W 30 -N 300 prot $WORK/test_file` |

- [ ] **Step 1: 逐个跑并记录输出**

`lat_sig catch/install/prot` 延迟类（`Signal handler overhead: <N> microseconds` 或类似）。记录各模式实际输出特征词。

- [ ] **Step 2: 写/复核 3 个 .meta**

延迟类 `RESULT_INDEX=NF-1`，`SEARCH_PATTERN` 锚定各模式输出特征词。复核 `signal_catch_lat.meta`。

- [ ] **Step 3: host 侧抽样验证**

```sh
LMBENCH_RUNNER_NO_MAIN=1 . runner/run.sh
for m in signal_install_lat signal_prot_lat; do
  PAT=$(kv_get runner/test_cases/$m.meta SEARCH_PATTERN)
  IDX=$(kv_get runner/test_cases/$m.meta RESULT_INDEX)
  NTH=$(kv_get runner/test_cases/$m.meta NTH_OCCURRENCE)
  printf '<容器输出行>\n' > /tmp/probe.out
  printf '%s -> %s\n' "$m" "$(extract_value /tmp/probe.out)"
done
```
Expected: 每个打印数值。

- [ ] **Step 4: Commit**

```sh
git add runner/test_cases/signal_install_lat.meta runner/test_cases/signal_prot_lat.meta
git commit -m "feat(lmbench): add .meta for signal latency tests"
```

---

### Task 6: vfs 组提取 .meta

**测例**（8）：`vfs_open_lat`（已有，复核）、`vfs_read_lat`、`vfs_write_lat`、`vfs_fstat_lat`、`vfs_stat_lat`、`vfs_fcntl_lat`、`vfs_select_lat`、`vfs_read_pagecache_bw`。

**lmbench 调用**：
| 测例 | 容器命令 |
|---|---|
| vfs_open_lat | `$BIN/lat_syscall -P 1 -W 1 -N 3 open "$testfile"`（需先 `touch /tmp/testfile; testfile=/tmp/testfile`） |
| vfs_read_lat | `$BIN/lat_syscall -P 1 read` |
| vfs_write_lat | `$BIN/lat_syscall -P 1 write` |
| vfs_fstat_lat | `$BIN/lat_syscall -P 1 fstat test_file`（需 `test_file` 存在） |
| vfs_stat_lat | `$BIN/lat_syscall -P 1 -W 1000 -N 1000 stat testfile`（需 `testfile` 存在） |
| vfs_fcntl_lat | `$BIN/lat_fcntl -P 1 -W 30 -N 200` |
| vfs_select_lat | `$BIN/lat_select -P 1 file` |
| vfs_read_pagecache_bw | `$BIN/bw_file_rd -P 1 -W 30 -N 300 512m io_only $WORK/test_file` |

- [ ] **Step 1: 建所需文件并逐个跑**

```sh
touch /tmp/testfile
cd /tmp
```
然后跑上表命令记录输出。`lat_syscall *` 延迟类，`lat_fcntl`/`lat_select` 延迟类，`bw_file_rd` 带宽类。

- [ ] **Step 2: 写/复核 8 个 .meta**

延迟类 `RESULT_INDEX=NF-1`（`lat_syscall open` 锚定 `open/close`，`read` 锚定 `read`，`write` 锚定 `write`，`fstat` 锚定 `fstat`，`stat` 锚定 `stat`；`lat_fcntl`/`lat_select` 按实际输出特征词）。`bw_file_rd` 带宽类 `RESULT_INDEX=NF`。复核 `vfs_open_lat.meta`。

- [ ] **Step 3: host 侧抽样验证**

```sh
LMBENCH_RUNNER_NO_MAIN=1 . runner/run.sh
for m in vfs_read_lat vfs_write_lat vfs_fstat_lat vfs_stat_lat vfs_fcntl_lat vfs_select_lat vfs_read_pagecache_bw; do
  PAT=$(kv_get runner/test_cases/$m.meta SEARCH_PATTERN)
  IDX=$(kv_get runner/test_cases/$m.meta RESULT_INDEX)
  NTH=$(kv_get runner/test_cases/$m.meta NTH_OCCURRENCE)
  printf '<容器输出行>\n' > /tmp/probe.out
  printf '%s -> %s\n' "$m" "$(extract_value /tmp/probe.out)"
done
```
Expected: 每个打印数值。

- [ ] **Step 4: Commit**

```sh
git add runner/test_cases/vfs_read_lat.meta runner/test_cases/vfs_write_lat.meta runner/test_cases/vfs_fstat_lat.meta runner/test_cases/vfs_stat_lat.meta runner/test_cases/vfs_fcntl_lat.meta runner/test_cases/vfs_select_lat.meta runner/test_cases/vfs_read_pagecache_bw.meta
git commit -m "feat(lmbench): add .meta for vfs tests"
```

---

### Task 7: filesystem 组提取 .meta

**测例**（6）：`ext4_create_delete_files_10k_ops`（已有，复核）、`ext4_create_delete_files_0k_ops`、`ramfs_create_delete_files_10k_ops`、`ramfs_create_delete_files_0k_ops`、`ext4_copy_files_bw`、`ramfs_copy_files_bw`。

**lmbench 调用**：
| 测例 | 容器命令 |
|---|---|
| ext4_create_delete_files_0k_ops | `$BIN/lat_fs -s 0k -P 1 $WORK` |
| ext4_create_delete_files_10k_ops | `$BIN/lat_fs -s 10k -P 1 $WORK` |
| ramfs_create_delete_files_0k_ops | `$BIN/lat_fs -s 0k -P 1 -W 30 -N 200 /dev/shm`（用 tmpfs 替代 ramfs） |
| ramfs_create_delete_files_10k_ops | `$BIN/lat_fs -s 10k -P 1 -W 30 -N 300 /dev/shm` |
| ext4_copy_files_bw | `$BIN/lmdd if=$WORK/zero_file of=$WORK/test_file`（去 sudo，容器内 root） |
| ramfs_copy_files_bw | `$BIN/lmdd if=/dev/shm/zero_file of=/dev/shm/test_file`（先 `dd` 建 /dev/shm 文件） |

- [ ] **Step 1: 准备 /dev/shm 文件并逐个跑**

```sh
dd if=/dev/zero of=/dev/shm/zero_file bs=1M count=512 2>/dev/null
dd if=/dev/zero of=/dev/shm/test_file bs=1M count=512 2>/dev/null
```
跑上表命令记录输出。`lat_fs` 输出多 size 表（ops 表类，`^<size>` 行 + 列号）。`lmdd` 带宽类。

- [ ] **Step 2: 写/复核 6 个 .meta**

`lat_fs` ops 表类：`SEARCH_PATTERN=^0k` 或 `^10k`，`RESULT_INDEX=<creates/sec 列号，跑后确认>`，`UNIT=ops/sec`，`METRIC_TYPE=ops`。`lmdd` 带宽类。复核 `ext4_create_delete_files_10k_ops.meta`（原 `RESULT_INDEX=3`，跑后确认列号）。

- [ ] **Step 3: host 侧抽样验证**

```sh
LMBENCH_RUNNER_NO_MAIN=1 . runner/run.sh
for m in ext4_create_delete_files_0k_ops ramfs_create_delete_files_10k_ops ramfs_create_delete_files_0k_ops ext4_copy_files_bw ramfs_copy_files_bw; do
  PAT=$(kv_get runner/test_cases/$m.meta SEARCH_PATTERN)
  IDX=$(kv_get runner/test_cases/$m.meta RESULT_INDEX)
  NTH=$(kv_get runner/test_cases/$m.meta NTH_OCCURRENCE)
  printf '<容器输出行>\n' > /tmp/probe.out
  printf '%s -> %s\n' "$m" "$(extract_value /tmp/probe.out)"
done
```
Expected: 每个打印数值。

- [ ] **Step 4: Commit**

```sh
git add runner/test_cases/ext4_create_delete_files_0k_ops.meta runner/test_cases/ramfs_create_delete_files_10k_ops.meta runner/test_cases/ramfs_create_delete_files_0k_ops.meta runner/test_cases/ext4_copy_files_bw.meta runner/test_cases/ramfs_copy_files_bw.meta
git commit -m "feat(lmbench): add .meta for filesystem tests"
```

---

### Task 8: network 组提取 .meta

**测例**（11）：`tcp_loopback_lat`（已有，复核）、`tcp_loopback_bw_128`、`tcp_loopback_bw_4k`、`tcp_loopback_bw_64k`、`tcp_loopback_connect_lat`、`tcp_loopback_select_lat`、`tcp_loopback_http_bw`、`tcp_virtio_lat`、`tcp_virtio_bw_128`、`tcp_virtio_bw_64k`、`tcp_virtio_connect_lat`、`udp_loopback_lat`、`udp_virtio_lat`。

> 注：实际 13 个（含 udp 2 个）。本组是最大组。

**lmbench 调用**（server+client 类：先起 server 后台，再跑 client；容器内全用 127.0.0.1）：
| 测例 | 容器命令（server + client） |
|---|---|
| tcp_loopback_lat | `$BIN/lat_tcp -s 127.0.0.1 -b 1 &` ; `$BIN/lat_tcp 127.0.0.1` |
| tcp_loopback_bw_128 | `$BIN/bw_tcp -s 127.0.0.1 -b 1 &` ; `$BIN/bw_tcp 127.0.0.1`（client 参数跑后确认） |
| tcp_loopback_bw_4k | 同上 |
| tcp_loopback_bw_64k | 同上 |
| tcp_loopback_connect_lat | `$BIN/lat_connect -s 127.0.0.1 &` ; `$BIN/lat_connect 127.0.0.1` |
| tcp_loopback_select_lat | `$BIN/lat_select -P 1 tcp` |
| tcp_loopback_http_bw | `$BIN/lmhttp &` ; `$BIN/bw_tcp 127.0.0.1`（http server + bw client；参数跑后确认） |
| tcp_virtio_lat | 用 127.0.0.1 替代 10.0.2.15：`$BIN/lat_tcp -s 127.0.0.1 -b 1 &` ; `$BIN/lat_tcp 127.0.0.1` |
| tcp_virtio_bw_128 | `$BIN/bw_tcp -s 127.0.0.1 -b 1 &` ; `$BIN/bw_tcp 127.0.0.1` |
| tcp_virtio_bw_64k | 同上 |
| tcp_virtio_connect_lat | `$BIN/lat_connect -s 127.0.0.1 -b 1000 &` ; `$BIN/lat_connect 127.0.0.1` |
| udp_loopback_lat | `$BIN/lat_udp -s 127.0.0.1 &` ; `$BIN/lat_udp 127.0.0.1` |
| udp_virtio_lat | `$BIN/lat_udp -s 127.0.0.1 &` ; `$BIN/lat_udp 127.0.0.1` |

- [ ] **Step 1: 逐个跑（server 后台 + client）并记录 client 输出**

`lat_tcp`/`lat_connect`/`lat_udp` 延迟类（`...: <N> microseconds`）。`bw_tcp` 带宽类。`lat_select tcp` 延迟类。`lmhttp`+`bw_tcp` http 带宽类。注意：server 起后台后等 1 秒再跑 client（`sleep 1`）。每个测例跑完 `kill %1` 清 server。

- [ ] **Step 2: 写/复核 13 个 .meta**

延迟类 `RESULT_INDEX=NF-1`，带宽类 `RESULT_INDEX=NF`。`SEARCH_PATTERN` 锚定特征词（`lat_tcp` 用 `^TCP latency using`——复核 `tcp_loopback_lat.meta`；`lat_connect` 锚定 `Connection latency`；`lat_udp` 锚定 `UDP latency`；`lat_select` 锚定 `Select latency`；`bw_tcp` 按输出格式）。virtio 类与 loopback 类输出格式相同（IP 不影响格式），`.meta` 的 SEARCH_PATTERN 可相同，DESCRIPTION 区分 virtio/loopback。

- [ ] **Step 3: host 侧抽样验证**

```sh
LMBENCH_RUNNER_NO_MAIN=1 . runner/run.sh
for m in tcp_loopback_bw_128 tcp_loopback_connect_lat tcp_loopback_select_lat tcp_virtio_lat udp_loopback_lat udp_virtio_lat; do
  PAT=$(kv_get runner/test_cases/$m.meta SEARCH_PATTERN)
  IDX=$(kv_get runner/test_cases/$m.meta RESULT_INDEX)
  NTH=$(kv_get runner/test_cases/$m.meta NTH_OCCURRENCE)
  printf '<容器输出行>\n' > /tmp/probe.out
  printf '%s -> %s\n' "$m" "$(extract_value /tmp/probe.out)"
done
```
Expected: 每个打印数值。

- [ ] **Step 4: Commit**

```sh
git add runner/test_cases/tcp_loopback_bw_128.meta runner/test_cases/tcp_loopback_bw_4k.meta runner/test_cases/tcp_loopback_bw_64k.meta runner/test_cases/tcp_loopback_connect_lat.meta runner/test_cases/tcp_loopback_select_lat.meta runner/test_cases/tcp_loopback_http_bw.meta runner/test_cases/tcp_virtio_lat.meta runner/test_cases/tcp_virtio_bw_128.meta runner/test_cases/tcp_virtio_bw_64k.meta runner/test_cases/tcp_virtio_connect_lat.meta runner/test_cases/udp_loopback_lat.meta runner/test_cases/udp_virtio_lat.meta
git commit -m "feat(lmbench): add .meta for network tests"
```

---

### Task 9: whitelist 扩全量 + 探查报告 + 最终验证

**Files:**
- Modify: `whitelist.txt`
- Create: `docs/superpowers/specs/2026-07-30-lmbench-fullsuite-survey.md`

- [ ] **Step 1: 扩 whitelist 到全量 48**

把 `whitelist.txt` 替换为全量 48 个测例名（每行一个，按类别分组注释，参考现有 8 个的格式）：
```
# memory
mem_copy_bw
mem_read_bw
mem_write_bw
mem_mmap_bw
mem_mmap_lat
mem_pagefault_lat

# process
process_fork_lat
process_exec_lat
process_ctx_lat
process_getppid_lat
process_shell_lat

# ipc
pipe_lat
pipe_bw
fifo_lat
unix_lat
unix_bw
unix_connect_lat
semaphore_lat

# signal
signal_catch_lat
signal_install_lat
signal_prot_lat

# vfs
vfs_open_lat
vfs_read_lat
vfs_write_lat
vfs_fstat_lat
vfs_stat_lat
vfs_fcntl_lat
vfs_select_lat
vfs_read_pagecache_bw

# filesystem
ext4_create_delete_files_0k_ops
ext4_create_delete_files_10k_ops
ramfs_create_delete_files_0k_ops
ramfs_create_delete_files_10k_ops
ext4_copy_files_bw
ramfs_copy_files_bw

# network
tcp_loopback_lat
tcp_loopback_bw_128
tcp_loopback_bw_4k
tcp_loopback_bw_64k
tcp_loopback_connect_lat
tcp_loopback_select_lat
tcp_loopback_http_bw
tcp_virtio_lat
tcp_virtio_bw_128
tcp_virtio_bw_64k
tcp_virtio_connect_lat
udp_loopback_lat
udp_virtio_lat
```

- [ ] **Step 2: 验证 .meta 齐全**

```sh
cd /home/yuming/project_ongoing/DragonOSDev/DragonOS/.worktrees/feat-lmbench/user/apps/tests/benchmark/lmbench
echo "meta count: $(ls runner/test_cases/*.meta | wc -l)"
echo "sh count: $(ls runner/test_cases/*.sh | grep -v '^runner/test_cases/test_' | wc -l)"
```
Expected: 两者都 = 48。

- [ ] **Step 3: 验证每个 whitelist 条目有对应 .meta**

```sh
while read -r line; do
  case "$line" in ''|\#*) continue ;; esac
  [ -f "runner/test_cases/$line.meta" ] || echo "MISSING meta: $line"
done < whitelist.txt
echo "check done"
```
Expected: 只打印 `check done`，无 `MISSING meta`。

- [ ] **Step 4: 写探查报告**

创建 `docs/superpowers/specs/2026-07-30-lmbench-fullsuite-survey.md`，每测例一条：lmbench 工具 + 容器跑法（哪组等价环境）+ 容器实际输出样本（一行）+ `.meta` 抽取规则（SEARCH_PATTERN/RESULT_INDEX）+ guest 校准状态（已校准/待 guest 校准/guest 可能不支持）。guest 校准状态：
- 已校准：原 8 个（`mem_copy_bw`/`process_fork_lat`/`pipe_lat`/`unix_lat`/`vfs_open_lat`/`tcp_loopback_lat`/`signal_catch_lat`/`ext4_create_delete_files_10k_ops`）。
- 待 guest 校准：host 提取的 40 个。
- guest 可能不支持：基于工具用的 syscall 推断标注（如 `lat_connect`/`lat_select`/`lat_fcntl` 可能依赖内核未实现特性），留后续 QEMU 验证确认。

- [ ] **Step 5: 验证未改 guest 文件**

```sh
cd /home/yuming/project_ongoing/DragonOSDev/DragonOS/.worktrees/feat-lmbench
git diff --name-only HEAD~9 HEAD -- user/apps/tests/benchmark/lmbench/runner/run.sh user/apps/tests/benchmark/lmbench/runner/env.sh user/apps/tests/benchmark/lmbench/runner/init.sh user/apps/tests/benchmark/lmbench/runner/clean_up.sh user/apps/tests/benchmark/lmbench/runner/test_cases/*.sh
```
Expected: 空输出（未改 `run.sh`/`env.sh`/`init.sh`/`clean_up.sh`/任何 `.sh`）。

- [ ] **Step 6: Commit whitelist + 报告**

```sh
git add whitelist.txt docs/superpowers/specs/2026-07-30-lmbench-fullsuite-survey.md
git commit -m "feat(lmbench): enable full whitelist and add survey report"
```

- [ ] **Step 7: 清理临时环境**

```sh
rm -rf /tmp/lmbench-survey
docker system prune -f 2>/dev/null || true
```

---

## Self-Review

**1. Spec coverage：**
- spec §目标1（48 个 .meta）→ Task 2-8 覆盖 6 组（memory/process/ipc/signal/vfs/filesystem/network），每组写 + 复核；Task 9 Step 2 验证齐全。
- spec §目标2（探查报告）→ Task 9 Step 4。
- spec §目标3（whitelist 全量）→ Task 9 Step 1。
- spec §硬约束（24.04 容器、guest 同款 archive、不改 guest 文件、无 QEMU）→ Global Constraints + Task 9 Step 5 验证未改 guest 文件。
- spec §流程（下载/解压/容器手操/写 .meta）→ Task 1 环境 + Task 2-8 各组。
- spec §等价环境分组 → Task 2-8 按组 + Task 7（/dev/shm 替代 ramfs）+ Task 8（127.0.0.1 替代 virtio）。
- spec §映射规则三类 → File Structure 映射规则 + 各 Task step 2。
- spec §banner 规避 → File Structure banner 规避 + Task 8 复核 TCP。
- spec §验证（48 .meta 齐全 + 报告 + 不改 guest）→ Task 9 Step 2/3/5。
- spec §不做（QEMU/内核/.sh 修复）→ Global Constraints + 无对应 Task。✓

**2. Placeholder scan：** `.meta` 的 `SEARCH_PATTERN` 实际值标为"跑后从输出填"——这是探查任务的本质（必须跑后观察），非"TODO"占位；每步有确切跑命令 + .meta 模板 + 映射规则 + 验证。✓

**3. Type consistency：** `LMBENCH_RUNNER_NO_MAIN=1 . runner/run.sh` + `kv_get`/`extract_value` 在各 Task 一致；`$BIN`/`$WORK` 环境变量在 Task 1 定义、Task 2-8 复用；`.meta` 字段名（SEARCH_PATTERN/RESULT_INDEX/NTH_OCCURRENCE）与 `run.sh` 的 `kv_get` 一致。✓

## Execution Handoff

计划已保存。鉴于本任务手操性质（docker 内跑 lmbench、观察输出、填 `.meta`），**Inline Execution** 更合适——需要交互式观察输出填值，fresh subagent 难处理。建议 inline 执行。