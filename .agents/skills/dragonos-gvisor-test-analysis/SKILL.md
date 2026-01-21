---
name: dragonos-gvisor-test-analysis
description: 通过对比 Linux/gvisor 参考实现来分析 DragonOS gVisor 测试失败。输出结构化的修复文档，采用表格格式（3+个失败）或详细格式（1-2个失败）并提供代码片段。当用户提及 gVisor 测试失败、特定测试用例或询问 bug 分析/修复方案时使用。
version: 0.1.1
allowed-tools: Read, Grep, Glob, Bash
---

# DragonOS gVisor 测试失败分析器

## 目的

通过参考 Linux 内核和 gVisor 实现来分析 DragonOS 在 gVisor 测试套件中的失败。输出识别根本原因并提供可执行修复计划的文档。

## 输出格式选择

- **1-2 个测试失败** → 详细格式，包含代码片段和逐行对比
- **3+ 个测试失败** → 表格格式，按根本原因分组以便快速概览

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

### 步骤 2: 选择格式

统计唯一的测试失败数量：
- ≤ 2: 使用**单测试格式**（参见 FORMAT.md）
- ≥ 3: 使用**批量格式**（参见 FORMAT.md）

### 步骤 3: 定位测试代码

查找 gVisor 测试实现：
```
使用 Glob 查找: ../gvisor/test/syscalls/linux/*<syscall>*.cc
使用 Grep 查找: TEST.*<test_name>
```

### 步骤 4: 追踪系统调用路径

映射调用链：
```
测试 → 系统调用 → DragonOS 实现 → Bug
```

查找 DragonOS 实现：
```
使用 Grep 查找: fn sys_<syscall_name> 或 syscall!(<syscall_name>)
```

### 步骤 5: 对比 Linux 参考

查找 Linux 参考实现：
```
在 ../linux 中使用 Grep: SYSCALL_DEFINE.*<syscall_name>
```

### 步骤 6: 生成修复文档

遵循 `references/FORMAT.md` 中的相应格式：
- **单测试格式**（1-2个失败）：包含代码片段的详细分析
- **批量格式**（3+个失败）：按根本原因分组的表格格式

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
- 对于批量格式，先按根本原因分组测试，而不是按测试套件
- 级联失败：注明哪个测试是根本原因
- 如果 Linux/gvisor 存在差异，说明你的选择和理由
- 提出修复方案时考虑 DragonOS 架构约束
