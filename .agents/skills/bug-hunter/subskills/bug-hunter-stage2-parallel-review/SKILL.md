---
name: bug-hunter-stage2-parallel-review
description: bug-hunter 阶段 2 技能。负责将随机化后的 diff 按 persona 矩阵分发给 3 个子智能体并行评审，并收集统一 JSON 结果。
---

# Stage 2 并行评审

## ⚠️ 强制约束 - 禁止手工替代

**本阶段必须使用 Agent 工具并行启动 3 个子智能体，严禁手工编写 findings！**

### 验证检查
- 必须调用 `Agent` 工具恰好 **3 次**
- 每次 Agent 调用必须使用不同的 `description`（对应固定 persona）
- 每个 Agent 必须从 `shuffled_passes.json` 的 `passes[*]` 中随机选择 1 个 pass 作为输入
- 每个 Agent 必须返回有效的 JSON findings
- 禁止直接写入或手工构造 `raw_findings.json`

### 违规检测
如果 `raw_findings.json` 是手工编写的（而非从 3 个 Agent 收集），后续阶段将拒绝处理。

## 角色矩阵（固定 3 个）

3 个角色按缺陷类别正交划分，覆盖原 8 角色的全部关注点。每个角色职责互斥、可独立召回，避免低权重复角色造成的 token 浪费。

| # | Persona | 权重 | 合并自 | 关注类别 |
|---|---------|------|--------|----------|
| 1 | **Security & Concurrency Sentinel** | 4.0 | Security Sentinel + Concurrency Engineer | security + concurrency |
| 2 | **Logic & Correctness Reviewer** | 3.0 | Diverse Reviewer A/B/C | logic + boundary + error handling |
| 3 | **System & Performance Reviewer** | 3.0 | Performance Analyst + Diverse Reviewer D/E | performance + 资源生命周期 + Linux 语义契约 |

权重保留“安全/并发 > 正确性 > 系统与性能”的原始优先序，但下层两级对齐为 3.0，反映现代模型在宽域评审上的均衡能力。

## 执行步骤

### 步骤 1: 准备输入
读取 Stage 1 输出的 `shuffled_passes.json`

要求：

- 只能从 `passes[*].diff` 中选取评审输入
- 默认每个 Agent 抽取 1 个 pass
- 允许不同 Agent 抽到同一个 pass，但禁止所有 Agent 固定使用同一个 pass
- `shuffled_passes.json` 的 `passes` 数量应 ≥ 3（默认 `--passes 4`）以保留输入多样性
- 应记录每个 Agent 实际使用的 `pass_id`

### 步骤 2: 并行启动 3 个 Agent（必须）

在**单次响应**中并行调用 Agent 工具 3 次，每个使用不同的 persona 提示词。

每个 Agent 必须同时满足：

- persona 固定，不得退化成泛化代码审查
- 输入为“随机抽中的 1 份 pass.diff”
- 只关注当前 persona 相关问题
- 输出统一 finding schema

### 角色职责与提示词关注点

#### 1. `Security & Concurrency Sentinel`（权重 4.0）
高危且需要对抗性/时序推理的两类缺陷，合并以提升单 agent 信息密度：
- **安全**：权限边界、输入校验、越界访问、信息泄漏、路径遍历、注入面（含 ReDoS）。
- **并发**：锁顺序、竞态、原子性、可见性、死锁、丢唤醒。

#### 2. `Logic & Correctness Reviewer`（权重 3.0）
“代码是否做了它应该做的事”的正交正确性域：
- **逻辑正确性**：核心逻辑、状态迁移、条件分支遗漏、控制流回归。
- **边界条件**：空值/极值、长度与容量、资源上限、off-by-one。
- **错误处理**：返回码传播、错误路径回滚与清理、部分失败语义。

#### 3. `System & Performance Reviewer`（权重 3.0）
系统级健康度与效率，合并原 Performance Analyst 与 Diverse D/E：
- **性能**：热点路径、复杂度、无谓拷贝、阻塞等待、缓存失效。
- **资源生命周期**：引用关系、释放时机、RAII/作用域、泄漏风险。
- **Linux 语义一致性**：接口契约、行为兼容性、POSIX/内核语义对齐。

### 步骤 3: 收集并合并结果
- 收集所有 Agent 返回的 JSON
- 为每条 finding 保留 `agent`（值必须为上述 3 个角色名之一）
- 建议额外记录 `pass_id` 作为调试元数据；后续脚本会忽略未知字段
- 合并为单个 findings 数组或 `{"schema_version":"1.0","findings":[...]}` 包装对象
- 写入 `artifacts/raw_findings.json`

推荐输出结构：

```json
{
  "schema_version": "1.0",
  "findings": [
    {
      "file": "kernel/src/foo.rs",
      "line": 42,
      "type": "logic",
      "severity": "major",
      "description": "error path forgets to release inode reference",
      "fix_code": "drop(inode);",
      "confidence": 0.81,
      "agent": "System & Performance Reviewer",
      "pass_id": 3
    }
  ]
}
```

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
    "agent": "Security & Concurrency Sentinel"
  }
]
```

## 约束

- ✅ **必须**使用 Agent 工具并行启动
- ✅ **必须**启动恰好 3 个不同的 Agent（每个对应一个固定 persona）
- ✅ **必须**从 `shuffled_passes.json` 中抽样输入，而不是直接评审原始 diff
- ✅ **每个** Agent 必须返回有效的 JSON
- ❌ **禁止**手工编写 findings
- ❌ **禁止**直接写入 raw_findings.json
- ❌ **禁止**使用自己分析代替 Agent 评审
- ❌ **禁止**所有 Agent 默认共用同一个 pass 作为输入
- 每个发现必须提供 `file:line`
- 置信度范围限定在 `[0, 1]`
- `agent` 字段必填，值必须是上述 3 个角色名之一（缺失时由校验阶段回退到 `Logic & Correctness Reviewer`）
- 纯风格建议直接过滤
