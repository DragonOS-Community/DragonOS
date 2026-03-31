---
name: bug-hunter-stage1-input-randomization
description: bug-hunter 阶段 1 技能。负责提取代码改动、执行敏感信息脱敏，并按文件/代码块生成多轮随机化输入以缓解 LLM 位置偏差。
---

# Stage 1 输入随机化

## 目标

把原始 diff 转换为可并行评审的多轮随机输入，并保证敏感信息不会泄露给子智能体。

## 步骤

1. 用 `git diff --cached` 获取改动；若为空，回退为“upstream/origin/HEAD/HEAD~1”自适应基线：
   `BASE_REF="$(git rev-parse --abbrev-ref --symbolic-full-name @{upstream} 2>/dev/null || git symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null || echo HEAD~1)"`，再执行 `git diff "$(git merge-base HEAD "$BASE_REF")"...HEAD`。
2. 将 diff 传入 `scripts/redact_sensitive.py` 完成脱敏。
3. 将脱敏后的 diff 传入 `scripts/shuffle_diff.py --passes 8` 生成 8 轮随机序列。
4. 将输出 JSON 保存为 `artifacts/shuffled_passes.json`。

## 验收

- 每一轮都包含完整改动信息。
- 不同轮次的顺序差异明显。
- 结果中不存在明文密钥与凭据。
