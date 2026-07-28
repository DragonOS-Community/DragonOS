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

## 架构

```
make test-benchmark (host)
      │  build + write disk + qemu-nographic AUTO_TEST=benchmark &
      │  monitor_test_results.sh  (轮询 serial_opt.txt,超时/完成判定)
      │  collect_results.py       (解析 → 校验 schema → 持久化)
      ▼
guest: /etc/init.d/rcS  →  $BENCHMARK_TEST_DIR/run_tests.sh
      · init.sh 建 ext4 loop fs + 测试文件(一次)
      · 读 whitelist.txt + config,逐测例跑 N 次
      · 按 test_cases/<name>.meta 的规则从输出抽取数值
      · 算 mean/median/stddev/min/max/cv (busybox awk,自实现 sqrt)
      · 逐条打印 "LMBENCH_JSON {...}" (JSONL) 到串口
      · 末尾打印 "benchmark测试完成"
```

## 目录结构

```
run_tests.sh              guest 端 runner(AUTO_TEST=benchmark 入口)
monitor_test_results.sh   host 端串口监控 + 超时/完成判定
collect_results.py        host 端解析串口 → 校验 → 持久化
init.sh / env.sh / clean_up.sh   测试环境准备/变量/清理
run_test_case.sh          手动单测例运行(init→run→cleanup)
whitelist.txt             要运行的测例清单(每行一个)
config                    全局配置(SAMPLES / TIMEOUT_SEC / WARMUP)
schema/lmbench-run.schema.json   结果 JSON Schema(draft-07)
test_cases/<name>.sh      测例包装脚本(调用 lmbench 二进制)
test_cases/<name>.meta    该测例的抽取规则 + 单位 + 方向元数据
toggle_compile_lmbench.sh 切换是否把本套件编进 rootfs
Makefile                  DADK build-from-source 的安装脚本
results/                  运行产物(gitignore;作为 CI 制品归档)
  <arch>/<ts>-<commit>.json    每次运行的完整快照(canonical)
  history.jsonl                每条指标一行(时序友好)
  github-benchmark/data.json   github-action-benchmark 兼容格式(为可视化预留)
```

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

见 `schema/lmbench-run.schema.json`。每次运行一个 JSON 文档：run 级元数据（run_id/时间/git/arch/host/config）
+ `metrics[]`（每指标含 `samples[]` 原始样本与 `stats{mean,median,stddev,min,max,cv}`）+ `summary`。
指标 `status` 为 `ok|failed|skipped`；失败时带 `error` 与 `raw_tail`（便于按真实输出校准抽取规则）。

## 校准提示

各 lmbench 工具输出格式稳定，`.meta` 的抽取规则起点移植自 Asterinas 已验证配置。
但 `lat_fs`（ext4/ramfs create/delete）等的列布局需在**一次真实运行**后按 `raw_tail` 核对确认
（相关 `.meta` 已标注 `CALIBRATE`）。第一版白名单只纳入代表性子集，闭环稳定后再一次性扩到全部 48 个测例。
