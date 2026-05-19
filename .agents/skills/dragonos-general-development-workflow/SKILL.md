---
name: dragonos-general-development-workflow
description: "DragonOS 通用开发流程技能：设计或修复内核/用户可见行为、审查 DragonOS 代码、对比 Linux 6.6 语义、处理 gVisor 系统调用测试、添加 dunitest 覆盖、在 QEMU nographic 模式下运行和交互 DragonOS。"
---

# DragonOS通用开发流程

## 核心规则

- DragonOS 应提供 Linux 兼容的用户可见行为。
- 用户需提供本地的 Linux 6.6 源码路径（如 `~/code/linux-6.6/`），如与 gVisor 系统调用测试用例相关的开发还需提供 gVisor 测试的源码路径（ 如 `~/code/gvisor/test/syscalls/linux/`）。
- 在实现、修复或审查暴露给用户空间的行为时，必须严格对照 `~/code/linux-6.6/` 下的 Linux 源码进行对比。
- 代码审查应将 DragonOS 代码与对应的 Linux 实现/语义结合考虑，注重内存安全、并发安全，尤其涉及系统调用、procfs、sysfs、devfs、VFS、调度器、信号、IPC、内存管理、网络和驱动可见行为。
- 功能开发和缺陷修复在可行时应添加或更新 DragonOS dunitest 覆盖。新回归测试优先使用 dunitest，因为它是自动化 CI 回归的一部分；不要仅在遗留的 `user/apps/c_unitest` 中添加新覆盖，除非用户明确要求手动 C 测试或没有可行的 dunitest 路径。
- 现有的 gVisor 系统调用测试用例是参考测试，不得修改以使 DragonOS 通过。从 `~/code/gvisor/test/syscalls/linux/` 读取。
- 不得使用 workaround 方法绕过失败的测试。用架构合理的方式在 DragonOS 中修复根因。
- 仅修改 DragonOS 仓库内的文件，除非用户明确要求修改全局配置或外部文件。
- 在 DragonOS 任务中不得修改 `env.mk`。如果 `env.mk` 已经被修改过，将其视为用户拥有的状态：不触碰、不还原、不纳入任何 git commit。
- 不得在公开产物（PR 描述、issue 评论、审查评论、更新日志或发布说明）中提及用户本地的已修改文件（如 `env.mk`），除非用户明确要求发布该信息。在与用户的直接对话中需要时可以简要提及。
- 提交 DragonOS 变更时，使用英文 Conventional Commits 格式，例如 `fix(vfs): handle mount propagation edge case`。
- 使用 `git commit -s` 签署提交。

## 强制开发循环

对于 DragonOS 的实现、缺陷修复、根因分析或审查任务，除非用户明确要求只读调查，否则遵循以下循环：

0. 对于缺陷修复任务，在编辑代码之前**先复现**或**证伪**报告的问题。
   - 在可行的情况下，在修复前的 DragonOS 内核上构建并运行相关的复现程序/测试。优先在 QEMU 中启动 DragonOS 并在 guest 内验证，而非仅靠宿主机推理。
   - 如果问题提供了 PoC，先在未修改的内核上运行该 PoC 或等效的回归测试，并捕获具体症状：panic 日志、失败的断言、errno、信号、挂起或不匹配。
   - 如果直接在修复前复现不可行，明确记录原因，并在实施前使用最接近的可用证据。不要将修复后通过的测试视为原始 bug 存在的证明。
   - 复现后，保留相同或等效的测试作为修复后验证，以便前后行为可比较。
1. 在提出代码变更之前先研究问题。
   - 捕获用户可见的症状、失败的测试、panic 日志或请求的语义变更。
   - 阅读相关的 DragonOS 实现及附近的抽象。
   - 阅读 `~/code/linux-6.6/` 下对应的 Linux 6.6 实现，包括调用路径、数据结构不变量、锁/生命周期规则、返回值、errno 行为和边界情况。
   - 在可用时阅读相关测试，尤其是 gVisor 系统调用测试和现有 DragonOS dunitest 覆盖。
2. **仅在该研究之后**制定具体**计划**。
   - 阐明要修复的根因或语义差距。
   - 标识需要变更的 DragonOS 模块/文件以及变更归属的理由。
   - 描述要匹配的 Linux 行为和重要的边界情况。
   - 包含验证步骤、预期的测试覆盖和任何残余风险。
   - 不得提出 workaround、测试特化绕过或仅使某个观察到的用例通过的行为。
3. 在编辑代码之前审查计划。
   - 检查计划是否遵循 Linux 语义、适配 DragonOS 架构、保持并发和生命周期不变量，且不产生隐藏的兼容性差距。
   - 如果计划不完整或有风险，**先修订计划**，而不是开始实施。
4. 仅在计划通过该审查后实施。
   - 将变更范围限定在计划中的 DragonOS 文件，除非实施过程中的研究证明计划必须变更。
   - 如果实施揭示了**不同的根因或架构问题**，**立即停止**代码变更，回到计划阶段，在继续之前审查新计划。
5. 代码变更后，再次对照 Linux 审查。
   - 重新阅读相关的 Linux 6.6 代码，对比最终的 DragonOS 行为、数据流、锁/生命周期处理、错误路径和边界情况。
   - 如果变更后的审查发现语义不匹配、架构问题、workaround 或遗漏的边界情况，回到计划步骤，在进一步变更之前制定修复计划。
6. 用聚焦的测试/构建进行验证。
   - 在适用时优先使用针对性的 dunitest 或 gVisor 覆盖，运行 `make kernel` 或更窄的可用检查来捕获编译错误。对于新添加的 DragonOS 回归覆盖，使用 dunitest 而非遗留的 `c_unitest`，以便 CI 自动运行。
   - 在目标模式下，不要仅基于宿主机编译或本地执行的测试二进制就将目标标记为完成。构建 DragonOS，在 QEMU 中启动，并在 DragonOS guest 内验证相关测试（当变更影响内核/用户可见行为时）。
   - 在 `/goal` 模式下进行 dunitest 覆盖时，DragonOS 启动后，从 guest 路径 `/opt/tests/dunitest/bin/normal/` 运行对应的测试二进制，例如 `cd /opt/tests/dunitest/bin/normal/ && ./xxx_test`。
   - 报告任何未能运行的验证及原因。
   - 当用户的 DragonOS 目标需要创建 PR 时，目标在根因修复已提交、推送、并创建包含验证证据的 PR 后即完成。不要等待 GitHub CI 完成，除非用户明确要求跟进 CI；如有需要可提及待定的 CI 状态。

## 必需的计划提示

在为 DragonOS 设计功能计划、实施计划、审查计划或缺陷修复/根因计划时，在提示或任务框架中明确包含以下指令：

```text
先结合Linux代码、问题现象、dragonos代码深入研究，再制定plan；制定后先审查plan是否符合Linux语义、DragonOS架构、并发/生命周期不变量、错误路径和边界条件，确认无workaround、无测试特化、无隐藏坑点后才实施代码变更。

代码变更后，必须再次结合Linux代码审查DragonOS实现。如果发现语义不一致、架构不合理、边界条件遗漏、并发/生命周期风险或workaround，必须回到plan阶段重新制定修复计划，再继续实施。

所有方案都要参考Linux代码、dragonos代码、深入研究，并且制定正确、完善、无坑点、无workaround、架构合理、功能正确的实现/根因修复计划。

Linux代码在： ~/code/linux-6.6/
```

## 参考路径

- DragonOS 仓库：`/root/code/DragonOS`
- Linux 参考源码：`~/code/linux-6.6/`
- gVisor 系统调用测试：`~/code/gvisor/test/syscalls/linux/`
- DragonOS dunitest 测试：在 DragonOS 树中搜索现有 dunitest 模式后再添加新测试。新的回归测试通常应为 dunitest 测试，而非遗留的 `user/apps/c_unitest` 测试，以确保被 CI 覆盖。

## 运行 DragonOS

从仓库根目录在 QEMU nographic 模式下运行 DragonOS，通过终端串口控制台与 guest 交互。

### 命令选择

- 当用户需要正常运行路径时使用 `make run-nographic`：根据需要重建内核/用户程序，更新 rootfs/磁盘镜像，然后启动 QEMU。
- 当用户明确要求跳过重建内核且避免更新 rootfs，或已有最新镜像且任务仅需启动/交互时，使用 `make qemu-nographic`。
- 从 DragonOS 仓库根目录运行这些命令，通常为 `/root/code/DragonOS`。

### 交互流程

1. 在支持 TTY 的终端会话中使用上述命令之一启动 QEMU。
2. 等待启动日志到达：

   ```text
   Please press Enter to activate this console.
   ```

3. 发送一次 Enter。预期的 guest 提示符类似：

   ```text
   root@dragonos:~# 
   ```

4. 正常输入命令；命令在 DragonOS 内执行。快速验证命令：

   ```sh
   pwd
   uname -a
   ls /
   ```

5. 在目标模式下验证 dunitest 用例时，在 guest 内运行相关的已安装测试二进制：

   ```sh
   cd /opt/tests/dunitest/bin/normal/
   ./xxx_test
   ```

   将 `xxx_test` 替换为变更对应的具体测试二进制，例如 `fdatasync_test`。

6. 使用 QEMU 转义序列退出 QEMU nographic 模式：按 `Ctrl-a`，然后按 `x`。在工具输入中，这是字节 `\x01` 后跟 `x`。

### 实用注意事项

- `make qemu-nographic` 使用现有的 `bin/kernel/kernel.elf` 和 `bin/disk-image-x86_64.img`。
- `make run-nographic` 可能需要宿主机权限，因为镜像更新路径会挂载/写入磁盘镜像。
- 若不是 root 用户，可以向用户索要密码。如果用户已经提供密码，优先使用以下命令预热sudo：

```bash
printf '%s\n' "$PASSWORD" | sudo -S -v
```

  这会先验证 sudo 权限（提示输入密码），然后在同一个带 TTY 的会话中运行 `make run-nographic`，确保后续的构建和启动步骤都具有必要的权限。
- Nographic 串口输出也会写入 DragonOS 仓库根目录的 `serial_opt.txt`。当终端输出过长或有溢出上下文的风险时，使用有针对性的 `rg`/`grep`/`tail` 命令检查该文件，而不是读取整个终端缓冲区。
- 实用示例：

  ```sh
  rg -n "Please press Enter|root@dragonos|ERROR|WARN|panic|Boot with specified init" serial_opt.txt
  tail -n 200 serial_opt.txt
  ```

- 如果 `make run-nographic` 在写入磁盘镜像时失败，记录实际错误。纯交互测试的有效回退方案是 `make qemu-nographic`，但不要将其视为 rootfs 更新测试。
- 已观察到的一种非权限失败情况：在成功构建内核后，在 `bin/mnt/disk-image-x86_64` 下反复出现 `cp: cannot stat '.../bin/...': Bad message`，随后 `write_diskimage` 失败。在这种情况下，`qemu-nographic` 可能仍能用现有镜像启动，但完整的镜像更新路径未成功。
- 在成功的本地验证中，`make qemu-nographic` 打印了 `QEMU accel=kvm`，启动到 DragonOS 横幅，需要按 Enter 激活控制台，并在 `root@dragonos:~#` 接受 shell 命令。
- 测试后避免在后台留下 QEMU 运行。始终用 `Ctrl-a x` 关闭，除非用户要求保持会话打开。

- 重要：灵活利用subagent拆解与分析问题，然后向主agent汇报，而不是经常占用主agent的上下文思考问题！

## 实用 SKILL
以下技能按场景分类，配合本通用开发流程使用。当任务匹配到对应场景时，应加载对应 skill 以获得专业化指导。

### 日常开发
**dragonos-general-development-workflow**（本技能）：通用开发、设计功能、修复 bug、审查代码、对照 Linux 语义，任何 DragonOS 开发任务的默认入口 

### 代码审查
**bug-hunter**：大规模 PR、复杂逻辑变更、安全敏感改动的多智能体并行缺陷检测
典型触发语："用 bug-hunter 跑一遍"、"做深度缺陷检测"、"多 agent review"

### 测试分析与修复
**dragonos-gvisor-test-analysis**：分析 gVisor 系统调用测试失败，对比 Linux/gVisor 参考实现，输出结构化修复文档
典型触发语："gVisor 测试挂了"、"分析这个 gVisor 失败用例"、"gVisor 测试失败帮我分析修复方案"

### 调试疑难问题
**dragonos-atomic-snapshot-debug**：调试内核时序问题、Heisenbug、阻塞挂起、丢唤醒、"加日志现象改变"的问题
典型触发语："任务卡住了/卡死了"、"CPU idle 但请求不返回"、"阻塞点偶发失效"、"在线取证"

该技能使用低扰动原子快照 + GDB 现场采样 + 语义对比，避免高频日志扰动时序。适用于网络、VFS、调度、IPC、驱动等子系统。
