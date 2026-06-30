---
name: dragonos-general-development-workflow
description: >
  DragonOS 通用开发流程技能。
  当任务涉及以下任一场景时使用：(1) 设计或修复 DragonOS 内核功能、系统调用、procfs/sysfs/devfs 行为；
  (2) 审查 DragonOS 代码或 PR；(3) 对比 Linux 6.6 语义进行实现或验证；
  (4) 分析或修复 gVisor 系统调用测试失败；(5) 添加 dunitest 内核单元测试覆盖；
  (6) 在 QEMU nographic 模式下运行和交互 DragonOS。
---

# DragonOS 通用开发流程

## 核心规则

- DragonOS 应提供 Linux 兼容的用户可见行为，严格对照 Linux 6.6 源码语义。
- 用户需在任务开始时提供以下路径（如未提供则主动询问）：
  - **$LINUX_SRC**：本地 Linux 6.6 源码路径
  - **$GVISOR_TESTS**（可选）：gVisor 测试源码路径
  - 如果用户未提供且任务涉及 Linux 语义对比或 gVisor 测试，**必须先询问路径再继续**。
- 代码审查注重内存安全、并发安全，尤其涉及系统调用、procfs、sysfs、devfs、VFS、调度器、信号、IPC、内存管理、网络和驱动可见行为。
- （可选）新回归测试优先使用 dunitest（自动化 CI 回归的一部分）；不要仅在遗留的 `user/apps/c_unitest` 中添加新覆盖，除非用户明确要求或没有可行的 dunitest 路径。
- **不得使用 workaround 方法绕过失败的测试**：不得为通过特定测试而修改 gVisor 测试用例、不得用绕过方法掩盖根因、不得特化某个观察到的用例。用架构合理、向 Linux 6.6 源码语义对齐的方式在 DragonOS 中修复**根因**。
- 仅允许修改 DragonOS 仓库内的文件，除非用户明确要求修改全局配置或外部文件。
- 不得修改 `env.mk`。如果已被修改过，视为用户拥有的状态：不触碰、不还原、不纳入任何 git commit。
- 不得在公开产物（PR 描述、issue 评论等）中提及用户本地的已修改文件（如 `env.mk`），除非用户明确要求。
- 提交使用英文 Conventional Commits 格式（如 `fix(vfs): handle mount propagation edge case`），使用 `git commit -s` 签署。代码注释用中文描述修复的根因和改动的语义，并避免提及测试细节或用户本地状态。
- 灵活利用 subagent 并行拆解和分析问题，将研究结果汇总到主 agent，避免在主上下文中做深度推理。

## 参考路径

- DragonOS 仓库：当前工作目录
- Linux 参考源码：**$LINUX_SRC**（用户提供）
- （可选）gVisor 系统调用测试：**$GVISOR_TESTS**（用户提供，仅 gVisor 相关任务时询问）
- （可选）DragonOS dunitest 测试：在 DragonOS 树中搜索现有 dunitest 模式后再添加新测试。

## 强制开发循环

### 适用性判断
根据任务性质选择执行路径：

| 任务类型 | 典型触发语 | 执行路径 |
|---|---|---|
| **实现/修复** | "实现"、"修复"、"添加"、"改一下"、"卡死"、"fails" | 完整循环 Step 0-6 |
| **纯调查/问答** | "解释"、"分析"、"为什么"、"怎么实现的"、"看看" | 仅 Step 1（研究），然后直接回答 |
| **代码审查** | "审查"、"review"、"看看这段代码"、"对比" | Step 1（研究）+ Step 5（对照审查） |
| **混合/不确定** | 既有调查又有"然后修复"的意图 | 先完整执行 Step 1，确认意图后再决定是否进入 Step 2-6 |

当用户只要求调查、解释、分析代码行为时，**不要进入实施循环**。完成研究后直接报告发现即可。

### 循环步骤
0. **复现**（仅缺陷修复任务必做；纯新增功能跳过此步）：在修复前的 DragonOS 内核上构建并运行相关复现程序/测试。优先通过 PTY 在 QEMU 中启动 DragonOS 并在 guest 内验证。复现后保留等效测试作为修复后验证。
1. **研究**：捕获用户可见的症状/失败测试/panic 日志。阅读相关的 DragonOS 实现及附近抽象。阅读 **$LINUX_SRC** 下对应的 Linux 6.6 源码实现（调用路径、数据结构不变量、锁/生命周期、返回值、errno、边界情况）。必要时阅读相关测试（gVisor 系统调用测试、现有 dunitest）。
2. **制定计划**（仅在该研究之后）：阐明**根因**或**语义差距**。标识需要变更的 DragonOS 模块/文件及变更理由。描述要匹配的 Linux 行为和边界情况。包含验证步骤、预期测试覆盖和残余风险。计划必须符合核心规则（无 workaround、无测试特化）。制定计划时，加载 [references/plan-prompts.md](references/plan-prompts.md) 将模板嵌入任务提示。
3. **审查计划**：检查是否遵循 Linux 语义、适配 DragonOS 架构、保持并发和生命周期不变量。如果计划不完整或有风险，**先修订计划**。
4. **实施**（仅计划通过审查后）：限定变更范围在计划中的文件。如果实施揭示了**不同的根因或架构问题**，**立即停止**并告知用户，然后马上回到 Step 2 重新制定计划。
5. **实施后再审查**：重新阅读 **$LINUX_SRC** 相关 Linux 源码，对比最终 DragonOS 行为、数据流、锁/生命周期、错误路径和边界情况。如发现语义不匹配，回到 Step 2。
6. **验证**：可以使用针对性的 dunitest 或 gVisor 覆盖。先运行 `make fmt` 进行代码格式化和 clippy 检查，再运行 `make kernel` 确保编译通过。对于影响内核/用户可见行为的变更，构建 DragonOS，通过 PTY 在 QEMU 中启动，在 guest 内验证（dunitest 路径：`/opt/tests/dunitest/bin/normal/`）。详细的 QEMU 操作步骤见 [references/qemu-operations.md](references/qemu-operations.md)。报告任何未能运行的验证及原因。当需要创建 PR 时，在根因修复已提交、推送并创建 PR 后任务完成。

## 相关技能

以下技能按场景分类，配合本通用开发流程使用：

### 代码审查
- **bug-hunter**：大规模 PR、复杂逻辑变更、安全敏感改动的多智能体并行缺陷检测。触发语："用 bug-hunter 跑一遍"、"做深度bug检测"

### 测试分析与修复
- **dragonos-gvisor-test-analysis**：分析 gVisor 系统调用测试失败，对比 Linux/gVisor 参考实现，输出结构化修复文档。触发语："gVisor 测试挂了"、"分析这个 gVisor 失败测例"

### 调试疑难问题
- **dragonos-atomic-snapshot-debug**：调试内核时序问题、Heisenbug、阻塞挂起、丢唤醒。触发语："任务卡住了"、"CPU idle 但请求不返回"、"在线取证"
