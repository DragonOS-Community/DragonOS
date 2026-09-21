# LMbench 有效结果恢复

## 完成结论

计时 ABI 修复、本轮覆盖与验收完成，但 DragonOS 稳定 baseline 尚未成立。

本轮修复 DragonOS 原生 64 位 `timeval` ABI，完善 LMbench FS、runner、Linux 校准透传、结果协议校验与 fixture 清理，并分别完成 DragonOS 和 Linux 验收。代码、DragonOS 数据及 Linux 数据均通过独立复核。Linux 的 dash 与真实 BusyBox sh 各自完整通过 48/48；DragonOS 的 single full attempt 因 panic/OOM 未完成，后续仅用两个独立 VM 补足 dispatch 覆盖，因此归档明确标记 `baseline_eligible=false`。

本计划基于 `origin/master=141bc52f` rebase 后的 `HEAD=cd59d7ba5c9833db60a88f69be89712c6d8152b4` 完成。用户自有 `config/app-blocklist.toml` 与 `dragonos-block-case/` 全程排除在暂存和清理范围之外。

## 实现结果

### timeval ABI

- 将 `PosixSusecondsT` 从 32 位 `c_int` 修正为原生 64 位目标使用的 `c_long`，同步修正 gettimeofday、itimer 与 select copyback 的显式构造点。
- 新增 `user/apps/c_unitest/test_timeval_abi.c`，主要以被污染内存下的 libc/raw gettimeofday 前后对照证明根因，并覆盖 itimer/select 路径。
- rebase 后内核 SHA256 为 `1f3a5a60a3f182f7f6f171da76d2b2c18f439ae25af186b5577c5d7cc0a7ac6b`；内核构建、timeval ABI 回归 1/1 与原生校准 3/3 通过。

### LMbench harness

- `lat_fs` 补丁区分显式 `-s 0k` 并按 `av[optind]` 选择目录；固定来源校验、构建脚本和真实二进制回归已覆盖。
- 四个 FS wrapper 同时固定参数目录与 `TMPDIR`；pagefault fixture 使用 runner 自有临时目录并在退出时清理。
- runner 保留包括 `ENOUGH=1000000` 在内的有效自动校准输出，仅在失败、空值、非法值或零值时有界回退。
- Linux 入口独立校准、验证、记录并穿透 `ENOUGH`、`TIMING_O`、`LOOP_O` 到 `bwrap --clearenv` 后的环境。
- collector 在持久写入前完成 schema 校验，要求完整 frame，并核对 summary 总数与状态计数。
- 两个缩小工作集的元数据、README 与回归测试同步修正。
- 最终 scoped re-review 判定全部阻断项已解决；保存的 dash/BusyBox focused tests、collector 5 项测试与 scoped diff check 均通过。

## Linux 最终验收

正式归档位于 `harness/references/lmbench-linux/20260920-timeval-rebased/`，包含 213 个文件。顶层 `manifest.json` SHA256 为 `0219c10d69530fcf2b115d8d54a040450df3806ee1636aab6a9f45e52bbb3674`。

| shell | 结果 | 本轮原生校准 | 数据校验 | 清理 |
|---|---|---|---|---|
| dash | 48 ok / 0 failed / 0 missing | `ENOUGH=5000`, `TIMING_O=0`, `LOOP_O=0.00000000` | raw 重抽取、统计、schema、176 项输入哈希全部通过 | fixture、FIFO、进程零残留 |
| BusyBox sh | 48 ok / 0 failed / 0 missing | `ENOUGH=1000000`, `TIMING_O=0`, `LOOP_O=0.00000309` | raw 重抽取、统计、schema、176 项输入哈希全部通过 | fixture、FIFO、进程零残留 |

两轮分别执行本轮原生校准，是不同 shell 和不同校准参数下的独立运行，不能合并成同一配置的重复样本。BusyBox 的 `ENOUGH=1000000` 只证明为工具成功输出、经入口验证并透传；本轮没有额外证明其精度已经收敛。

## DragonOS 最终验收

正式归档位于 `harness/references/lmbench-dragonos/20260920-timeval-rebased-coverage/`，包含 45 个文件、53,558 字节。`artifact-sha256.json` SHA256 为 `cec6135535ce0c77b770b8dd90d858327ac02c7636ec13c7799e72e537d77ac0`。

- single full attempt：29 ok、2 failed、1 panic/no-result、16 未执行。该运行不完整，不能作为 baseline。
- 三个独立 VM 合并覆盖：48/48 均 dispatch，39 个 wrapper-valid ok、8 failed、1 panic/no-result。该数字仅表示覆盖，不是单次完整或稳定 baseline。
- 首次 panic 出现在 `signal_prot_lat` 的 RCU context transition；同一 VM 后续在 `ramfs_create_delete_files_0k_ops` 触发 allocator OOM。执行顺序不能证明 OOM 是独立根因。
- 五个 virtio 网络用例当时报告 `Network is unreachable`，但没有 DHCP lease、地址或路由就绪证据，不能直接归因为 DragonOS 内核缺陷。

## 验证边界

- 源码实现与最终修复 scoped review：APPROVED。
- DragonOS 三 VM coverage 数据复核：通过；归档保持 `baseline_eligible=false`。
- Linux 两轮数据 gate：通过；每轮各 48 个唯一 case，48 个 `.out` 和 48 个 `.rc` 完整且 rc 全为 0，canonical summary 与 schema 有效。
- 本轮没有为了收尾重复运行已完成的内核构建、性能测试或 validator；收尾只复核保存的报告、manifest、Git 范围与保护指纹。
- 旧 patch 中为 plain `patch -p1` 精确匹配保留的 tab/context，以及旧 reference CSV 的 CRLF，属于已评审的既有数据格式，不作为本轮新增错误。

## 延后裁决与后续工作

- timeval C 回归中 gettimeofday 提供主要根因证据；itimer/select 对旧 ABI 的独立失败判别较弱，后续可增强逐接口回归。
- collector 已做到校验后写入，但多个输出文件仍不是完全事务写；后续可采用临时文件加 rename，并处理 history 去重。
- 保留 `lat_fifo-cleanup.patch` 的精确补丁上下文和旧 CSV 的 CRLF；若未来统一格式，必须单独验证补丁可应用性与历史数据语义。
- 单独定位 `signal_prot_lat` 的 RCU panic，并在干净 VM 中确认 ramfs0k OOM 是否可独立复现。
- virtio 网络性能项运行前应等待并记录地址、路由和 DHCP 状态；环境未就绪时明确报告前置失败。
- 获得不受 panic/OOM 污染的一次完整 DragonOS 48 项运行后，才能建立稳定 baseline。

## 恢复与证据保留

pre-rebase stash `aae56652648a26f575a47b76617e33a0cb006781` 已 apply 并完成恢复完整性核对；提交前用户 dirty 指纹与准备阶段一致。恢复 ref `refs/codex/recovery/feat-lmbench-pre-rebase-20260920` 保留，以提供额外安全恢复点。

正式 references 只包含精简、已复核的数据。首次 RCU panic、后续 OOM 以及有界 serial、GDB、controller、run、validation 与 focused test 日志保留在忽略的 `result/lmbench-recovery-20260920/`，不复制到 references。重型 qcow2、readback、构建副本、候选副本和 `/tmp/lmbench-recovery-20260920/` 在确认无进程引用后清理；实际清理范围与本地提交 SHA 记录在 ignored closeout receipt 中。
