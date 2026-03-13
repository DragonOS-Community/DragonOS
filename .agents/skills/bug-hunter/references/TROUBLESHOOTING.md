# Bug Hunter 常见问题排查

## 1. `raw findings not found`

现象：`run_pipeline.py` 直接报找不到文件。

排查：

- 确认 `--raw-findings` 路径存在。
- 先运行外部 Stage2，生成 `raw_findings.json`。

## 2. 校验失败：缺字段或类型错误

现象：`validate_findings.py` 报错 `missing required field` 或 `confidence out of range`。

排查：

- 对照 `references/CONTRACTS.md` 修正字段。
- 确认 `line` 为整数且 `>=1`。
- 确认 `confidence` 在 `[0,1]`。

## 3. `weighted_vote.py` 权重文件格式不兼容

现象：分数异常或所有角色权重都变成默认值。

排查：

- 优先使用 `{"suggested_weights": {...}}`。
- 或使用扁平映射 `{"Security Sentinel": 5.0, ...}`。
- 检查角色名是否与 `persona_matrix.json` 一致。

## 4. 报告为空（Accepted findings = 0）

现象：`bug_hunter_report.md` 里无通过项。

排查：

- 降低 `--threshold`（如从 `0.60` 调到 `0.55`）。
- 确认 Stage2 输出提供了合理 `confidence`。
- 检查 `fix_code` 是否长期缺失（会被惩罚）。

## 5. 聚类过度，多个问题被并成一个桶

现象：`buckets.json` 数量显著少于原始 findings。

排查：

- 适当调高 `--sim-threshold`。
- 缩小 `--line-window`。
- 检查描述文本是否过于模板化（导致高相似度）。

## 6. 安全隔离执行不规范

现象：中间产物混写、路径污染。

排查：

- 每次运行使用独立 `--out-dir`。
- 确保 Stage1 已执行脱敏。
- 禁止把产物输出到生产路径。
