---
name: bug-hunter-stage3-evidence-fusion
description: bug-hunter 阶段 3 技能。负责对多智能体原始发现做语义去重、桶化聚类与冲突识别，形成可投票的缺陷候选池。
---

# Stage 3 证据融合

## 步骤

1. 读取 `artifacts/raw_findings.json`。
2. 执行 `scripts/semantic_bucket.py` 做语义桶化。
3. 执行 `scripts/debate_picker.py` 标记边界争议桶。
4. 产出：
   - `artifacts/buckets.json`
   - `artifacts/debate_candidates.json`

## 合并策略

- 位置优先：同一 `file` 且行号距离小于等于 3 优先合并。
- 语义次之：描述相似度高于阈值（默认 0.88）合并。
- 同类型弱合并：同类型时仍需达到最低语义阈值（默认 0.35）。
- 不同类型但同位置时标记为冲突候选。
