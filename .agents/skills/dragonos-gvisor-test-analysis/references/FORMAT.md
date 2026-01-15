# DragonOS Bug Fix 文档格式规范

本文档定义了两种标准的修复文档格式：批量格式（用于多个测试失败）和单个测试格式（用于深度分析）。

---

## 格式一：批量格式（Batch Format）

用于分析 3+ 个测试失败时，提供高密度表格概览。

### 模板

```markdown
# <test_suite_or_syscall> 失败分析与修复方案

**背景**:
- gVisor 测试: `../gvisor/test/syscalls/linux/<test_file>.cc`（参考行 X, Y, Z）
- Linux 参考: `../linux/kernel/<path>.c`（`function_name` 等）

## 根因汇总

1) [根因1 - 简洁描述]
2) [根因2 - 简洁描述]
3) [根因3 - 简洁描述]

## 按测试项映射（原因、修复、变更范围）

| 测试 | 原因 | 修复方案 | 变更范围 |
| --- | --- | --- | --- |
| TestSuite.TestCase1 | [简洁原因] | [具体修复] | `kernel/src/xxx.rs`, `kernel/src/yyy.rs` |
| TestSuite.TestCase2 | [简洁原因] | [具体修复] | `kernel/src/xxx.rs` |
| TestSuite.TestCase3+ | [同上（N个测试）] | [同上] | 同上 |

## 实现要点

- [关键实现细节1]
- [关键实现细节2]
- [注意事项]
```

### 实际示例


---

## 格式二：单个测试格式（Single Test Format）

用于 1-2 个具体测试失败的深度分析，包含代码片段和行号引用。

### 模板
```
# Bug Fix: <test_suite>.<test_case>

## 根因分析

[详细描述问题的本质，为什么会产生这个错误]

## 测试行为

**gVisor 测试**: `../gvisor/test/syscalls/linux/<file>.cc:<line>`

测试逻辑：
- [步骤1]
- [步骤2]
- ...
- [步骤x]
- [预期结果]

```c
// ../gvisor/test/syscalls/linux/<file>.cc:<line>
TEST(<Test_name>, <Test_case>) {
    [足够说明测试逻辑的代码片段，行数不被全局约束所限制]
}
```

**实际错误**: [错误信息或现象]

## 参考实现（Linux）

**文件**: `../linux/<path>.c`

```c
// <line_number>
[关键代码片段 5-10 行]
```

关键语义：
- [语义点1]
- [语义点2]

## DragonOS 当前实现

**文件**: `kernel/src/<path>.rs`

```rust
// <line_number>
[当前代码片段]
```

问题：
- [缺失/错误的逻辑]
- [与 Linux 的差异]

## 修复方案

1. [具体修改1 - 在某文件添加某逻辑]
2. [具体修改2 - 修改某函数的某行为]

## 变更范围

- **文件**: `kernel/src/xxx.rs`, `kernel/src/yyy.rs`
- **风险**: [描述可能的副作用或兼容性问题]
- **依赖**: [如果涉及多个修改，说明依赖顺序]

## 预期结果

- [测试通过条件]
- [副作用检查]
```

### 实际示例

参考 `../DragonOS-fix-shm_test/docs/shm_test_fix_zh.md`

---

## 通用规则

### 引用规范
- Linux 参考: `../linux/<path>.c:<line>` 或 `function_name`
- gVisor 测试: `../gvisor/test/syscalls/linux/<file>.cc:<line>`
- DragonOS: `kernel/src/<path>.rs:<line>`

### 代码片段
- 最多 20 行，只包含关键逻辑
- 始终标注文件路径和行号
- 使用实际代码，非伪代码

### 表格单元格
- 最多 20 词
- 使用技术术语，避免冗长解释
- 多个测试共享同一原因/修复时，注明 "（N 个测试）"

### 根因描述
- 先说数据结构/状态问题，后说行为表现
- 引用 Linux/gvisor 的具体语义
- 说明 DragonOS 当前实现的偏差点
