# LMbench 性能基准测试套件

## 在 Linux 上运行（POSIX sh）

在仓库根目录执行：

```sh
sh user/apps/tests/benchmark/lmbench/run_linux.sh --samples 3

# 指定另一套 lmbench 二进制，或使用 BusyBox ash 执行 runner 和每个用例：
LMBENCH_BIN_DIR=/path/to/lmbench/bin/x86_64-linux-gnu \
LMBENCH_SH=/usr/bin/busybox \
    sh user/apps/tests/benchmark/lmbench/run_linux.sh --samples 1
```

入口复用 `runner/run.sh` 及全部白名单用例。默认二进制目录为仓库的
`bin/sysroot/usr/lib/lmbench/bin/x86_64-linux-gnu`。Linux 需要 `bwrap`、允许非特权用户
创建 namespace、GNU `timeout`、`findmnt`、`ps` 和常用 POSIX 工具；输出目录必须位于 ext4。
GNU `timeout` 默认使用 `/usr/bin/timeout`，可用 `LMBENCH_TIMEOUT` 指定绝对路径。
入口在每次运行开始时调用所选二进制目录中的 `enough`、`timing_o` 和 `loop_o`
完成一次有界校准，并把三项值及其来源写入 `environment.txt` 后传入隔离环境。
调用者也可显式设置 `ENOUGH`、`TIMING_O`、`LOOP_O`；其中 `ENOUGH` 必须为正整数，
两项 overhead 可为零，显式值会校验、保留并标记为 `explicit`。
入口在私有 PID、网络、IPC 和挂载 namespace 内连续运行整套用例，ext4 用例使用该输出目录
下的专用 fixture，内存文件系统用例使用私有 tmpfs；无需 root，也不会挂载宿主 loop 设备。

所有用例都使用 `LMBENCH_BIN_DIR` 下同一套预编译二进制，运行时不下载源码、打补丁或编译。
DADK 与 Nix 的包来源保持一致，输入哈希随结果保存。

Linux 入口为原版 `lat_fifo` 启用 `runner/fifo_cleanup.sh`：收到 writer 的 EOF 诊断后，
通过 `/proc` 确认它属于本次 benchmark 的子进程，且其两条 FIFO 都已被 unlink，才终止
无法响应 SIGTERM 的 writer。该阶段测量数据已交回 controller；驱动不介入计时循环，
由原 controller 正常输出结果并退出。已确认清理后的重复 EOF 诊断会合并计数；其他输出保留。
仍要求真实退出码为 0 且 latency 有效，超时、测量中失败不会算成功。
该辅助仅由 Linux 入口启用，依赖 Linux `/proc` 和 `readlink`。

原版 `lat_fs` 的零大小用例使用默认大小扫描，并由 metadata 只抽取 `0k` 行；
10k 用例使用 `-s 10k`。四个 wrapper 均设置 `TMPDIR`，确保测量落在指定文件系统。
零大小调用还会运行 1k、4k、10k，但这些行不计入零大小指标。

默认产物写入 `harness/references/lmbench-linux/<UTC时间>-sh-validation/`，可通过
`LMBENCH_LINUX_OUTPUT` 指定新目录；已有目录会被拒绝覆盖。仅保留测试结果：
`results.jsonl`（保留 runner 协议前缀）、每次采样的 `raw/<case>.<轮次>.out` 和 `.rc`、环境、参数及输入哈希。源码快照、构建文件和过程日志使用临时目录，退出时清理。运行结束后会检查存活的 benchmark 进程和 FIFO 文件。
任一用例失败、缺失或有效样本不足时，入口返回非零。单次采样默认超时 120 秒，整套默认
上限 7200 秒（`LMBENCH_SUITE_TIMEOUT`）。

`stat`、信号保护、`fcntl` 和 page-cache 读取使用 11 次 lmbench 内部采样
（上游 `TRIES` 的常用值）。原来的 200～1000 次在 `ENOUGH=1000000` 时会耗时数分钟，
超过默认用例期限。`--samples` 控制外层独立运行次数，与内部 `-N` 分开记录。

Linux 入口将网络客户端指向私有 namespace 的 `127.0.0.1`；保留 `virtio` 名称的用例在这里
验证本地协议路径与脚本行为，结果不能解释为 virtio 网卡性能。

runner 和用例也支持通过 `LMBENCH_BIN_DIR`、`LMBENCH_TMP_DIR`、`LMBENCH_EXT4_DIR` 覆盖路径。
提供现有文件系统目录时设置 `LMBENCH_MANAGE_EXT4=0`，并使用专用测试目录；初始化会写入
fixture。直接运行网络用例时，`LMBENCH_NET_SERVER` 应指向本机地址，默认仍为 guest 的
`10.0.2.15`。

## runner 入口参数

`runner/run.sh` 支持以下参数（无参数 = 读 `config` + `whitelist.txt` 全跑，与 rcS 调用一致）：

```
run.sh [--samples N] [--timeout S] [--warmup N]
       [--whitelist FILE] [--config FILE] [--only NAME] [--list]
```

- `--samples N` / `--timeout S` / `--warmup N`：覆盖 `config` 的对应项。
- `--whitelist FILE` / `--config FILE`：覆盖默认路径（`$SCRIPT_DIR/whitelist.txt` / `$SCRIPT_DIR/config`）。
- `--only NAME`：只跑单个测例（用于手动调试），等价于临时 whitelist 只含 NAME。
- `--list`：列出具有 `test_cases/*.meta` 的可用测例名，不跑。

优先级：命令行参数 > `--config` 指定文件 > 内置默认。

## 添加/启用一个测例

1. 在 `whitelist.txt` 里加上测例名（对应 `test_cases/<name>.sh`）。
2. 确保存在 `test_cases/<name>.sh`（调用某个 lmbench 二进制）。
3. 写 `test_cases/<name>.meta` 描述如何从输出抽取数值：

```
CATEGORY=memory              # memory|process|ipc|vfs|network|signal|filesystem|other
BINARY=bw_mem                # 底层 lmbench 二进制
METRIC_TYPE=bandwidth        # latency|bandwidth|ops|other
UNIT=MB/s
BIGGER_IS_BETTER=1           # 1=越大越好(带宽/吞吐),0=越小越好(延迟)
SEARCH_PATTERN=^[0-9]        # awk 正则,定位结果行
RESULT_INDEX=NF              # awk 字段:数字 | NF | NF-1(取 "microseconds" 前的值)
NTH_OCCURRENCE=1             # 匹配的第几行
DESCRIPTION=Memory copy bandwidth via bw_mem fcp
# SAMPLES=5                  # 可选:覆盖全局采样次数
```

抽取等价于 `awk "/SEARCH_PATTERN/ {print \$(RESULT_INDEX)}"` 取第 `NTH_OCCURRENCE` 个匹配。
延迟类输出多为 `... : VALUE microseconds`，统一用 `RESULT_INDEX=NF-1`；`bw_mem` 输出 `<size> <bw>`，用 `NF`。

## 校准提示

各 lmbench 工具输出格式稳定，`.meta` 的抽取规则起点移植自 Asterinas 已验证配置。
但 `lat_fs`（ext4/ramfs create/delete）等的列布局需在**一次真实运行**后按 `raw_tail` 核对确认
（相关 `.meta` 已标注 `CALIBRATE`）。当前白名单已包含全部 48 个测例。HTTP 带宽单位按 `lat_http` 的实际输出记为 `MB/s`。
