---
name: bug-hunter-stage4-consensus-judge
description: bug-hunter 阶段 4 技能。负责对缺陷桶执行加权共识投票，筛选过阈值问题，并输出裁决级结构化评审报告。
---

# Stage 4 共识裁决

## 步骤

1. 读取 `artifacts/buckets.json`。
2. 根据 persona 权重运行 `scripts/weighted_vote.py --threshold 0.6`。
   - 默认从 `references/persona_matrix.json` 读取权重。
   - 可用 `--weights` 叠加历史学习权重（兼容扁平映射与 `suggested_weights` 包装格式）。
3. 运行 `scripts/render_report.py` 生成 Markdown 报告。
4. 输出：
   - `artifacts/verdict.json`
   - `artifacts/bug_hunter_report.md`

## 裁决规则

- 仅保留加权分数超过阈值的问题。
- 对没有 `fix_code` 的条目额外惩罚 10%。
- 严重级别排序：`critical > major > minor`。
