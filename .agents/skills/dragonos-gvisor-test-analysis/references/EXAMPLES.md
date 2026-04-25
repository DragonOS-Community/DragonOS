# 示例

本文件包含 dragonos-gvisor-test-analysis skill 的使用示例。

## 单个测试失败分析

**输入**：
```
分析一下 WaitTest.Wait4Rusage 为什么失败
```

**处理流程**（符合 7 步工作流）：
1. **步骤 1** - 解析测试失败：提取 `WaitTest.Wait4Rusage`、错误消息、堆栈跟踪
2. **步骤 2** - 定位测试代码：`Glob` 查找 `../gvisor/test/syscalls/linux/*wait*.cc`，`Grep` 查找 `TEST.*Wait4Rusage`
3. **步骤 3** - 追踪系统调用路径：映射 `wait4` → DragonOS 实现 `kernel/src/process/exit.rs`
4. **步骤 4** - 对比 Linux 参考：在 `../linux` 中 `Grep` 查找 `SYSCALL_DEFINE.*wait4`
5. *（跳过）* - 单个测试无需生成概览文档的第一、二部分
6. **步骤 6** - 生成详细格式修复文档：遵循 FORMAT.md"单个测试格式"，包含代码片段和逐行对比
7. *（跳过）* - 单个测试无需生成概览文档的第三、四部分

**输出格式**：详细格式（单个测试格式）
- 包含来自 `exit.c:wait4` 和 `process/exit.rs` 的代码片段
- 逐行对比分析
- 详细的修复步骤

详见 [FORMAT.md](FORMAT.md) 中的"单个测试格式"部分。

---

## 多个测试失败批量分析

**输入**：
```
wait_test 失败了20个测试用例，包括 Wait4Rusage, WaitidRusage, WaitAnyChildTest.*,
请对比 Linux 和 gvisor 的实现，分析失败原因并给出修复方案
```

**处理流程**（完整 7 步工作流）：
1. **步骤 1** - 解析测试失败：提取出 20 个测试用例的名称、错误消息、堆栈跟踪
2. **步骤 2** - 定位测试代码：`Glob` 查找 `../gvisor/test/syscalls/linux/*wait*.cc`，`Grep` 查找各测试用例
3. **步骤 3** - 追踪系统调用路径：映射 `wait4/waitid` → DragonOS 实现
4. **步骤 4** - 对比 Linux 参考：在 `../linux` 中查找 `SYSCALL_DEFINE` 定义
5. **步骤 5** - 生成概览文档第一、二部分：遵循 FORMAT.md"概览格式"，生成"测试范围理解"和"内核子系统现状"
6. **步骤 6** - 循环生成详细文档：对每个测试生成"单个测试格式"文档，直到没有更多失败测试
7. **步骤 7** - 生成概览文档第三、四部分：
   - 7.1 汇总所有详细文档的根因，提炼共性偏差，生成"根因分析"表格
   - 7.2 汇总所有修复方案，去重合并，生成"修复方案"部分（关键改动表格 + 实现细节）

**输出格式**：概览格式（表格格式） + 详细格式（单个测试格式）
- 概览文档：包含测试范围、内核现状、根因分析表格、修复方案表格
- 详细文档：每个测试一个文档，包含代码片段和逐行对比

详见 [FORMAT.md](FORMAT.md) 中的"概览格式"和"单个测试格式"部分。

---

## 测试失败输出模式识别

**输入（GTEST 输出）**：
```
[  RUN     ] WaitTest.Wait4Rusage
[  FAILED  ] WaitTest.Wait4Rusage (0 ms)
```

**识别内容**：
- 测试套件：`WaitTest`
- 测试用例：`Wait4Rusage`
- 完整名称：`WaitTest.Wait4Rusage`
- 失败状态：`FAILED`

**输入（panic 消息）**：
```
panicked at 'assertion failed: `(left == right)`
  left: `12`,
 right: `0`', kernel/src/process/exit.rs:42:13
```

**识别内容**：
- Panic 位置：`kernel/src/process/exit.rs:42:13`
- 断言失败：期望值 0，实际值 12
- 可能根因：`rusage` 结构体字段未正确初始化

---

## 系统调用路径追踪示例

**目标**：追踪 `wait4` 系统调用

**步骤**：
1. **查找 gVisor 测试**：
   ```
   Glob: ../gvisor/test/syscalls/linux/*wait*.cc
   Grep: TEST.*Wait4Rusage
   ```

2. **查找 DragonOS 实现**：
   ```
   Grep: fn sys_wait4
   Grep: syscall!(wait4)
   ```

3. **查找 Linux 参考**：
   ```
   Grep (in ../linux): SYSCALL_DEFINE.*wait4
   ```

4. **映射调用链**：
   ```
   WaitTest.Wait4Rusage (测试)
     → gVisor test/cc (测试代码)
       → wait4 syscall (系统调用)
         → DragonOS kernel/src/process/exit.rs (DragonOS 实现)
           → Bug: rusage 未初始化
   ```

---

## 根本原因分组示例

当有多个测试失败时，按根本原因分组而非按测试套件分组：

**错误的分组**（按测试套件）：
```
WaitTest: 5 个失败
WaitidTest: 3 个失败
SignalTest: 4 个失败
```

**正确的分组**（按根本原因）：
```
根因 1: rusage 结构体未正确初始化
  - WaitTest.Wait4Rusage
  - WaitTest.WaitidRusage
  - SignalTest.WaitidRusage

根因 2: 进程状态转换错误
  - WaitTest.WaitAnyChildTest.Pid
  - WaitTest.WaitAnyChildTest.Pgid

根因 3: 信号处理不完整
  - SignalTest.SignalDelivery
  - SignalTest.SignalMask
  ...
```

这样可以一次性解决所有相关问题，而不是逐个修复。
