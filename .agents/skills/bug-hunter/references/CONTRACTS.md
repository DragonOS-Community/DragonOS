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

## 5. debate_candidates.json

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

## 6. verdict.json

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

## 7. decisions.json

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

## 8. review_history.json / weight_suggestion.json

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
