# LMbench 性能基准测试套件

DragonOS 的性能基准评测套件，基于用户态移植的 [LMbench](http://lmbench.sourceforge.net/) 二进制，
参考隔壁 Asterinas 的 benchmark runner 设计。它自动启动 QEMU、在 guest 内跑白名单测例、抽取性能数值、
计算统计量，并在 host 侧把结果按统一 schema 持久化。

设计上对齐现有的 gVisor syscall 测试套件（`user/apps/tests/syscall/gvisor`）：host 侧 monitor 轮询串口、
guest 侧 runner 由 `AUTO_TEST` 触发。

## 快速开始

```bash
# 在仓库根目录:构建 DragonOS → 启动 QEMU → 跑基准 → 收集结果
make test-benchmark
```

该目标会：
1. `toggle_compile_lmbench.sh enable` 临时把基准套件编进 rootfs（默认屏蔽以免拖慢常规构建）；
2. `make all` + `write_diskimage`；
3. 后台启动 `qemu-nographic AUTO_TEST=benchmark`（串口 → `serial_opt.txt`）；
4. `monitor_test_results.sh` 轮询串口，判定启动/超时/完成；
5. `collect_results.py` 解析串口、补 run 级元数据、校验 schema、落盘到 `results/`；
6. 恢复 blocklist（disable）。

结果位于 `results/<arch>/<timestamp>-<commit>.json`。

当前白名单包含 48 个测例；每轮以实际结果中的成功、失败和跳过数判断覆盖情况。GNU `timeout` 调用 `timer_create(CLOCK_REALTIME)` 的内核兼容性问题由 [#2159](https://github.com/DragonOS-Community/DragonOS/issues/2159) 独立跟踪；它不属于 LMbench 结果解析修复。

## DragonOS guest 环境前置条件

`LMBENCH_NET_SERVER` 默认值 `10.0.2.15` 只是网络用例的连接目标，不会为 guest 配置地址或路由。运行网络用例前，guest 必须实际拥有所选地址及可用路由；网卡名应从该 guest 的 `ip link` 输出确定，不能硬编码为 `eth0`。以下检查均在 **guest 内**执行，并应在测试前通过：

```sh
net_server=${LMBENCH_NET_SERVER:-10.0.2.15}
ip -4 addr show
ip -4 route show
ip -4 route get "$net_server"
getent hosts localhost
```

若需要静态配置，只能在私有 guest 镜像或一次性 overlay 中，由调用者明确选择接口和地址；不得修改宿主网络。`tcp_loopback_select_lat` 使用的 `lat_select tcp` 依赖 libc 将 `localhost` 解析为 `127.0.0.1`。制作私有 guest 镜像时应保留已有 `/etc/hosts` 内容，并确定性地提供该映射；不得复制或修改宿主 `/etc/hosts`。guest 缺少 `getent` 时，应使用调用 guest libc 的等价解析探针验证，而不能只检查文件内容。

五个 `*_virtio_*` wrapper 会在同一 guest 内启动 server 和 client。默认目标 `.15` 是 guest 本机地址时，结果只覆盖本机非 loopback 地址上的 TCP/UDP 协议路径，不代表 guest 与 host 或外部端点之间的 virtio 数据传输性能。

`runner/run.sh` 可直接使用 `--only NAME` 或 `--whitelist FILE` 做定向调试；`make test-benchmark` 经 `rcS` 无参数调用 runner，不会转发这些命令行参数。需要筛选时可在私有 guest 中直接调用 runner，或安装私有入口和 whitelist，无需修改通用 `rcS`。

结果归档应随测试数据简要保存 guest 地址/路由/解析现场、校准值、各层超时，以及 kernel、suite 安装树和私有 whitelist 的哈希；若将精简结果纳入 references，过程日志与镜像仍留在 references 之外。完整验收须保留正式 whitelist 的全部 48 个测例及原参数。若为减少已知 panic 对后续用例的污染而在私有 whitelist 中将 `signal_prot_lat` 移到最后，应记录实际顺序并验证测例集合不变；该用例仍计为失败，不能据此宣称问题已修复或基线稳定。

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

## 架构

```
make test-benchmark (host)
      │  build + write disk + qemu-nographic AUTO_TEST=benchmark &
      │  monitor_test_results.sh  (轮询 serial_opt.txt,超时/完成判定)
      │  collect_results.py       (解析 → 校验 schema → 持久化)
      ▼
guest: /etc/init.d/rcS  →  $BENCHMARK_TEST_DIR/run.sh
      · init.sh 建 ext4 loop fs + 测试文件(一次)
      · 读 whitelist.txt + config,逐测例跑 N 次
      · 按 test_cases/<name>.meta 的规则从输出抽取数值
      · 算 mean/median/stddev/min/max/cv (busybox awk,自实现 sqrt)
      · 逐条打印 "LMBENCH_JSON {...}" (JSONL) 到串口
      · 末尾打印 "benchmark测试完成"
```

## 目录结构

```
run_linux.sh                   Linux 隔离运行入口(POSIX sh)
tests/                         runner/fixture 回归检查
runner/                        共用 runner(AUTO_TEST=benchmark 入口)
  run.sh                       参数化入口(原 run_tests.sh)
  init.sh / env.sh / clean_up.sh   测试环境准备/变量/清理
  test_cases/<name>.sh        测例包装脚本(调用 lmbench 二进制)
  test_cases/<name>.meta      该测例的抽取规则 + 单位 + 方向元数据
  test_cases/test_*.sh        host 侧解析及包装脚本回归测试
orchestrator/                  host 端编排
  monitor_test_results.sh      串口监控 + 超时/完成判定
  collect_results.py           解析串口 → 校验 → 持久化
  schema/lmbench-run.schema.json   结果 JSON Schema(draft-07)
config                         全局配置(SAMPLES / TIMEOUT_SEC / WARMUP)
whitelist.txt                  要运行的测例清单(每行一个)
toggle_compile_lmbench.sh      切换是否把本套件编进 rootfs
Makefile                       DADK build-from-source 的安装脚本
default.nix / flake.nix        Nix 打包(可选)
results/                       运行产物(gitignore;作为 CI 制品归档)
  <arch>/<ts>-<commit>.json    每次运行的完整快照(canonical)
  history.jsonl                每条指标一行(时序友好)
  github-benchmark/data.json   github-action-benchmark 兼容格式(为可视化预留)
```

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

## 结果 Schema

见 [orchestrator/schema/lmbench-run.schema.json](orchestrator/schema/lmbench-run.schema.json)。每次运行一个 JSON 文档：run 级元数据（run_id/时间/git/arch/host/config）
+ `metrics[]`（每指标含 `samples[]` 原始样本与 `stats{mean,median,stddev,min,max,cv}`）+ `summary`。
指标 `status` 为 `ok|failed|skipped`；失败时带 `error` 与 `raw_tail`（便于按真实输出校准抽取规则）。

## 校准提示

各 lmbench 工具输出格式稳定，`.meta` 的抽取规则起点移植自 Asterinas 已验证配置。
但 `lat_fs`（ext4/ramfs create/delete）等的列布局需在**一次真实运行**后按 `raw_tail` 核对确认
（相关 `.meta` 已标注 `CALIBRATE`）。当前白名单已包含全部 48 个测例。HTTP 带宽单位按 `lat_http` 的实际输出记为 `MB/s`。
