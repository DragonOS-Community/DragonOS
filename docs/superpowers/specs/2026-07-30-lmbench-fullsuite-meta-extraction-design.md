# LMbench 全量测例 .meta 提取设计

- 日期：2026-07-30
- 分支：`feat/lmbench`
- 状态：设计已用户确认，待 review

## Context

`runner/test_cases/` 下有 48 个测例 `.sh`，但仅 8 个有 `.meta` 抽取规则——这 8 个是 MVP whitelist 已跑通的。剩余 40 个无 `.meta`，`run.sh` 对它们走默认抽取（`PAT='^[0-9]'`）会 failed，且 `raw_tail` 仅 300 字节尾部不足以可靠看清输出格式。在扩全量 whitelist 前必须为这 40 个补 `.meta`。

`.meta` 抽取规则的核心是 lmbench 二进制的**输出格式**，而输出格式由二进制自身决定、不依赖 guest 内核。因此可在 host 侧（Docker ubuntu:24.04 容器，提供 lmbench 24.04 二进制所需的 glibc 2.39）跑 lmbench 工具看完整输出、提取 `.meta`，不必启 QEMU、不必覆盖 disk image、不必授权。

## 目标

1. 为 48 个测例补全 `.meta`（含已有 8 个复核）。
2. 产出探查报告：每测例的 lmbench 工具、容器输出样本、`.meta` 规则、guest 校准状态。
3. `whitelist.txt` 保留全量 48 个。
4. 不改任何 guest 生产代码（`env.sh`/`.sh`/`init.sh`）；不做 QEMU 验证。

## 硬约束

- host glibc 2.35（Ubuntu 22.04），跑不了 24.04 glibc 2.39 编译的 lmbench 二进制 → 必须用 Docker ubuntu:24.04 容器（已确认 docker 29.6.2 可用）。
- lmbench 二进制来源：用 guest 同款 archive（toml 里 `lmbench-ubuntu2404-...tar.xz`），保证 `.meta` 抽取规则和 guest 完全一致。
- 一次性探查工作，手操即可，不为它改 guest 文件。
- QEMU 验证不纳入本轮：guest 内核兼容性问题可能蔓延，导致核心任务"获取 .meta"延期。

## 流程

1. 下载 `lmbench-ubuntu2404-...tar.xz`（URL 见 `user/dadk/config/all/lmbench_bin_ubuntu2404.toml` 的 `source-path`），解压到 `/tmp/lmbench-survey/`（含 `lib/lmbench/bin/x86_64-linux-gnu/*` + `lib/x86_64-linux-gnu/*` glibc 2.39）。
2. `docker run --rm -v /tmp/lmbench-survey:/srv ubuntu:24.04 bash`，容器内直接跑 lmbench 二进制（路径 `/srv/lib/lmbench/bin/x86_64-linux-gnu/<tool>`，参数参考对应 `.sh` 里的调用），捕获完整 stdout/stderr。
3. server+client 类（`tcp_loopback_*`/`unix_*`/`tcp_virtio_*`/`udp_*` 等）手操起 server 后台 + 跑 client，看输出。
4. 据输出写 `runner/test_cases/<name>.meta`。
5. `env.sh`/`.sh`/`init.sh` 一律不动。`.sh` 的完整性（如 `tcp_virtio_lat.sh` 只 client 没 server）是后续 guest 修复问题，不影响 host 看输出格式提取 `.meta`。

## 容器等价环境分组

48 个测例按 lmbench 工具的环境依赖分 4 组，容器里用等价环境跑（只看输出格式，数值不重要）：

| 组 | 测例 | 容器跑法 |
|---|---|---|
| 无依赖 | `lat_pipe`/`lat_fifo`/`lat_unix`/`bw_unix`/`lat_proc *`/`lat_sig *`/`lat_syscall *`/`bw_mem *`/`lat_mem`/`lat_ctx`/`lat_pagefault`/`bw_mmap_rd`(mmap_only) | 直接跑，容器内 cwd |
| 需文件/目录 | `bw_file_rd *`(pagecache)/`bw_mmap_rd`(file)/`lat_fs *`(ext4/ramfs create+delete) | 容器内 `dd` 建 dummy 文件 / `mkdir` 建目录；ext4/ramfs 用容器普通目录/tmpfs 替代（格式不依赖 fs 类型） |
| 需 server+client | `lat_tcp`/`bw_tcp *`/`lat_connect`(tcp)/`unix_connect_lat` | 容器内 127.0.0.1：起 server 后台 `lat_tcp -s &`，再跑 client `lat_tcp 127.0.0.1` |
| virtio 特定 | `tcp_virtio_*`/`udp_virtio_*` | 容器用 127.0.0.1 替代 `10.0.2.15`（格式相同，IP 只影响数值）；`.sh` 里只 client 没 server 的，容器手操补 server 看输出 |

## 输出→`.meta` 映射规则

三类格式（基于已有 8 个 `.meta` 验证）：

- **延迟类**（`lat_pipe`/`lat_sig`/`lat_syscall`/`lat_tcp`/`lat_proc`/`lat_unix`/`lat_fifo`/`lat_sem`/`lat_connect`/`lat_ctx`/`lat_pagefault`/`lat_select`）：输出 `<描述>: <N> microseconds` → `SEARCH_PATTERN=<锚定词>`, `RESULT_INDEX=NF-1`（"microseconds"前的值）, `BIGGER_IS_BETTER=0`, `UNIT=microseconds`, `METRIC_TYPE=latency`。
- **带宽类**（`bw_mem`/`bw_unix`/`bw_tcp`/`bw_mmap_rd`/`bw_file_rd`）：输出 `<size> <bw>` 或 `... <N> MB/sec` → `SEARCH_PATTERN=^[0-9]` 或锚定词, `RESULT_INDEX=NF`（最后字段=带宽）, `BIGGER_IS_BETTER=1`, `UNIT=MB/s`, `METRIC_TYPE=bandwidth`。
- **ops/表类**（`lat_fs`）：多 size 表 → `SEARCH_PATTERN=^<size>`, `RESULT_INDEX=<列号>`, `BIGGER_IS_BETTER=1`, `UNIT=ops/sec`, `METRIC_TYPE=ops`。

每个 `.meta` 含 `CATEGORY`/`BINARY`/`METRIC_TYPE`/`UNIT`/`BIGGER_IS_BETTER`/`SEARCH_PATTERN`/`RESULT_INDEX`/`NTH_OCCURRENCE`/`DESCRIPTION`。

## banner 规避

`.sh` 打的 `=== Running XXX test ===` banner 不是结果行。`SEARCH_PATTERN` 必须锚定 lmbench 二进制实际输出的特征词（`Pipe latency`/`Signal handler`/`open/close` 这种二进制才会打的描述），避免误匹配 banner。

教训：`tcp_loopback_lat.meta` 原 `SEARCH_PATTERN=TCP latency` 会先撞 banner `=== Running TCP latency test ===`，`RESULT_INDEX=NF-1` 取了 `test` 而非数字；修成 `^TCP latency using` 才跑通。延迟类尤其注意；带宽类 `^[0-9]` 因 banner 不以数字开头，天然安全。

## whitelist

`whitelist.txt` 保留全量 48 个测例（本轮补全了 `.meta`，不再恢复 8 个）。`.meta` 是 host 提取、未在 guest 校准，正式 `make test-benchmark` 跑全量可能仍有部分 failed（guest 输出与 host 差异，或 guest 内核不支持某些 syscall）——这是有价值的校准信号，不视为问题。

## 报告

落到 `docs/superpowers/specs/2026-07-30-lmbench-fullsuite-survey.md`，每测例一条：
- lmbench 工具 + 容器跑法（哪组等价环境）
- 容器实际输出样本（一行）
- `.meta` 抽取规则（SEARCH_PATTERN/RESULT_INDEX）
- guest 校准状态：**已校准**（原 8 个）/ **待 guest 校准**（host 提取，40 个）/ **guest 可能不支持**（基于工具用的 syscall 推断，如 `lat_connect`/`lat_select` 等可能依赖内核未实现特性，留后续 QEMU 验证确认）

## 不做（YAGNI / 留后续）

- 不做 QEMU 验证（guest 内核兼容性问题可能蔓延，导致 .meta 提取延期）。
- 不改 guest 生产代码（`env.sh`/`.sh`/`init.sh`）。
- 不修内核限制、不补 `.sh` server 逻辑、不扩 `init.sh` 环境——这些留后续 guest 校准轮。
- 不要求 48 个在 guest 全部正确出数——本轮只交付 host 产物（48 `.meta` + 报告）。

## 验证

- 48 个 `.meta` 齐全（`ls runner/test_cases/*.meta | wc -l` = 48）。
- 报告每测例含容器输出样本 + 抽取规则。
- `whitelist.txt` 含 48 行有效测例名。
- 不改 `env.sh`/`.sh`/`init.sh`（`git diff` 无 guest 文件改动）。