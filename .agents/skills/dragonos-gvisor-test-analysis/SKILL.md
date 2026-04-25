---
name: dragonos-gvisor-test-analysis
description: 通过对比 Linux/gvisor 参考实现来分析 DragonOS gVisor 测试失败。输出结构化的修复文档，包含对于所有失败用例的表格格式分析文档，针对每个具体的失败用例的详细格式修复文档，并提供代码片段。当用户提及 gVisor 测试失败、特定测试用例或询问 bug 分析/修复方案时使用。
version: 0.2.0
allowed-tools: Read, Grep, Glob, Bash
---

# DragonOS gVisor 测试失败分析器

## 目的

通过参考 Linux 内核和 gVisor 实现来分析 DragonOS 在 gVisor 测试套件中的失败。输出识别根本原因并提供可执行修复计划的文档。

## 参考路径

```
gVisor 测试:  ../gvisor/test/syscalls/linux/
Linux 内核:   ../linux/kernel/
DragonOS:     kernel/src/
```

## 工作流程

### 步骤 1: 解析测试失败

从用户输入中提取：
- 所有失败的测试名称（格式：`TestSuite.TestCase`）
- GTEST 输出消息，例如：
  ```
  [  RUN     ] WaitTest.Wait4Rusage
  [  FAILED  ] WaitTest.Wait4Rusage (0 ms)
  ```
- 错误模式或 panic 消息
- 堆栈跟踪（如果存在）

### 步骤 2: 定位测试代码

查找 gVisor 测试实现：
```
使用 Glob 查找: ../gvisor/test/syscalls/linux/*<syscall>*.cc
使用 Grep 查找: TEST.*<test_name>
```

### 步骤 3: 追踪系统调用路径

映射调用链：
```
测试 → 系统调用 → DragonOS 实现 → Bug
```

查找 DragonOS 实现：
```
使用 Grep 查找: fn sys_<syscall_name> 或 syscall!(<syscall_name>)
```

### 步骤 4: 对比 Linux 参考

查找 Linux 参考实现：
```
在 ../linux 中使用 Grep: SYSCALL_DEFINE.*<syscall_name>
```

### 步骤 5: 生成概览修复文档的“测试范围理解”和“内核子系统现状”部分

遵循 `references/FORMAT.md` 中的"概览格式"部分，生成其前两部分：
- 测试范围理解
- 内核子系统现状


### 步骤六: 循环生成详细单个测试修复文档

对于每个失败的测试，遵循 `references/FORMAT.md` 中的"单个测试格式"部分，生成详细的修复文档，知道没有更多失败测试需要处理为止。


### 步骤七：生成概览文档的"根因分析"和"修复建议"部分

结合 Overview 和 Detailed 文档，生成概览文档的最后两部分：

#### 7.1 生成第三部分：根因分析表格

汇总所有详细文档中的根因，提炼共性偏差：

```markdown
## 三、根因分析

| 测试点 | Linux 期望 | DragonOS 实际 | 差距 |
|-------|-----------|---------------|------|
| `[接口/字段]` | [Linux 行为] | [DragonOS 行为] | [偏差说明] |
| `[接口/字段]` | [Linux 行为] | [DragonOS 行为] | [偏差说明] |
```

**提炼规则**：
- 每行对应一个"接口/字段"级别的偏差
- 如果多个测试共享同一根因，在"差距"列注明 `（N 个测试）`
- 优先描述数据结构/状态层面的差异，而非行为表现
- 引用具体文件和行号作为证据

#### 7.2 生成第四部分：修复方案

##### 7.2.1 生成关键改动表格

```markdown
## 四、修复方案

### 4.1 关键改动
| 文件 | 改动 | 原因 |
|-----|------|------|
| `kernel/src/xxx.rs` | [具体修改] | [修改原因] |
| `kernel/src/yyy.rs` | [具体修改] | [修改原因] |
```

**生成规则**：
- 从所有 Detailed 文档的"修复方案"部分汇总
- 去重合并：同一文件的多个修改合并为一条
- 按依赖关系排序（基础改动在前）
- 原因列引用对应的根因（"差距"列）

##### 7.2.2 生成实现细节

```markdown
### 4.2 实现细节
[补充关键实现注意事项、依赖关系、风险点等]
```

**内容规则**：
- 说明跨文件修改的依赖顺序
- 标注可能影响的其他子系统（向后兼容性检查，防止回归）
- 列出需要同步修改的测试/文档
- 注明未完全实现的边缘情况（如果存在）


## 示例

完整的输入输出示例和详细使用场景，请参见 [EXAMPLES.md](references/EXAMPLES.md)。

该文件包含：
- 单个测试失败分析示例
- 多个测试失败批量分析示例
- 测试失败输出模式识别
- 系统调用路径追踪示例
- 根本原因分组示例

## 注意事项

- 始终引用 `file:line` 作为代码参考
- 代码片段应最小化（最多 5-10 行）
- 级联失败：注明哪个测试是根本原因
- 提出修复方案时考虑 DragonOS 架构约束
