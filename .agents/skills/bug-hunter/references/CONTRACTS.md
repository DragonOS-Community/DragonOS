# Bug Hunter 数据契约

## 1. 版本策略

- 所有核心产物建议携带 `schema_version`，当前版本为 `1.0`。
- 为兼容历史输入，缺失 `schema_version` 视为 `1.0`。

## 2. Finding（基础对象）

字段定义：

- `file` `string` 必填
- `line` `int>=1` 必填
- `type` `security|concurrency|performance|logic` 必填
- `severity` `critical|major|minor` 必填
- `description` `string` 必填
- `fix_code` `string` 建议填
- `confidence` `float[0,1]` 必填
- `agent` `string` 强烈建议填（用于加权）

参考 Schema：`finding_schema.json`。

## 3. raw_findings.json

兼容两种输入：

### A. 列表格式（历史）

```json
[
  {"file": "...", "line": 1, "type": "logic", "severity": "major", "description": "...", "fix_code": "...", "confidence": 0.8, "agent": "..."}
]
```

### B. 包装对象格式（推荐）

```json
{
  "schema_version": "1.0",
  "findings": [
    {"file": "...", "line": 1, "type": "logic", "severity": "major", "description": "...", "fix_code": "...", "confidence": 0.8, "agent": "..."}
  ]
}
```

推荐扩展字段：

- `pass_id` `int>=1` 可选
  表示该 finding 来自 `shuffled_passes.json` 的哪一轮输入，便于复盘 Stage 2 抽样行为。后续脚本应忽略未识别字段，因此该字段不会影响 Stage 3/4。

## 4. buckets.json

`semantic_bucket.py` 输出：

```json
{
  "schema_version": "1.0",
  "buckets": [
    {
      "bucket_id": "BUG-001",
      "file": "kernel/src/foo.rs",
      "line": 42,
      "primary_type": "logic",
      "type_conflict": false,
      "types": ["logic"],
      "severities": ["major"],
      "evidence_count": 3,
      "findings": []
    }
  ]
}
```

## 5. shuffled_passes.json

`shuffle_diff.py` 输出：

```json
{
  "schema_version": "1.0",
  "strategy": "deterministic_file_and_hunk_shuffle",
  "original_block_count": 2,
  "passes": [
    {
      "pass_id": 1,
      "seed": 123456789,
      "file_order": ["kernel/src/foo.rs", "kernel/src/bar.rs"],
      "block_count": 2,
      "diff": "diff --git a/..."
    }
  ]
}
```

字段约束：

- `strategy`：当前固定为 `deterministic_file_and_hunk_shuffle`
- `original_block_count`：原始 diff 中按文件切分的 block 数量
- `passes[*].pass_id`：从 `1` 开始递增
- `passes[*].seed`：该轮派生 seed，便于复现
- `passes[*].file_order`：该轮文件 block 顺序
- `passes[*].block_count`：该轮 block 数量
- `passes[*].diff`：重排后的完整 unified diff 文本

## 6. debate_candidates.json

`debate_picker.py` 输出：

```json
{
  "schema_version": "1.0",
  "candidates": [
    {
      "bucket_id": "BUG-002",
      "file": "...",
      "line": 10,
      "score": 0.55,
      "type_conflict": false,
      "reason": "borderline_score",
      "findings": []
    }
  ]
}
```

## 7. verdict.json

`weighted_vote.py` 输出：

```json
{
  "schema_version": "1.0",
  "threshold": 0.6,
  "accepted": [],
  "rejected": []
}
```

每个 verdict 项至少包含：

- `bucket_id`
- `file`
- `line`
- `primary_type`
- `evidence_count`
- `score`
- `consensus_strength`
- `findings`

## 8. decisions.json

`update_resolution_history.py` 输入：

```json
[
  {
    "agent": "Security Sentinel",
    "status": "accepted",
    "bucket_id": "BUG-001",
    "reason": "fixed in follow-up patch",
    "timestamp": "2026-03-13T10:00:00Z"
  }
]
```

兼容策略：只要包含 `agent` + `status` 即可。

## 9. review_history.json / weight_suggestion.json

- `review_history.json`：累计接受率、拒绝率、分角色统计。
- `weight_suggestion.json`：建议权重，格式为：

```json
{
  "suggested_weights": {
    "Security Sentinel": 4.8,
    "Diverse Reviewer A": 2.4
  }
}
```

`weighted_vote.py` 同时兼容：

- `{"suggested_weights": {...}}`
- `{"Security Sentinel": 5.0, "Concurrency Engineer": 4.0}`
