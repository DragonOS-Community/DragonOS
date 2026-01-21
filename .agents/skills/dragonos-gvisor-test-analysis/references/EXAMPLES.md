# 示例

本文件包含 dragonos-gvisor-test-analysis skill 的使用示例。

## 单个测试失败分析

**输入**：
```
分析一下 WaitTest.Wait4Rusage 为什么失败
```

**处理流程**：
1. 解析测试名称：`WaitTest.Wait4Rusage`
2. 定位测试代码：`../gvisor/test/syscalls/linux/*wait*.cc`
3. 追踪系统调用：`wait4` → DragonOS 实现
4. 对比 Linux 参考：`SYSCALL_DEFINE(wait4)`
5. 生成详细格式的修复文档

**输出格式**：详细格式
- 包含来自 `exit.c:wait4` 和 `process/exit.rs` 的代码片段
- 逐行对比分析
- 详细的修复步骤

详见 [FORMAT.md](FORMAT.md) 中的"单测试格式"部分。

---

## 多个测试失败批量分析

**输入**：
```
wait_test 失败了20个测试用例，包括 Wait4Rusage, WaitidRusage, WaitAnyChildTest.*,
请对比 Linux 和 gvisor 的实现，分析失败原因并给出修复方案
```

**处理流程**：
1. 解析所有失败测试：提取出 20 个测试用例
2. 选择批量格式：≥ 3 个失败使用表格格式
3. 按根本原因分组：将 20 个测试按 4-5 个根因分类
4. 对每组分析：参考 Linux/gvisor 实现
5. 生成表格格式的修复文档

**输出格式**：表格格式
- 约 20 行，每个测试一行
- 按根本原因分组
- 包含根因列和修复建议列

详见 [FORMAT.md](FORMAT.md) 中的"批量格式"部分。

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
