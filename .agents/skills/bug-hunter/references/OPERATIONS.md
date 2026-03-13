# Bug Hunter 运行手册

## 1. 快速开始

### 从 raw findings 开始

```bash
python3 .agents/skills/bug-hunter/scripts/run_pipeline.py \
  --raw-findings artifacts/raw_findings.json \
  --out-dir artifacts
```

### 从 diff 开始

```bash
BASE_REF="$(git rev-parse --abbrev-ref --symbolic-full-name @{upstream} 2>/dev/null || git symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null || echo HEAD~1)"
git diff "$(git merge-base HEAD "$BASE_REF")"...HEAD > /tmp/current.diff
python3 .agents/skills/bug-hunter/scripts/run_pipeline.py \
  --diff-file /tmp/current.diff \
  --raw-findings artifacts/raw_findings.json \
  --out-dir artifacts
```

## 2. 推荐参数

- `--threshold 0.60`：默认通过阈值。
- `--passes 8`：随机化轮次，适合中大型变更。
- `--strict-validation`：严格校验输入契约，发现错误立即失败。
- `--weights`：指定历史学习产出的权重文件。
- `--fail-on-severity critical`：若通过项出现 critical，命令返回非 0（CI 门禁）。
- `--ci-mode`：启用开发友好默认值（严格校验 + critical 门禁）。

## 3. 产物目录约定

`artifacts/` 默认包含：

- `redacted.diff`
- `shuffled_passes.json`
- `buckets.json`
- `debate_candidates.json`
- `verdict.json`
- `bug_hunter_report.md`

## 4. 推荐闭环流程

1. 运行本轮 pipeline，得到报告。
2. 记录采纳/拒绝决策到 `decisions.json`。
3. 运行 `update_resolution_history.py` 产出新权重。
4. 下一轮用 `--weights artifacts/weight_suggestion.json`。
5. 观察 `resolution_rate` 是否上升。

## 5. 开发场景用法

### 本地开发自测（不阻塞）

```bash
python3 .agents/skills/bug-hunter/scripts/run_pipeline.py \
  --raw-findings artifacts/raw_findings.json \
  --out-dir artifacts
```

### 提交前强校验（阻塞 critical）

```bash
python3 .agents/skills/bug-hunter/scripts/run_pipeline.py \
  --raw-findings artifacts/raw_findings.json \
  --ci-mode \
  --out-dir artifacts
```

### CI 渐进收紧（阻塞 major+）

```bash
python3 .agents/skills/bug-hunter/scripts/run_pipeline.py \
  --raw-findings artifacts/raw_findings.json \
  --strict-validation \
  --fail-on-severity major \
  --out-dir artifacts
```

## 6. 并行评审输入要求

外部 Stage2 编排器必须输出统一 Finding 列表：

- 必填：`file,line,type,severity,description,confidence`
- 强烈建议：`agent,fix_code`

缺失 `agent` 会导致降级为默认角色权重。
