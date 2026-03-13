# Bug Hunter 输出格式规范

本文件定义 bug-hunter 的标准输出结构，目标是让不同评审轮次的报告可比较、可追溯、可自动处理。

## 1. 最终报告模板

`scripts/render_report.py` 生成的 Markdown 报告建议包含以下结构：

```markdown
## Bug Hunter Report

- Threshold: `0.60`
- Accepted findings: `N`
- Rejected findings: `M`
- Disputed findings: `K`

| 缺陷编号 | 位置 | 类型 | 严重级别 | 描述 | 建议修复 | 共识强度 |
|---|---|---|---|---|---|---|
| BUG-001 | kernel/src/foo.rs:42 | concurrency | major | ... | ... | 7.1/10 |

## Developer TODO

- [ ] `BUG-001` `major` `kernel/src/foo.rs:42` owner=`Concurrency Engineer`: ... | 修复建议: ...

## Disputed Findings

| 缺陷编号 | 位置 | 争议原因 | 分数 |
|---|---|---|---|
| BUG-013 | ... | borderline_score | 0.55 |

## Rejected Findings

| 缺陷编号 | 位置 | 类型 | 严重级别 | 分数 |
|---|---|---|---|---|
```

## 2. 字段解释

- `缺陷编号`：`BUG-XXX`，来自 `buckets.json`。
- `位置`：`file:line`，必须可回溯。
- `类型`：`security | concurrency | performance | logic`。
- `严重级别`：`critical | major | minor`。
- `描述`：优先使用最高置信度证据的描述，单行呈现。
- `建议修复`：若缺失显示 `(需要补充修复建议)`。
- `共识强度`：`score * 10`，范围 `[0, 10]`。

## 3. 排序规则

1. 先按严重级别：`critical > major > minor`。
2. 同级别按分数降序。
3. Rejected 区也按分数降序输出。

## 4. 争议项规则

争议项来自 `debate_candidates.json`，包括：

- `type_conflict = true`
- 或 `score` 落在辩论区间 `[low, high)`

争议项不直接并入 accepted 表，需人工二次裁决。

## 5. TODO 区规则

- `Developer TODO` 只列 accepted 项。
- `owner` 默认取该桶内最高置信度 finding 的 `agent`。
- 修复建议为空时显示“补充可执行修复代码”。

## 6. 文本质量要求

- 每个问题都必须有 `file:line`。
- 不输出纯格式、命名风格问题。
- 不使用模糊结论（例如“可能有问题”）作为最终结论。
- 表格字段尽量单行，超长描述写到附加说明。
