# Bug Hunter 示例

## 示例 1：从 diff 到最终报告（推荐）

### 输入

- 基线：优先使用当前分支 upstream；无 upstream 时回退 `origin/HEAD`，再回退 `HEAD~1`
- 当前分支包含多个 Rust 文件改动

### 步骤

1) 生成 diff

```bash
BASE_REF="$(git rev-parse --abbrev-ref --symbolic-full-name @{upstream} 2>/dev/null || git symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null || echo HEAD~1)"
git diff "$(git merge-base HEAD "$BASE_REF")"...HEAD > /tmp/current.diff
```

2) Stage1 随机化输入

```bash
python3 .agents/skills/bug-hunter/scripts/redact_sensitive.py \
  /tmp/current.diff \
  -o artifacts/redacted.diff

python3 .agents/skills/bug-hunter/scripts/shuffle_diff.py \
  artifacts/redacted.diff \
  --passes 8 \
  -o artifacts/shuffled_passes.json
```

说明：`artifacts/shuffled_passes.json` 交给外部 Stage2 编排器。编排器应为每个 persona 随机抽取 1 个 `passes[*].diff`，并把 8 个 agent 的输出汇总为 `artifacts/raw_findings.json`。

3) 并行评审（外部编排器）后写入 `raw_findings.json`

最小编排要求：

- 8 个 agent 并行启动
- 每个 agent persona 固定
- 每个 agent 从 `shuffled_passes.json` 随机选取 1 个 pass
- 每个 agent 只返回 JSON findings
- 编排器统一汇总为 `raw_findings.json`

4) 运行 Stage3/4 报告链路

```bash
python3 .agents/skills/bug-hunter/scripts/run_pipeline.py \
  --raw-findings artifacts/raw_findings.json \
  --out-dir artifacts \
  --threshold 0.60
```

### 输出

- `artifacts/redacted.diff`
- `artifacts/shuffled_passes.json`
- `artifacts/raw_findings.validated.json`
- `artifacts/buckets.json`
- `artifacts/debate_candidates.json`
- `artifacts/verdict.json`
- `artifacts/bug_hunter_report.md`

## 示例 2：最小 raw findings 样例

`artifacts/raw_findings.json`：

```json
{
  "schema_version": "1.0",
  "findings": [
    {
      "file": "kernel/src/process/exit.rs",
      "line": 42,
      "type": "logic",
      "severity": "major",
      "description": "wait4 path forgets to propagate rusage error code",
      "fix_code": "return_errno!(Errno::ECHILD);",
      "confidence": 0.78,
      "agent": "Diverse Reviewer C",
      "pass_id": 4
    }
  ]
}
```

可直接复用仓库样例文件：

- `.agents/skills/bug-hunter/references/examples/raw_findings.sample.json`
- `.agents/skills/bug-hunter/references/examples/decisions.sample.json`

## 示例 3：闭环学习输入

`artifacts/decisions.json`：

```json
[
  {
    "agent": "Security Sentinel",
    "status": "accepted",
    "bucket_id": "BUG-001"
  },
  {
    "agent": "Diverse Reviewer B",
    "status": "rejected",
    "bucket_id": "BUG-007",
    "reason": "not reproducible"
  }
]
```

执行：

```bash
python3 .agents/skills/bug-hunter/scripts/update_resolution_history.py \
  artifacts/decisions.json \
  --output artifacts/review_history.json \
  --weights-output artifacts/weight_suggestion.json
```

输出：

- `artifacts/review_history.json`
- `artifacts/weight_suggestion.json`

## 示例 4：复用上一轮权重

```bash
python3 .agents/skills/bug-hunter/scripts/weighted_vote.py \
  artifacts/buckets.json \
  --weights artifacts/weight_suggestion.json \
  --threshold 0.60 \
  -o artifacts/verdict.json
```

兼容说明：`--weights` 同时支持扁平映射和 `{"suggested_weights": {...}}` 格式。
