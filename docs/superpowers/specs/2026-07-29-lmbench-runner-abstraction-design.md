# LMbench Runner 抽象与目录分层设计

- 日期：2026-07-29
- 分支：`feat/lmbench-suite`
- 状态：设计已用户确认，待 review

## Context

`make test-benchmark` 已达成 MVP 8/8 端到端验证。在扩到全部 48 个测例之前，`user/apps/tests/benchmark/lmbench/` 目录里的脚本角色混杂：runner 组成部分、host 编排、打包脚本混放同一层，且存在死代码 `run_test_case.sh`。本设计在扩展前先清边界：给 guest runner 一个参数化入口，按职责分层目录，底层实现暂不拆 lib。

## 目标

1. 把脚本按 `runner/`（guest 内运行）、`orchestrator/`（host 编排）、根（用户配置 + 包元信息）分层，让"加测例/调 host/改打包"三条工作路径互不干扰。
2. 给 guest runner 一个参数化 CLI 入口，屏蔽底层实现；新增测例或调采样只跟接口打交道。
3. 顺手修 `default.nix` 误装 host 脚本、漏装 config/whitelist 的既有问题。
4. 不改输出 contract、不改 rcS 调用方式、不改 CI 制品路径——`make test-benchmark` 外部行为零变化。

## 硬约束（查证所得）

- `Makefile` 必须留包根：DADK `lmbench_benchmark_tests-1.0.0.toml` 用 `build-from-source` + `source-path = "user/apps/tests/benchmark/lmbench"` + `build-command = "make install"`，在包根找 Makefile。
- `default.nix` / `flake.nix` / `flake.lock` 必须留包根：`user/apps/default.nix:66` 用 `callPackage ./tests/benchmark/lmbench {}` 找 default.nix。
- `toggle_compile_lmbench.sh` 留包根：其 `../../../../..` 相对路径依赖当前层级。

## runner 定义

**窄定义**：runner = 仅 guest 内运行部分（`run.sh` + init/env/clean_up + test_cases）。host 侧 `monitor_test_results.sh` / `collect_results.py` 是 orchestrator（协作者，不是 runner），因为它们的"运行"对象是 QEMU 进程 + 串口流，不是测例；且 host(py+sh) 与 guest(busybox sh) 无共享实现层，强行归一个"runner"会再次模糊边界。

## 目录布局

```
lmbench/
  runner/                      # guest 内运行（DADK/nix install 时摊平到 rootfs）
    run.sh                     # 参数化入口（原 run_tests.sh，重命名）
    init.sh / env.sh / clean_up.sh
    test_cases/
      <name>.sh / <name>.meta
      test_tcp_loopback_lat_parser.sh   # 回归测试随迁
  orchestrator/                # host 侧编排
    monitor_test_results.sh
    collect_results.py
    schema/lmbench-run.schema.json
  config                       # 留根：用户调参（SAMPLES/TIMEOUT_SEC/WARMUP）
  whitelist.txt                # 留根：用户选测例
  Makefile                     # 留根：install 改为从 runner/ 拷
  default.nix / flake.nix / flake.lock   # 留根：sourceByRegex 改为只抓 runner/
  toggle_compile_lmbench.sh    # 留根（构建开关，路径不动）
  README.md / .gitignore / results/
```

分层逻辑：`runner/` = 纯执行机制 + test_cases（runner 直接 dispatch 的对象，且 48×2 文件放根会淹没根目录）；根 = 用户面向配置（config/whitelist）+ 包元信息（Makefile/nix/toggle/README）。

### guest 运行时布局：扁平

Makefile/nix install 时把 `runner/` 内文件 + 根的 `config`/`whitelist.txt` 全摊平到 rootfs `/opt/tests/benchmark/lmbench/`，`runner/test_cases/` → `/opt/tests/benchmark/lmbench/test_cases/`。`orchestrator/` 与 `schema/` 不装进 rootfs（只 host 用）。

源码分层是 host 侧组织，guest 运行时仍扁平。`run.sh` 用 `$SCRIPT_DIR/{test_cases,config,whitelist.txt}` 定位——运行时同目录，天然兼容；`init.sh` 的 `$SCRIPT_DIR/env.sh`、`$SCRIPT_DIR/ext4.img` 同理。

注：`run.sh` 只在 guest 运行时执行，源码树里不直接跑（无 lmbench 二进制与 guest 环境）。源码层面 `$SCRIPT_DIR/config` 指向 `runner/config`（不存在）的"错位"无实际影响；host 单测用 `LMBENCH_RUNNER_NO_MAIN=1` source 不走 main。

## 参数化接口

`run.sh` 的 CLI 面（busybox sh POSIX 兼容，手写 `while [ $# -gt 0 ]; do case` 解析，不用 getopt）：

```
run.sh [--samples N] [--timeout S] [--warmup N]
       [--whitelist FILE] [--config FILE] [--only NAME] [--list]
```

| 参数 | 语义 | 覆盖 |
|---|---|---|
| `--samples N` | 每测例采样次数 | config `SAMPLES` |
| `--timeout S` | 单测例墙钟超时秒 | config `TIMEOUT_SEC` |
| `--warmup N` | 丢弃的预热轮数 | config `WARMUP` |
| `--whitelist FILE` | 测例清单路径 | `$SCRIPT_DIR/whitelist.txt` |
| `--config FILE` | 配置文件路径 | `$SCRIPT_DIR/config` |
| `--only NAME` | 只跑单个测例（取代 `run_test_case.sh`） | 临时 whitelist 只含 NAME |
| `--list` | 列出 `test_cases/*.sh` 可用测例名，不跑 | — |

**优先级**：命令行参数 > `--config` 指定文件 > 内置默认（`SAMPLES=5` 等）。实现：先 `load_config()`，再让命令行覆盖。

**向后兼容**：无参数 = 现状行为（读 `config` + `whitelist.txt` 全跑）。rcS 仅文件名 `run_tests.sh` → `run.sh`，调用方式不变。

**`--only NAME` 实现 constraint**：仅作用于 whitelist 读取环节（构造单行清单），下游 `run_one_case` 不改——不引入第二条 dispatch 分支。NAME 无对应 `.sh` 时走现有 skipped 路径，不 abort 整轮。

**`--list` 实现**：`ls test_cases/*.sh` 去后缀去路径打印。

**错误处理**：未知参数 → stderr 打 usage、exit 2。

**输出 contract 不变**：`===LMBENCH_RUN_BEGIN===` / `LMBENCH_META` / `LMBENCH_JSON` / `LMBENCH_SUMMARY` / `===LMBENCH_RUN_END===` / `benchmark测试完成` 全部保持。host `collect_results.py` / `monitor_test_results.sh` 零改动。

## 迁移影响面

### A. Guest 侧（runner/）

| 文件 | 改动 |
|---|---|
| `run_tests.sh` → `runner/run.sh` | 重命名 + 加 `parse_args()`；`LMBENCH_RUNNER_NO_MAIN` 保留；内部 `$SCRIPT_DIR/{test_cases,config,whitelist.txt}` 运行时扁平仍对 |
| `init.sh`/`env.sh`/`clean_up.sh` → `runner/` | 移位，内容不动 |
| `test_cases/` → `runner/test_cases/` | 整目录移；`test_tcp_loopback_lat_parser.sh` 内 `RUNNER="$SCRIPT_DIR/../run_tests.sh"` → `../run.sh` |
| `run_test_case.sh` | 删除（死代码：在 Makefile GUEST_SCRIPTS 里被装进 rootfs，但 guest 内无人调；host 上跑不通因无 lmbench 二进制） |

### B. 根目录配置（留根）

| 文件 | 改动 |
|---|---|
| `config` / `whitelist.txt` | 留根，内容不动；Makefile/nix install 时摊平进 rootfs |

### C. Host 编排（orchestrator/）

| 文件 | 改动 |
|---|---|
| `monitor_test_results.sh` → `orchestrator/` | 移位，内容不动（全用 env var，无内部相对路径） |
| `collect_results.py` → `orchestrator/` | `--schema` 默认 `HERE/schema/...`（schema 跟移，仍对）；`--outdir` 默认 `HERE/../results`（results 留根）；docstring `run_tests.sh` → `run.sh` |
| `schema/` → `orchestrator/schema/` | 跟 collect 移 |

### D. 打包 / 构建（留根）

| 文件 | 改动 |
|---|---|
| `Makefile` | `GUEST_SCRIPTS` 从 `runner/` 取（`runner/run.sh` 等，`install` 自动取 basename 摊平）；删 `run_test_case.sh`；`GUEST_DATA = whitelist.txt config`；`cp -r runner/test_cases` |
| `default.nix` | `sourceByRegex` 改为只抓 `runner/` 下 `.sh` + `runner/test_cases`；installPhase 从 `runner/` 拷 + 补装 `config`/`whitelist.txt`（当前漏装，顺手修）；`run_tests.sh` → `run.sh` |
| `flake.nix:41` | `program` 路径 `run_tests.sh` → `run.sh` |
| `toggle_compile_lmbench.sh` | 留根不动 |

### E. 外部调用点

| 文件 | 改动 |
|---|---|
| `user/sysconfig/etc/init.d/rcS:52` | `run_tests.sh` → `run.sh` |
| 根 `Makefile:325/327` | `monitor_test_results.sh`/`collect_results.py` 路径加 `orchestrator/` 前缀；toggle 路径不变 |
| `.github/workflows/benchmark.yml` | `results/**` 路径不变（results 留根）；无内部脚本引用 |

### F. 文档

| 文件 | 改动 |
|---|---|
| `README.md` | 目录结构图 + 所有脚本路径引用同步新布局；新增 `run.sh` 参数说明；删 `run_test_case.sh` 条目 |
| `Makefile` 注释 / `collect_results.py` docstring | `run_tests.sh` → `run.sh` |

## 验证

1. **静态引用扫描**（门禁，先于 e2e）：迁移完成后 `grep -rn "run_tests.sh\|run_test_case\.sh"` 全仓应为零命中（除 spec/commit message）；`grep -rn "monitor_test_results.sh\|collect_results.py"` 确认只剩 `orchestrator/` 前缀引用。

2. **host 侧回归测试**（`busybox sh`，不依赖 guest）：
   - `test_tcp_loopback_lat_parser.sh`（现有，随迁）：source `run.sh`，断言 `extract_value` 返回 `4745.8345`。
   - `test_args_parser.sh`（新增）：source `run.sh`，断言 `--samples 7 --only foo` 解析后 `SAMPLES=7` 且 whitelist 等效单行 `foo`；断言未知参数 exit 2。

3. **构建冒烟**：`make all` 确认 DADK `make install` 从 `runner/` 摊平安装不报错，sysroot 下 `/opt/tests/benchmark/lmbench/` 出现扁平 `run.sh`/`init.sh`/`test_cases/`/`config`/`whitelist.txt`。

4. **端到端**（需用户授权覆盖 `bin/disk-image-x86_64.img`）：`make test-benchmark` 跑通 8/8，schema 校验通过，确认 rcS→`run.sh`、orchestrator 路径、摊平布局全链路工作。

## 风险与缓解

| 风险 | 缓解 |
|---|---|
| DADK/nix 两条安装路径摊平布局不一致，guest 运行时分叉 | spec 明确两条 install 都摊平到 `/opt/tests/benchmark/lmbench/` 扁平；构建冒烟验证 DADK 输出；nix 输出由 `default.nix` installPhase 对齐 |
| `--only` 悄悄引入第二条 dispatch 分支 | 实现 constraint：`--only NAME` 仅作用于 whitelist 读取环节，下游 `run_one_case` 不改 |
| busybox sh 参数解析兼容性 | 手写 `while/case`，不用 getopt/getopts；回归测试覆盖 |
| 源码树 `run.sh` 的 `$SCRIPT_DIR/config` 指向 `runner/config`（不存在）造成困惑 | spec 注明 run.sh 只在 guest 运行时执行；host 单测用 `LMBENCH_RUNNER_NO_MAIN=1` source |
| 迁移漏改旧路径导致 e2e 失败 | 静态 grep 扫描作为门禁，先于 e2e |

## 不做（YAGNI）

- 不拆 `run.sh` 内部 lib（`kv_get`/`extract_value`/`compute_stats` 等留在单文件）。
- 不参数化 host 侧（`monitor`/`collect` 已被 Makefile 串好，收益小）。
- 不加 CI 回归门控/可视化。
- 不扩 48 测例——本设计是扩展前的边界清理，扩展是后续独立轮次。