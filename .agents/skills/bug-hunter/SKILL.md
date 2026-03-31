---
name: bug-hunter
description: 分布式多智能体缺陷检测总控技能。基于输入随机化、角色化并行评审、语义桶化、加权共识与裁决复核输出高信噪比代码评审报告。用于大规模 PR、复杂逻辑变更、安全敏感改动或单智能体评审召回率不足的场景。
---

# Bug Hunter 总控

## 目标

构建一个可复用的多阶段代码评审流水线：

1. 随机化输入，缓解位置偏差。
2. 并行化子智能体评审，提升召回率。
3. 聚合与去重，压缩重复告警。
4. 通过加权投票和裁决复核，降低误报。
5. 记录分辨率反馈，动态优化后续轮次。

## 目录结构

- `subskills/bug-hunter-stage1-input-randomization/SKILL.md`
- `subskills/bug-hunter-stage2-parallel-review/SKILL.md`
- `subskills/bug-hunter-stage3-evidence-fusion/SKILL.md`
- `subskills/bug-hunter-stage4-consensus-judge/SKILL.md`
- `scripts/shuffle_diff.py`
- `scripts/redact_sensitive.py`
- `scripts/semantic_bucket.py`
- `scripts/weighted_vote.py`
- `scripts/debate_picker.py`
- `scripts/render_report.py`
- `scripts/update_resolution_history.py`
- `scripts/run_pipeline.py`
- `scripts/validate_findings.py`

说明：Stage 5 闭环学习和 Stage 6 运行隔离目前由 `scripts/update_resolution_history.py`、`references/OPERATIONS.md` 与外部编排约束承载，尚未拆分为独立 subskill 文件。

## 参考文档

- `references/FORMAT.md`：最终报告格式与排序规则。
- `references/EXAMPLES.md`：从 diff 到闭环学习的示例。
- `references/CONTRACTS.md`：所有中间产物数据契约。
- `references/OPERATIONS.md`：命令化运行手册。
- `references/TROUBLESHOOTING.md`：常见失败排查。
- `references/METRICS.md`：质量指标与阈值调优。
- `references/finding_schema.json`：Finding 对象 schema。
- `references/persona_matrix.json`：角色矩阵与默认权重。

## 执行顺序

1. **Stage 1 输入处理**：提取 diff，脱敏，按文件/块级生成 N 轮随机输入。
2. **Stage 2 并行评审**：⚠️ **必须使用 Agent 工具并行启动 8 个子智能体；每个子智能体从 `shuffled_passes.json` 随机抽取 1 个 pass，并按固定 persona 分工输出 findings，禁止手工编写 findings！**
3. **Stage 3 证据融合**：将 JSON 发现项做语义去重与冲突标记。
4. **Stage 4 共识裁决**：按权重计算共识分，筛选过阈值问题并格式化输出。
5. **Stage 5 闭环学习**：记录建议被接受/拒绝情况，更新人格权重参考。
6. **Stage 6 安全隔离**：确保评审执行在只读工作树与脱敏上下文中完成。

## 开发工作流映射

把 6 个阶段映射到日常开发动作：

1. 本地改代码后，对 `main...HEAD` 生成 diff 并做 Stage1 脱敏随机化。
2. Stage2 外部并行评审产出 `raw_findings.json`。
3. 运行 `run_pipeline.py` 生成 `report/verdict/debate`。
4. 开发者先修复 Accepted，再处理 Disputed，最后复跑验证。
5. 提交 MR/PR 时开启 `--ci-mode` 或 `--fail-on-severity critical` 做阻塞门禁。
6. 合并后把采纳结果写入 `decisions.json`，更新权重进入下一轮。

## 标准输入输出

- 子智能体输出统一 JSON schema：

```json
[
  {
    "file": "kernel/src/foo.rs",
    "line": 42,
    "type": "security|concurrency|performance|logic",
    "severity": "critical|major|minor",
    "description": "问题描述",
    "fix_code": "建议修复代码",
    "confidence": 0.0,
    "agent": "Security Sentinel"
  }
]
```

兼容说明：`raw_findings.json` 允许两种格式：

- 直接数组 `[{...}]`
- 包装对象 `{"schema_version":"1.0","findings":[...]}`

- 总控最终输出包含：
  - `shuffled_passes.json`（仅当提供 `--diff-file` 时生成）
  - `raw_findings.validated.json`
  - 通过阈值的问题表格（按严重度降序）
  - 共识强度与证据数
  - 边界争议项（需人工复核）

Stage 2 输入规则：

- 外部编排器必须消费 `shuffled_passes.json`
- 每个 agent 随机选取 1 个 `passes[*].diff` 作为输入
- 每个 agent 使用固定 persona，只关注该 persona 负责的问题类型
- 汇总结果统一写入 `raw_findings.json`

## 快速执行

当已有子智能体原始发现时，可直接运行：

```bash
python3 .agents/skills/bug-hunter/scripts/run_pipeline.py \
  --raw-findings artifacts/raw_findings.json \
  --strict-validation \
  --fail-on-severity critical \
  --out-dir artifacts
```

当需要从 diff 开始，可先生成随机化输入：

```bash
BASE_REF="$(git rev-parse --abbrev-ref --symbolic-full-name @{upstream} 2>/dev/null || git symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null || echo HEAD~1)"
git diff "$(git merge-base HEAD "$BASE_REF")"...HEAD > /tmp/current.diff
python3 .agents/skills/bug-hunter/scripts/run_pipeline.py \
  --diff-file /tmp/current.diff \
  --raw-findings artifacts/raw_findings.json \
  --weights artifacts/weight_suggestion.json \
  --ci-mode \
  --out-dir artifacts
```

说明：Stage 2 并行子智能体评审在当前环境中由外部编排器负责，`run_pipeline.py` 负责 Stage 1/3/4 的可复用自动化与产物落盘。

## 阈值建议

- 默认投票阈值：`0.60`
- 语义合并阈值：`0.88`
- 同类型最低合并相似度：`0.35`
- 辩论触发区间：`[0.50, 0.60)`

## 规则

- ⚠️ **Stage 2 必须使用 Agent 工具并行启动 8 个子智能体，禁止手工编写 findings！**
- 不允许跳过 Stage 3 和 Stage 4。
- 无 `fix_code` 的发现项默认降权。
- 不报告纯格式问题或命名偏好。
- 结论必须可回溯到 `file:line`。

## 优化方向（贴近开发）

1. 门禁策略分层：先只阻塞 `critical`，稳定后逐步提升到 `major`。
2. 报告可执行化：Developer TODO 区必须有位置、责任角色、修复代码。
3. 噪声治理：对高误报 persona 下调权重，按周观察 resolution/误报趋势。
4. 评审成本控制：小改动降低 `--passes`，大改动提升 `--passes` 与辩论比例。
5. 结果复用：把 `weight_suggestion.json` 纳入仓库 CI 缓存，减少冷启动抖动。
