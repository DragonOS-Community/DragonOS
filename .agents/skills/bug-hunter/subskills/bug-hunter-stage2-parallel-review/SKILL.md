---
name: bug-hunter-stage2-parallel-review
description: bug-hunter 阶段 2 技能。负责将随机化后的 diff 按 persona 矩阵分发给 8 个子智能体并行评审，并收集统一 JSON 结果。
---

# Stage 2 并行评审

## ⚠️ 强制约束 - 禁止手工替代

**本阶段必须使用 Agent 工具并行启动 8 个子智能体，严禁手工编写 findings！**

### 验证检查
- 必须调用 `Agent` 工具至少 **8 次**
- 每次 Agent 调用必须使用不同的 `description`
- 每个 Agent 必须返回有效的 JSON findings
- 禁止直接写入或手工构造 `raw_findings.json`

### 违规检测
如果 `raw_findings.json` 是手工编写的（而非从 8 个 Agent 收集），后续阶段将拒绝处理。

## 角色矩阵（固定 8 个）

1. **Security Sentinel** - 权重 5.0 - 专注安全漏洞（ReDoS、注入、路径遍历）
2. **Concurrency Engineer** - 权重 4.0 - 专注并发问题（死锁、竞态、原子性）
3. **Performance Analyst** - 权重 3.0 - 专注性能问题（算法复杂度、内存、I/O）
4. **Diverse Reviewer A** - 权重 2.0 - 专注逻辑错误
5. **Diverse Reviewer B** - 权重 2.0 - 专注边界条件
6. **Diverse Reviewer C** - 权重 2.0 - 专注错误处理
7. **Diverse Reviewer D** - 权重 2.0 - 专注代码质量
8. **Diverse Reviewer E** - 权重 2.0 - 专注可维护性

## 执行步骤

### 步骤 1: 准备输入
读取 Stage 1 输出的 `shuffled_passes.json`

### 步骤 2: 并行启动 8 个 Agent（必须）

在**单次响应**中并行调用 Agent 工具 8 次，每个使用不同的 persona 提示词

### 步骤 3: 收集并合并结果
- 收集所有 Agent 返回的 JSON
- 合并为单个 findings 数组
- 写入 `artifacts/raw_findings.json`

## 输出格式要求

每个 Agent 必须返回纯 JSON 数组：
```json
[
  {
    "file": "path/to/file.py",
    "line": 42,
    "type": "security|concurrency|performance|logic",
    "severity": "critical|major|minor",
    "description": "问题描述",
    "fix_code": "修复代码片段",
    "confidence": 0.9,
    "agent": "Security Sentinel"
  }
]
```

## 约束

- ✅ **必须**使用 Agent 工具并行启动
- ✅ **必须**启动至少 8 个不同的 Agent
- ✅ **每个** Agent 必须返回有效的 JSON
- ❌ **禁止**手工编写 findings
- ❌ **禁止**直接写入 raw_findings.json
- ❌ **禁止**使用自己分析代替 Agent 评审
- 每个发现必须提供 `file:line`
- 置信度范围限定在 `[0, 1]`
- `agent` 字段必填，值必须是当前角色名
- 纯风格建议直接过滤
