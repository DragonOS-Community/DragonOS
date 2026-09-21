# LMbench Runner 抽象与目录分层 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在扩到 48 测例前，把 `user/apps/tests/benchmark/lmbench/` 按 `runner/`（guest）、`orchestrator/`（host）、根（用户配置 + 包元信息）分层，给 guest runner 一个参数化入口 `run.sh`，删死代码 `run_test_case.sh`，顺手修 `default.nix` 误装/漏装问题。

**Architecture:** 源码分层是 host 侧组织，guest 运行时仍扁平——DADK/nix install 时把 `runner/` 内文件 + 根 `config`/`whitelist.txt` 摊平到 rootfs `/opt/tests/benchmark/lmbench/`。`run.sh` 加 `parse_args()` 实现 `--samples/--timeout/--warmup/--whitelist/--config/--only/--list`，无参数 = 现状行为。输出 contract、rcS 调用方式、CI 制品路径全不变。

**Tech Stack:** BusyBox `sh`（POSIX，guest）、Python 3（host collector）、GNU make（DADK install）、Nix（可选打包）、JSON Schema（结果校验）。

## Global Constraints

- DADK 硬约束：`Makefile` 必须留包根 `user/apps/tests/benchmark/lmbench/`（`user/dadk/config/all/lmbench_benchmark_tests-1.0.0.toml` 用 `build-from-source` + `source-path` 指向包根 + `build-command = "make install"`）。
- Nix 硬约束：`default.nix` / `flake.nix` / `flake.lock` 必须留包根（`user/apps/default.nix:66` 用 `callPackage ./tests/benchmark/lmbench {}`）。
- `toggle_compile_lmbench.sh` 留包根：其 `../../../../..` 相对路径依赖当前层级。
- guest 运行时布局必须扁平：DADK 与 Nix 两条 install 路径都摊平到 `/opt/tests/benchmark/lmbench/`，`run.sh` 用 `$SCRIPT_DIR/{test_cases,config,whitelist.txt}` 定位。
- `run.sh` 只在 guest 运行时执行；host 单测用 `LMBENCH_RUNNER_NO_MAIN=1` source，不走 main。
- 输出 contract 不变：`===LMBENCH_RUN_BEGIN===` / `LMBENCH_META` / `LMBENCH_JSON` / `LMBENCH_SUMMARY` / `===LMBENCH_RUN_END===` / `benchmark测试完成`。
- 代码注释用英文；commit message 用 `refactor(lmbench):` / `feat(lmbench):` / `docs(lmbench):` 前缀。
- 用户已授权写完计划直接执行，e2e 步骤若需覆盖 `bin/disk-image-x86_64.img` 仍需明确授权（本计划把 e2e 单列为 Task 7，标注需授权）。

---

## File Structure

迁移后的目标结构（spec §目录布局）：

```
user/apps/tests/benchmark/lmbench/
  runner/                      # guest 内运行
    run.sh                     # 参数化入口（原 run_tests.sh）
    init.sh / env.sh / clean_up.sh
    test_cases/
      *.sh / *.meta
      test_tcp_loopback_lat_parser.sh   # 现有回归
      test_args_parser.sh       # 新增：parse_args 回归
  orchestrator/                # host 侧
    monitor_test_results.sh
    collect_results.py
    schema/lmbench-run.schema.json
  config                       # 留根
  whitelist.txt                # 留根
  Makefile                     # 留根，install 改为从 runner/ 拷
  default.nix / flake.nix / flake.lock   # 留根
  toggle_compile_lmbench.sh    # 留根
  README.md / .gitignore / results/
```

各文件职责：
- `runner/run.sh` — guest runner 主入口；内含 `kv_get`/`load_config`/`extract_value`/`compute_stats`/`metric_head`/`run_one_case`/`parse_args`/`run_main`；输出 JSONL contract。
- `runner/init.sh` / `env.sh` / `clean_up.sh` — guest 环境生命周期钩子，被 `run_main` 调起。
- `runner/test_cases/<name>.sh` + `.meta` — 单测例包装 + 抽取规则元数据。
- `runner/test_cases/test_*_parser.sh` — host 侧 `busybox sh` 回归测试，source `run.sh`。
- `orchestrator/monitor_test_results.sh` — host 串口轮询 + 超时判定。
- `orchestrator/collect_results.py` — host 解析串口 → schema 校验 → 持久化。
- `orchestrator/schema/lmbench-run.schema.json` — 结果 schema。
- `Makefile` — DADK install：从 `runner/` 摊平安装到 rootfs。
- `default.nix` / `flake.nix` — Nix 打包。
- `config` / `whitelist.txt` — 用户面向顶层配置。
- `toggle_compile_lmbench.sh` — 构建开关（改 `config/app-blocklist.toml`）。

---

### Task 1: guest 脚本移入 runner/ + 重命名 + 删死代码

**Files:**
- Move: `run_tests.sh` → `runner/run.sh`
- Move: `init.sh` / `env.sh` / `clean_up.sh` → `runner/`
- Move: `test_cases/` → `runner/test_cases/`
- Delete: `run_test_case.sh`
- Modify: `runner/test_cases/test_tcp_loopback_lat_parser.sh`（`RUNNER` 路径）

**Interfaces:**
- Produces: `runner/run.sh`，被回归测试以 `LMBENCH_RUNNER_NO_MAIN=1 . runner/run.sh` source，暴露 `kv_get`/`extract_value`/`run_one_case`（本任务不改这些函数，仅移动 + 重命名）。

- [ ] **Step 1: 用 git mv 移动 guest 脚本到 runner/**

```sh
cd user/apps/tests/benchmark/lmbench
mkdir -p runner
git mv run_tests.sh runner/run.sh
git mv init.sh runner/init.sh
git mv env.sh runner/env.sh
git mv clean_up.sh runner/clean_up.sh
git mv test_cases runner/test_cases
git rm run_test_case.sh
```

- [ ] **Step 2: 修回归测试里的 RUNNER 路径**

编辑 `runner/test_cases/test_tcp_loopback_lat_parser.sh`，把：
```sh
RUNNER="$SCRIPT_DIR/../run_tests.sh"
```
改为：
```sh
RUNNER="$SCRIPT_DIR/../run.sh"
```

- [ ] **Step 3: 跑回归测试验证移动不破坏现有功能**

Run:
```sh
cd user/apps/tests/benchmark/lmbench
busybox sh runner/test_cases/test_tcp_loopback_lat_parser.sh
```
Expected: 输出 `tcp_loopback_lat parser regression: PASS`，退出码 0。

- [ ] **Step 4: 静态确认无残留旧引用**

Run:
```sh
cd user/apps/tests/benchmark/lmbench
grep -rn "run_test_case" . --exclude-dir=results --exclude-dir=__pycache__ || true
grep -rn "run_tests\.sh" . --exclude-dir=results --exclude-dir=__pycache__ || true
```
Expected: `run_test_case` 零命中；`run_tests.sh` 仅在 README.md / Makefile / default.nix / flake.nix（这些在后续 Task 修）。Task 1 范围内文件（runner/）无旧名残留。

- [ ] **Step 5: Commit**

```sh
git add -A user/apps/tests/benchmark/lmbench
git commit -m "refactor(lmbench): move guest scripts into runner/ and drop dead run_test_case.sh"
```

---

### Task 2: run.sh 参数化（TDD）

**Files:**
- Create: `runner/test_cases/test_args_parser.sh`
- Modify: `runner/run.sh`（加 `usage`/`parse_args`/`apply_cli_overrides`，改 `run_main`）

**Interfaces:**
- Consumes: Task 1 产出的 `runner/run.sh`（已含 `load_config`/`run_one_case`）。
- Produces: `runner/run.sh` 新增 `parse_args()`（`return 2` on unknown arg，可被 source 单测捕获）、`apply_cli_overrides()`、`usage()`；`run_main` 开头调 `parse_args "$@" || exit $?`；`--only` 通过写临时单行 whitelist 文件复用 `run_one_case`（不引入新 dispatch 分支）；`--list` 列 `test_cases/*.sh` 名后 `return 0`。

- [ ] **Step 1: 写失败的参数解析回归测试**

创建 `runner/test_cases/test_args_parser.sh`：
```sh
#!/bin/sh
# Regression test for run.sh argument parsing.
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
RUNNER="$SCRIPT_DIR/../run.sh"

LMBENCH_RUNNER_NO_MAIN=1
export LMBENCH_RUNNER_NO_MAIN
. "$RUNNER"

reset_cli() {
    CLI_SAMPLES=""; CLI_TIMEOUT=""; CLI_WARMUP=""
    CLI_WHITELIST=""; CLI_CONFIG=""; ONLY_NAME=""; LIST_ONLY=""
}

# --samples / --only parsed into CLI_* / ONLY_NAME
reset_cli
parse_args --samples 7 --only foo
[ "$CLI_SAMPLES" = "7" ] || { echo "FAIL: CLI_SAMPLES=$CLI_SAMPLES" >&2; exit 1; }
[ "$ONLY_NAME" = "foo" ] || { echo "FAIL: ONLY_NAME=$ONLY_NAME" >&2; exit 1; }

# --timeout / --warmup / --whitelist / --config / --list
reset_cli
parse_args --timeout 30 --warmup 2 --whitelist /tmp/wl --config /tmp/cfg --list
[ "$CLI_TIMEOUT" = "30" ]        || { echo "FAIL: CLI_TIMEOUT" >&2; exit 1; }
[ "$CLI_WARMUP" = "2" ]          || { echo "FAIL: CLI_WARMUP" >&2; exit 1; }
[ "$CLI_WHITELIST" = "/tmp/wl" ] || { echo "FAIL: CLI_WHITELIST" >&2; exit 1; }
[ "$CLI_CONFIG" = "/tmp/cfg" ]   || { echo "FAIL: CLI_CONFIG" >&2; exit 1; }
[ "$LIST_ONLY" = "1" ]           || { echo "FAIL: LIST_ONLY" >&2; exit 1; }

# unknown arg returns 2 (does not exit the sourced shell)
reset_cli
set +e
parse_args --bogus 2>/dev/null
rc=$?
set -e
[ "$rc" = "2" ] || { echo "FAIL: unknown arg rc=$rc" >&2; exit 1; }

echo "args parser regression: PASS"
```

- [ ] **Step 2: 跑测试确认失败**

Run:
```sh
cd user/apps/tests/benchmark/lmbench
busybox sh runner/test_cases/test_args_parser.sh
```
Expected: FAIL（`parse_args: not found` 或类似），因为 `run.sh` 还没定义 `parse_args`。

- [ ] **Step 3: 在 run.sh 实现 usage / parse_args / apply_cli_overrides**

在 `runner/run.sh` 的 `log()` 函数定义之后、`kv_get()` 之前，插入：
```sh
usage() {
    cat >&2 <<'EOF'
usage: run.sh [--samples N] [--timeout S] [--warmup N]
              [--whitelist FILE] [--config FILE] [--only NAME] [--list]
EOF
}

# Parse CLI args into CLI_* intent variables (applied after load_config).
# Returns 2 on unknown arg (caller exits); does not exit the shell itself
# so it can be unit-tested via `LMBENCH_RUNNER_NO_MAIN=1 . run.sh`.
parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --samples)    CLI_SAMPLES=$2;    shift 2 ;;
            --timeout)    CLI_TIMEOUT=$2;    shift 2 ;;
            --warmup)     CLI_WARMUP=$2;     shift 2 ;;
            --whitelist)  CLI_WHITELIST=$2;  shift 2 ;;
            --config)     CLI_CONFIG=$2;     shift 2 ;;
            --only)       ONLY_NAME=$2;      shift 2 ;;
            --list)       LIST_ONLY=1;       shift ;;
            -h|--help)    usage; return 0 ;;
            *)            usage; return 2 ;;
        esac
    done
}

# Apply CLI overrides on top of config-file values.
apply_cli_overrides() {
    [ -n "$CLI_SAMPLES" ]   && SAMPLES=$CLI_SAMPLES
    [ -n "$CLI_TIMEOUT" ]   && TIMEOUT_SEC=$CLI_TIMEOUT
    [ -n "$CLI_WARMUP" ]    && WARMUP=$CLI_WARMUP
    [ -n "$CLI_WHITELIST" ] && WHITELIST_FILE=$CLI_WHITELIST
}
```

- [ ] **Step 4: 改 run_main 支持 --config / --only / --list**

把 `runner/run.sh` 末尾的 `run_main()` 改为：
```sh
run_main() {
    parse_args "$@" || exit $?

    # --config may redirect the config file before load_config reads it.
    [ -n "$CLI_CONFIG" ] && CONFIG_FILE=$CLI_CONFIG
    load_config
    apply_cli_overrides
    mkdir -p "$WORK_TMP"

    if [ "$LIST_ONLY" = "1" ]; then
        ls "$CASES_DIR"/*.sh 2>/dev/null | sed 's|.*/||; s|\.sh$||'
        return 0
    fi

    # --only: replace whitelist with a single-line file, reusing run_one_case
    # unchanged (no second dispatch branch).
    if [ -n "$ONLY_NAME" ]; then
        wl_tmp="$WORK_TMP/only_whitelist"
        printf '%s\n' "$ONLY_NAME" > "$wl_tmp"
        WHITELIST_FILE="$wl_tmp"
    fi

    log "LMbench benchmark run starting"
    echo "===LMBENCH_RUN_BEGIN==="
    printf 'LMBENCH_META {"suite":"lmbench","suite_version":"%s","samples":%s,"timeout_sec":%s,"warmup":%s}\n' \
        "$SUITE_VERSION" "$SAMPLES" "$TIMEOUT_SEC" "$WARMUP"

    if [ -f "$SCRIPT_DIR/init.sh" ]; then
        log "initializing test environment (init.sh)..."
        if ! sh "$SCRIPT_DIR/init.sh"; then
            log "ERROR: test environment initialization failed"
            printf 'LMBENCH_SUMMARY {"total":0,"ok":0,"failed":0,"skipped":0}\n'
            echo "===LMBENCH_RUN_END==="
            echo "benchmark测试完成"
            exit 1
        fi
    fi

    if [ ! -f "$WHITELIST_FILE" ]; then
        log "ERROR: whitelist not found: $WHITELIST_FILE"
        echo "===LMBENCH_RUN_END==="
        echo "benchmark测试完成"
        exit 1
    fi

    total=0; ok=0; failed=0; skipped=0
    while read -r line; do
        case "$line" in ''|\#*) continue ;; esac
        total=$((total + 1))
        log "dispatching $line"
        run_one_case "$line"
        case $? in
            0) ok=$((ok + 1)) ;;
            2) skipped=$((skipped + 1)) ;;
            *) failed=$((failed + 1)) ;;
        esac
        echo "---"
    done < "$WHITELIST_FILE"

    printf 'LMBENCH_SUMMARY {"total":%s,"ok":%s,"failed":%s,"skipped":%s}\n' \
        "$total" "$ok" "$failed" "$skipped"
    echo "===LMBENCH_RUN_END==="

    if [ -f "$SCRIPT_DIR/clean_up.sh" ]; then
        sh "$SCRIPT_DIR/clean_up.sh" >/dev/null 2>&1 || true
    fi

    log "done: total=$total ok=$ok failed=$failed skipped=$skipped"
    echo "benchmark测试完成"
}
```

并在 `run.sh` 顶部全局 config 区（`WARMUP=0` 之后）加 CLI 意图变量初值：
```sh
# CLI intent variables (set by parse_args, applied after load_config).
CLI_SAMPLES=""; CLI_TIMEOUT=""; CLI_WARMUP=""
CLI_WHITELIST=""; CLI_CONFIG=""; ONLY_NAME=""; LIST_ONLY=""
```

- [ ] **Step 5: 跑参数解析测试确认通过**

Run:
```sh
cd user/apps/tests/benchmark/lmbench
busybox sh runner/test_cases/test_args_parser.sh
```
Expected: `args parser regression: PASS`，退出码 0。

- [ ] **Step 6: 跑 parser 回归测试确认未破坏**

Run:
```sh
busybox sh runner/test_cases/test_tcp_loopback_lat_parser.sh
```
Expected: `tcp_loopback_lat parser regression: PASS`。

- [ ] **Step 7: Commit**

```sh
git add runner/run.sh runner/test_cases/test_args_parser.sh
git commit -m "feat(lmbench): parameterize runner entrypoint with parse_args"
```

---

### Task 3: host 编排移入 orchestrator/

**Files:**
- Move: `monitor_test_results.sh` / `collect_results.py` → `orchestrator/`
- Move: `schema/` → `orchestrator/schema/`
- Modify: `orchestrator/collect_results.py`（`--outdir` 默认 + docstring）
- Modify: `Makefile:325,327`（根 Makefile，加 `orchestrator/` 前缀）

**Interfaces:**
- Produces: `orchestrator/collect_results.py` 的 `--outdir` 默认指向 `lmbench/results/`（即 `HERE/../results`）；`--schema` 默认 `HERE/schema/...` 不变（schema 跟随移动）。

- [ ] **Step 1: 移动 host 编排文件**

```sh
cd user/apps/tests/benchmark/lmbench
mkdir -p orchestrator
git mv monitor_test_results.sh orchestrator/monitor_test_results.sh
git mv collect_results.py orchestrator/collect_results.py
git mv schema orchestrator/schema
```

- [ ] **Step 2: 改 collect_results.py 的 outdir 默认值与 docstring**

编辑 `orchestrator/collect_results.py`：

第 4 行 docstring `The guest runner (run_tests.sh) prints` → `The guest runner (run.sh) prints`。

第 269 行：
```python
    ap.add_argument("--outdir", default=os.path.join(HERE, "results"))
```
改为：
```python
    ap.add_argument("--outdir", default=os.path.join(HERE, "..", "results"))
```
（`--schema` 默认 `os.path.join(HERE, "schema", "lmbench-run.schema.json")` 不用改——schema 已跟随移到 `orchestrator/schema/`。）

- [ ] **Step 3: 改根 Makefile 的 monitor/collect 路径**

编辑 `Makefile` 第 325、327 行：
```makefile
		bash user/apps/tests/benchmark/lmbench/monitor_test_results.sh || status=$$?; \
		if [ $$status -eq 0 ]; then \
			python3 user/apps/tests/benchmark/lmbench/collect_results.py || status=$$?; \
```
改为：
```makefile
		bash user/apps/tests/benchmark/lmbench/orchestrator/monitor_test_results.sh || status=$$?; \
		if [ $$status -eq 0 ]; then \
			python3 user/apps/tests/benchmark/lmbench/orchestrator/collect_results.py || status=$$?; \
```
（第 312、315 行的 `toggle_compile_lmbench.sh` 路径不变——toggle 留根。）

- [ ] **Step 4: 语法检查 collect_results.py**

Run:
```sh
cd user/apps/tests/benchmark/lmbench
python3 -c "import ast; ast.parse(open('orchestrator/collect_results.py').read()); print('syntax ok')"
```
Expected: `syntax ok`。

- [ ] **Step 5: 静态确认无残留旧 host 路径**

Run:
```sh
grep -rn "lmbench/monitor_test_results\|lmbench/collect_results\|lmbench/schema" Makefile .github/ \
  || true
```
Expected: 零命中（已全部改为 `orchestrator/` 前缀）。

- [ ] **Step 6: Commit**

```sh
git add -A user/apps/tests/benchmark/lmbench Makefile
git commit -m "refactor(lmbench): move host orchestration into orchestrator/"
```

---

### Task 4: 打包文件更新（Makefile / default.nix / flake.nix）

**Files:**
- Modify: `user/apps/tests/benchmark/lmbench/Makefile`（install 从 `runner/` 拷，删 `run_test_case.sh`）
- Modify: `user/apps/tests/benchmark/lmbench/default.nix`（sourceByRegex + installPhase + 补装 config/whitelist + run.sh）
- Modify: `user/apps/tests/benchmark/lmbench/flake.nix:41`（`run_tests.sh` → `run.sh`）

**Interfaces:**
- Produces: DADK 与 Nix 两条 install 路径都摊平到 `/opt/tests/benchmark/lmbench/`，产物含 `run.sh`/`init.sh`/`env.sh`/`clean_up.sh`/`test_cases/`/`config`/`whitelist.txt`（扁平）。

- [ ] **Step 1: 改 lmbench/Makefile 的 install 目标**

把 `user/apps/tests/benchmark/lmbench/Makefile` 第 11-30 行：
```makefile
# Files needed inside the guest at runtime.
GUEST_SCRIPTS = run_tests.sh init.sh env.sh clean_up.sh run_test_case.sh
GUEST_DATA    = whitelist.txt config
```
改为：
```makefile
# Files needed inside the guest at runtime. Sources live under runner/;
# `install` flattens them into INSTALL_DIR (guest runtime layout is flat).
RUNNER_DIR    = runner
GUEST_SCRIPTS = $(RUNNER_DIR)/run.sh $(RUNNER_DIR)/init.sh $(RUNNER_DIR)/env.sh $(RUNNER_DIR)/clean_up.sh
GUEST_DATA    = whitelist.txt config
```

并把 install 目标：
```makefile
install:
	@echo "[lmbench] installing to $(INSTALL_DIR)"
	@mkdir -p $(INSTALL_DIR)
	@install -m755 $(GUEST_SCRIPTS) $(INSTALL_DIR)/
	@install -m644 $(GUEST_DATA) $(INSTALL_DIR)/
	@cp -r test_cases $(INSTALL_DIR)/
	@chmod +x $(INSTALL_DIR)/test_cases/*.sh 2>/dev/null || true
	@echo "[lmbench] install done"
```
改为：
```makefile
install:
	@echo "[lmbench] installing to $(INSTALL_DIR)"
	@mkdir -p $(INSTALL_DIR)
	@install -m755 $(GUEST_SCRIPTS) $(INSTALL_DIR)/
	@install -m644 $(GUEST_DATA) $(INSTALL_DIR)/
	@cp -r $(RUNNER_DIR)/test_cases $(INSTALL_DIR)/
	@chmod +x $(INSTALL_DIR)/test_cases/*.sh 2>/dev/null || true
	@echo "[lmbench] install done"
```

- [ ] **Step 2: 改 default.nix 的 sourceByRegex + installPhase**

把 `default.nix` 第 17-28 行：
```nix
    src = lib.sourceByRegex ./. [
      "^test_cases"
      "^.*\.sh$"
    ];

    installPhase = ''
      mkdir -p $out/${installDir}

      install -m755 *.sh $out/${installDir}/
      cp -r test_cases $out/${installDir}/
      chmod +x $out/${installDir}/test_cases/*.sh
    '';
```
改为：
```nix
    src = lib.sourceByRegex ./. [
      "^runner"
      "^runner/.*\.sh$"
      "^runner/test_cases"
      "^runner/test_cases/.*"
      "^config$"
      "^whitelist\.txt$"
    ];

    installPhase = ''
      mkdir -p $out/${installDir}

      install -m755 runner/*.sh $out/${installDir}/
      install -m644 config whitelist.txt $out/${installDir}/
      cp -r runner/test_cases $out/${installDir}/
      chmod +x $out/${installDir}/test_cases/*.sh
    '';
```
（顺手修两个既有问题：原 `^.*\.sh$` 会误装 host 脚本进 nix 输出；原 installPhase 漏装 `config`/`whitelist.txt`。）

- [ ] **Step 3: 改 flake.nix 的 program 路径**

把 `flake.nix` 第 41 行：
```nix
        program = "${lmbench}/${defaultInstallDir}/run_tests.sh";
```
改为：
```nix
        program = "${lmbench}/${defaultInstallDir}/run.sh";
```

- [ ] **Step 4: 验证 DADK Makefile install 摊平布局**

Run:
```sh
cd user/apps/tests/benchmark/lmbench
rm -rf /tmp/lmbench-install-test
make install INSTALL_DIR=/tmp/lmbench-install-test
ls /tmp/lmbench-install-test
ls /tmp/lmbench-install-test/test_cases | head
```
Expected: `/tmp/lmbench-install-test` 下扁平出现 `run.sh` `init.sh` `env.sh` `clean_up.sh` `config` `whitelist.txt` `test_cases/`；`test_cases/` 下有各 `.sh`/`.meta`。**不应**出现 `run_tests.sh`、`run_test_case.sh`、`runner/` 子目录。

- [ ] **Step 5: 清理测试产物**

```sh
rm -rf /tmp/lmbench-install-test user/apps/tests/benchmark/lmbench/install
```

- [ ] **Step 6: Commit**

```sh
git add user/apps/tests/benchmark/lmbench/Makefile user/apps/tests/benchmark/lmbench/default.nix user/apps/tests/benchmark/lmbench/flake.nix
git commit -m "build(lmbench): install from runner/ and fix nix sourceByRegex"
```

---

### Task 5: 外部调用点 + 文档

**Files:**
- Modify: `user/sysconfig/etc/init.d/rcS:52`（`run_tests.sh` → `run.sh`）
- Modify: `user/apps/tests/benchmark/lmbench/README.md`（目录结构 + 路径 + 参数说明 + 删 `run_test_case.sh`）
- Modify: `orchestrator/collect_results.py` docstring（已在 Task 3 改，此处仅核对）
- Modify: `user/apps/tests/benchmark/lmbench/Makefile` 注释（`run_tests.sh` → `run.sh` 若有）

**Interfaces:**
- Produces: rcS 调 `sh $BENCHMARK_TEST_DIR/run.sh`；README 反映新布局与 `run.sh` 参数面。

- [ ] **Step 1: 改 rcS 的 runner 入口名**

编辑 `user/sysconfig/etc/init.d/rcS` 第 52 行：
```sh
    /bin/busybox sh $BENCHMARK_TEST_DIR/run_tests.sh
```
改为：
```sh
    /bin/busybox sh $BENCHMARK_TEST_DIR/run.sh
```

- [ ] **Step 2: 更新 README.md 目录结构图**

把 `README.md` 的 `## 目录结构` 代码块替换为：
```
```
runner/                        guest 端 runner（AUTO_TEST=benchmark 入口）
  run.sh                       参数化入口(原 run_tests.sh)
  init.sh / env.sh / clean_up.sh   测试环境准备/变量/清理
  test_cases/<name>.sh        测例包装脚本(调用 lmbench 二进制)
  test_cases/<name>.meta      该测例的抽取规则 + 单位 + 方向元数据
  test_cases/test_*_parser.sh host 侧回归测试(busybox sh,source run.sh)
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
```

- [ ] **Step 3: 更新 README.md 快速开始里的脚本名**

把 `## 快速开始` 区域里所有 `run_tests.sh` 引用改为 `run.sh`（如架构图里的 `run_tests.sh` → `run.sh`）。

- [ ] **Step 4: 在 README.md 添加 run.sh 参数说明**

在 `## 添加/启用一个测例` 之前插入新章节：
```markdown
## runner 入口参数

`runner/run.sh` 支持以下参数（无参数 = 读 `config` + `whitelist.txt` 全跑，与 rcS 调用一致）：

```
run.sh [--samples N] [--timeout S] [--warmup N]
       [--whitelist FILE] [--config FILE] [--only NAME] [--list]
```

- `--samples N` / `--timeout S` / `--warmup N`：覆盖 `config` 的对应项。
- `--whitelist FILE` / `--config FILE`：覆盖默认路径（`$SCRIPT_DIR/whitelist.txt` / `$SCRIPT_DIR/config`）。
- `--only NAME`：只跑单个测例（用于手动调试），等价于临时 whitelist 只含 NAME。
- `--list`：列出 `test_cases/*.sh` 可用测例名，不跑。

优先级：命令行参数 > `--config` 指定文件 > 内置默认。
```

- [ ] **Step 5: 从 README 删除 run_test_case.sh 条目**

`README.md` 原 `## 目录结构` 里有 `run_test_case.sh          手动单测例运行(init→run→cleanup)` 行——已在 Step 2 整块替换时移除。确认 README 全文无 `run_test_case` 残留：
```sh
grep -n "run_test_case" user/apps/tests/benchmark/lmbench/README.md || true
```
Expected: 零命中。

- [ ] **Step 6: 全仓静态扫描门禁**

Run:
```sh
cd /home/yuming/project_ongoing/DragonOSDev/DragonOS/.worktrees/feat-lmbench
grep -rn "run_tests\.sh\|run_test_case" . \
  --exclude-dir=.git --exclude-dir=results --exclude-dir=__pycache__ --exclude-dir=target \
  --exclude='*.json' || true
```
Expected: 命中只在 `docs/superpowers/specs/` 与 `docs/superpowers/plans/`（设计/计划文档引用旧名是历史记录，可接受）以及 `results/` 之外无源码/配置/CI 命中。若有源码命中，回到对应 Task 修复。

- [ ] **Step 7: Commit**

```sh
git add user/sysconfig/etc/init.d/rcS user/apps/tests/benchmark/lmbench/README.md
git commit -m "docs(lmbench): update rcS entrypoint name and README for new layout"
```

---

### Task 6: 构建冒烟验证

**Files:** 无（仅验证）

- [ ] **Step 1: 跑两个 host 回归测试**

Run:
```sh
cd user/apps/tests/benchmark/lmbench
busybox sh runner/test_cases/test_tcp_loopback_lat_parser.sh
busybox sh runner/test_cases/test_args_parser.sh
```
Expected: 两个都 PASS，退出码 0。

- [ ] **Step 2: DADK 构建冒烟（不覆盖 disk image）**

Run:
```sh
cd /home/yuming/project_ongoing/DragonOSDev/DragonOS/.worktrees/feat-lmbench
make all -j$(nproc)
```
Expected: 构建成功。验证 sysroot 下扁平布局：
```sh
find bin -path '*opt/tests/benchmark/lmbench*' -maxdepth 8 2>/dev/null | head -20
```
Expected: 出现 `.../opt/tests/benchmark/lmbench/run.sh`、`.../init.sh`、`.../test_cases/`、`.../config`、`.../whitelist.txt`，且**无** `run_tests.sh`、`run_test_case.sh`、`runner/` 子目录、`monitor_test_results.sh`、`collect_results.py`（后两者不装进 rootfs）。

- [ ] **Step 3: 记录验证结果到 commit（可选）**

若一切通过，无需额外 commit（本任务是验证）。若发现缺失，回到对应 Task 修复后再跑。

---

### Task 7: 端到端验证（需用户授权覆盖 disk image）

**Files:** 无（仅验证）

**授权要求：** 本任务执行 `make test-benchmark`，会覆盖 `bin/disk-image-x86_64.img`。需用户明确授权该确切路径。用户已表示"请求的话，我就通过"——执行前向用户确认一次即可。

- [ ] **Step 1: 请求用户授权覆盖 bin/disk-image-x86_64.img**

向用户确认："`make test-benchmark` 会覆盖 `bin/disk-image-x86_64.img`，是否授权执行？"得到肯定答复后继续。

- [ ] **Step 2: 跑 make test-benchmark**

Run:
```sh
cd /home/yuming/project_ongoing/DragonOSDev/DragonOS/.worktrees/feat-lmbench
make test-benchmark
```
Expected: 8 个指标全部 `ok`，`LMBENCH_SUMMARY {"total":8,"ok":8,"failed":0,"skipped":0}`，`[collect] schema validation: PASS`，退出码 0。

- [ ] **Step 3: 确认产物落盘**

Run:
```sh
ls -t user/apps/tests/benchmark/lmbench/results/x86_64/*.json | head -1
tail -n 8 user/apps/tests/benchmark/lmbench/results/history.jsonl
```
Expected: 新 run 的 canonical snapshot 已写入；history.jsonl 末尾有 8 条新指标行。

- [ ] **Step 4: 确认 blocklist 已恢复**

Run:
```sh
grep -n "lmbench benchmark tests" config/app-blocklist.toml
```
Expected: 该行存在且未被注释（`make test-benchmark` 的 trap cleanup 已 disable）。

---

## Self-Review

**1. Spec coverage：**
- spec §目标1（分层）→ Task 1（runner/）+ Task 3（orchestrator/）+ Task 5（rcS/README）。
- spec §目标2（参数化入口）→ Task 2。
- spec §目标3（修 default.nix 误装/漏装）→ Task 4 Step 2。
- spec §目标4（外部行为零变化）→ Task 6 + Task 7 验证；Task 3/4 保持 contract/调用方式/CI 路径不变。
- spec §硬约束（Makefile/nix/toggle 留根）→ Task 1 只移动 guest 脚本，Task 3 只移动 host 编排，Makefile/nix/toggle 未移动。
- spec §guest 运行时扁平 → Task 4 Step 4 验证摊平布局。
- spec §删除 run_test_case.sh → Task 1 Step 1 `git rm` + Task 5 Step 5 确认 README 无残留。
- spec §验证（静态扫描/回归/构建冒烟/e2e）→ Task 5 Step 6 + Task 6 + Task 7。
- spec §风险缓解（--only 不走新分支）→ Task 2 Step 4 用临时单行 whitelist 文件复用 run_one_case。
- spec §不做（YAGNI）→ 计划未包含拆 lib / host 参数化 / CI 门控 / 扩 48 测例。✓

**2. Placeholder scan：** 无 TBD/TODO；每个 step 有具体命令或代码块。✓

**3. Type consistency：** `parse_args` 在 Task 2 定义、Task 5/6 不再调用；`CLI_SAMPLES`/`ONLY_NAME`/`LIST_ONLY` 在 Task 2 定义并被 test_args_parser.sh 断言；`runner/run.sh` 路径在 Task 1 产出，Task 2/4/5 引用一致；`orchestrator/` 路径在 Task 3 产出，根 Makefile 引用一致。✓

## Execution Handoff

用户已授权写完计划直接执行，不请求审批。采用 Inline Execution（任务间路径耦合强、机械迁移为主，单 context 持续跟踪已改路径比 fresh subagent 更可靠），直接按 Task 1→7 顺序执行。Task 7 执行前需就 `bin/disk-image-x86_64.img` 覆盖向用户确认一次（用户已预告会通过）。