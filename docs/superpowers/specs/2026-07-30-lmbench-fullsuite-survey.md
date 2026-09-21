# LMbench 全量测例 .meta 提取探查报告

- 日期：2026-07-30
- 分支：feat/lmbench
- 对应 spec：`2026-07-30-lmbench-fullsuite-meta-extraction-design.md`

## 概述

为 48 个 LMbench 测例提取/复核 `.meta` 抽取规则，扩 whitelist 全量。

**方法**：host（WSL2 glibc 2.35）跑不了 24.04 glibc 2.39 编译的 lmbench 二进制，改用 archive 自带的 `ld-linux-x86-64.so.2` + `--library-path` 直接在 host 跑（`/tmp/lmbench-survey/run.sh` helper），不启 QEMU、不依赖 docker（docker.io 拉取超时）。

- 能跑通的工具：直接跑看完整输出。
- host 跑不通/卡死的工具（lat_fifo/bw_unix/lat_sem/lat_unix_connect/lat_syscall null/bw_tcp 等）：用 `strings` 从二进制提取输出格式字符串 + lmbench 标准格式推断，标注"待 guest 校准"。
- 每个新 `.meta` 用 host 侧 `extract_value`（busybox sh，和 guest 一致）抽样验证抽取正确。

## .meta 提取结果（按组）

格式列说明：`SEARCH_PATTERN` / `RESULT_INDEX`。校准状态：**已校准**=原 8 个 e2e 验证；**host 确认**=host 跑通看输出；**strings 推断**=host 跑不通，基于 strings + lmbench 标准格式。

### memory（6）
| 测例              | 工具                 | 输出样本                                 | SEARCH_PATTERN / IDX | 状态      |
| ----------------- | -------------------- | ---------------------------------------- | -------------------- | --------- |
| mem_copy_bw       | bw_mem fcp           | `67.11 16445.12`                         | `^[0-9]` / NF        | 已校准    |
| mem_read_bw       | bw_mem frd           | `536.87 33961.94`                        | `^[0-9]` / NF        | host 确认 |
| mem_write_bw      | bw_mem fwr           | `536.87 10045.11`                        | `^[0-9]` / NF        | host 确认 |
| mem_mmap_bw       | bw_mmap_rd mmap_only | `67.11 54358.60`                         | `^[0-9]` / NF        | host 确认 |
| mem_mmap_lat      | lat_mmap             | `4.194304 20`                            | `^[0-9]` / NF        | host 确认 |
| mem_pagefault_lat | lat_pagefault        | `Pagefaults on <f>: 0.1149 microseconds` | `^Pagefaults` / NF-1 | host 确认 |

### process（5）
| 测例                | 工具             | 输出样本                                       | SEARCH_PATTERN / IDX  | 状态                                     |
| ------------------- | ---------------- | ---------------------------------------------- | --------------------- | ---------------------------------------- |
| process_fork_lat    | lat_proc fork    | `Process fork+exit: 143.03 microseconds`       | `Process fork` / NF-1 | 已校准                                   |
| process_exec_lat    | lat_proc exec    | `Process fork+execve: 156.85 microseconds`     | `Process fork` / NF-1 | host 确认                                |
| process_shell_lat   | lat_proc shell   | `Process fork+/bin/sh -c: 936.22 microseconds` | `Process fork` / NF-1 | host 确认（需 `/var/tmp/lmbench/hello`） |
| process_ctx_lat     | lat_ctx 18       | `18 28.51`                                     | `^[0-9]` / NF         | host 确认                                |
| process_getppid_lat | lat_syscall null | `Simple null: N microseconds`（推断）          | `Simple null` / NF-1  | strings 推断（host 卡死）                |

### ipc（7）
| 测例             | 工具             | 输出样本                                          | SEARCH_PATTERN / IDX                   | 状态                      |
| ---------------- | ---------------- | ------------------------------------------------- | -------------------------------------- | ------------------------- |
| pipe_lat         | lat_pipe         | `Pipe latency: 48.27 microseconds`                | `Pipe latency` / NF-1                  | 已校准                    |
| pipe_bw          | bw_pipe          | `Pipe bandwidth: 3065.49 MB/sec`                  | `Pipe bandwidth` / NF-1                | host 确认                 |
| fifo_lat         | lat_fifo         | `Fifo latency: N microseconds`                    | `Fifo latency` / NF-1                  | strings 推断（host 报错） |
| unix_lat         | lat_unix         | `AF_UNIX sock stream latency: 47.97 microseconds` | `AF_UNIX` / NF-1                       | 已校准                    |
| unix_bw          | bw_unix          | `AF_UNIX sock stream bandwidth: N MB/sec`         | `AF_UNIX sock stream bandwidth` / NF-1 | strings 推断（host 卡死） |
| unix_connect_lat | lat_unix_connect | `UNIX connection cost: N microseconds`            | `UNIX connection cost` / NF-1          | strings 推断（host 报错） |
| semaphore_lat    | lat_sem          | `Semaphore latency: N microseconds`               | `Semaphore latency` / NF-1             | strings 推断（host 卡死） |

### signal（3）
| 测例               | 工具            | 输出样本                                      | SEARCH_PATTERN / IDX                 | 状态                         |
| ------------------ | --------------- | --------------------------------------------- | ------------------------------------ | ---------------------------- |
| signal_catch_lat   | lat_sig catch   | `Signal handler overhead: N microseconds`     | `Signal handler` / NF-1              | 已校准                       |
| signal_install_lat | lat_sig install | `Signal handler installation: N microseconds` | `Signal handler installation` / NF-1 | strings 确认                 |
| signal_prot_lat    | lat_sig prot    | `Protection fault: N microseconds`            | `Protection fault` / NF-1            | strings 确认（需 test file） |

### vfs（8）
| 测例                  | 工具               | 输出样本                             | SEARCH_PATTERN / IDX        | 状态         |
| --------------------- | ------------------ | ------------------------------------ | --------------------------- | ------------ |
| vfs_open_lat          | lat_syscall open   | `Simple open/close: N microseconds`  | `open/close` / NF-1         | 已校准       |
| vfs_read_lat          | lat_syscall read   | `Simple read: 0.10 microseconds`     | `Simple read` / NF-1        | strings 确认 |
| vfs_write_lat         | lat_syscall write  | `Simple write: N microseconds`       | `Simple write` / NF-1       | strings 确认 |
| vfs_fstat_lat         | lat_syscall fstat  | `Simple fstat: N microseconds`       | `Simple fstat` / NF-1       | strings 确认 |
| vfs_stat_lat          | lat_syscall stat   | `Simple stat: N microseconds`        | `Simple stat` / NF-1        | strings 确认 |
| vfs_fcntl_lat         | lat_fcntl          | `Fcntl lock latency: N microseconds` | `Fcntl lock latency` / NF-1 | strings 确认 |
| vfs_select_lat        | lat_select file    | `Select on N fd's: M microseconds`   | `Select on` / NF-1          | strings 确认 |
| vfs_read_pagecache_bw | bw_file_rd io_only | `<size> <bw>`                        | `^[0-9]` / NF               | strings 确认 |

### filesystem（6）
| 测例                              | 工具       | 输出样本                               | SEARCH_PATTERN / IDX | 状态                       |
| --------------------------------- | ---------- | -------------------------------------- | -------------------- | -------------------------- |
| ext4_create_delete_files_10k_ops  | lat_fs 10k | `10k\tN1\tN2\tN3`                      | `^10k` / 3           | 已校准                     |
| ext4_create_delete_files_0k_ops   | lat_fs 0k  | `0k\tN1\tN2\tN3`                       | `^0k` / 3            | host 确认                  |
| ramfs_create_delete_files_0k_ops  | lat_fs 0k  | 同上                                   | `^0k` / 3            | host 确认（用 tmpfs 替代） |
| ramfs_create_delete_files_10k_ops | lat_fs 10k | 同上                                   | `^10k` / 3           | host 确认                  |
| ext4_copy_files_bw                | lmdd       | `67.11 MB in 0.19 secs, 352.97 MB/sec` | `MB in` / NF-1       | host 确认                  |
| ramfs_copy_files_bw               | lmdd       | 同上                                   | `MB in` / NF-1       | host 确认                  |

### network（13）
| 测例                       | 工具                  | 输出样本                                      | SEARCH_PATTERN / IDX            | 状态                             |
| -------------------------- | --------------------- | --------------------------------------------- | ------------------------------- | -------------------------------- |
| tcp_loopback_lat           | lat_tcp               | `TCP latency using 127.0.0.1: N microseconds` | `^TCP latency using` / NF-1     | 已校准                           |
| tcp_loopback_bw_128/4k/64k | bw_tcp                | `socket connection: N MB/sec`                 | `socket connection` / NF-1      | strings 推断（host 卡死）        |
| tcp_loopback_connect_lat   | lat_connect           | `TCP/IP connection cost to H: N microseconds` | `TCP/IP connection cost` / NF-1 | strings 推断                     |
| tcp_loopback_select_lat    | lat_select tcp        | `Select on N tcp fd's: M microseconds`        | `Select on` / NF-1              | strings 确认                     |
| tcp_loopback_http_bw       | lat_http+lmhttp       | `Avg xfer: NKB, X ms, Y KB/sec`               | `Avg xfer` / NF-1               | strings 推断（.sh 缺 file_list） |
| tcp_virtio_lat             | lat_tcp 10.0.2.15     | 同 loopback 格式                              | `^TCP latency using` / NF-1     | strings 推断（.sh 缺 server）    |
| tcp_virtio_bw_128/64k      | bw_tcp 10.0.2.15      | 同 loopback                                   | `socket connection` / NF-1      | strings 推断                     |
| tcp_virtio_connect_lat     | lat_connect 10.0.2.15 | 同 loopback                                   | `TCP/IP connection cost` / NF-1 | strings 推断                     |
| udp_loopback_lat           | lat_udp               | `UDP latency using H: N microseconds`         | `^UDP latency using` / NF-1     | strings 确认                     |
| udp_virtio_lat             | lat_udp 10.0.2.15     | 同 loopback                                   | `^UDP latency using` / NF-1     | strings 推断                     |

## guest 校准状态分类

- **已校准（8）**：mem_copy_bw, process_fork_lat, pipe_lat, unix_lat, vfs_open_lat, tcp_loopback_lat, signal_catch_lat, ext4_create_delete_files_10k_ops——e2e 8/8 验证。
- **host 确认（~18）**：host 跑通看完整输出，格式确定。
- **strings 推断（~22）**：host 跑不通（卡死/报错），基于 `strings` 提取的输出格式字符串 + lmbench 标准格式。需 guest 校准确认（guest raw_tail 会暴露真实格式）。

## guest 可能不支持的测例（留 QEMU 验证确认）

基于工具用的 syscall / 特性推断，可能受 DragonOS 内核限制：
- `lat_connect` / `lat_select tcp` / `lat_unix_connect`：依赖 socket connect/select，内核未实现的特性可能 failed。
- `lat_sem`：semaphore syscall。
- `lat_fcntl`：fcntl lock。
- `lat_syscall null`：host 已卡死，guest 行为待确认。
- `tcp_virtio_*` / `udp_virtio_*`：`.sh` 只 client 没 server，guest 跑前需补 server 逻辑。
- `tcp_loopback_http_bw`：`.sh` 用 `lat_http < file_list` 但未建 file_list，guest 跑前需补。

这些是 guest 侧 `.sh`/环境问题，不属于本轮 `.meta` 提取范围；`.meta` 已写，待 guest 校准轮确认或修 `.sh`。

## 验证

- `.meta` 数：48（`ls runner/test_cases/*.meta | wc -l` = 48）。
- `.sh`（excl test_ parser）：48，每个都有对应 `.meta`。
- whitelist：48 条，每条都有 `.meta`。
- guest 文件未改：`git diff HEAD~7 HEAD -- runner/run.sh runner/env.sh runner/init.sh runner/clean_up.sh 'runner/test_cases/*.sh'` 为空。
- 每个新 `.meta` 用 host 侧 `extract_value`（busybox sh）抽样验证抽取正确数值。

## 不做（留后续）

- QEMU guest 校准（guest 内核兼容性可能蔓延，本轮只交付 host 产物）。
- 修 `.sh` server 逻辑（tcp_virtio/udp_virtio 缺 server）。
- 补 `init.sh` 环境（ramfs 挂载、test_file 创建、hello 二进制）。
- 修 `tcp_loopback_http_bw.sh` 的 file_list。
- 内核限制修复。

## 容器复核补充（docker 24.04 代理配好后补测）

`.meta` 提取完成后，docker 代理可用，用 ubuntu:24.04 容器（原生 glibc 2.39，非 archive LD hack）补测 host 跑不通的工具：

- **容器跑通，格式确认**：
  - `lat_syscall null`（process_getppid_lat）：输出 `Simple syscall: <N> microseconds`——**不是** `Simple null`。已修正 `process_getppid_lat.meta` 的 SEARCH_PATTERN 为 `Simple syscall`。
  - `lat_connect`（tcp_loopback/virtio_connect_lat）：输出 `TCP/IP connection cost to <host>: <N> microseconds`，与 strings 推断一致，`.meta` 不变。
- **容器仍卡死（无输出，timeout rc=124）**：`lat_fifo`/`bw_unix`/`lat_sem`/`bw_tcp`/`lat_select tcp`/`lat_udp`/`lat_unix_connect`——这些用 lmbench `benchmp` 框架（fork + SIGCHLD + waitpid + 共享内存同步），容器与 host 共享同一内核，`benchmp` 的信号竞争死锁在容器里同样发生。`.meta` 保留 strings 推断的 SEARCH_PATTERN（锚定二进制真实输出 prefix），待 guest 校准。
- **网络工具依赖 libtirpc**：容器原生跑需 `apt install libtirpc3t64`（archive sysroot 自带，host LD 方式不缺；guest DADK 打包会带上依赖）。