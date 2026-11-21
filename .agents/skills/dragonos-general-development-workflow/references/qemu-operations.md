# QEMU Nographic 模式运行 DragonOS

## 目录

- [命令选择](#命令选择)
- [成功信号](#成功信号)
- [交互流程](#交互流程)
- [命令投递规则](#命令投递规则)
- [sudo 处理](#sudo-处理)
- [失败分类](#失败分类)
- [实用注意事项](#实用注意事项)

## 命令选择

- 当用户需要正常运行路径时使用 `make run-nographic`：根据需要重建内核/用户程序，更新 rootfs/磁盘镜像，然后启动 QEMU。
- 当用户明确要求**跳过**重建内核且避免更新 rootfs，或已有最新镜像且任务仅需启动/交互时，使用 `make qemu-nographic`。
- 从 DragonOS 仓库根目录运行这些命令。

> 如果是 nix 用户，可以用 `nix run .#start-x86_64` 来等效替代 `make qemu-nographic`，用 `make kernel && nix run .#start-x86_64` 简单替代 `make run-nographic`（如需更新rootfs 需要先执行 `nix run .#rootfs-x86_64`）。
> 如需更多详情 先读 `docs/introduction/develop_nix.md`，确认文档仍然推荐使用以上命令。

## 成功信号

启动过程中重点盯以下日志，逐条确认：

- `Kernel Build Done.`
- `DragonOS release ...`
- `Successfully migrate rootfs to ext4!`
- `Boot with specified init process`
- `Please press Enter to activate this console.`
- `root@dragonos:~#` 或等价 shell prompt

如果看到 panic、init 启动失败、mount 失败、卡死在早期阶段，要把原始日志摘出来，而不是一直等待。

## 交互流程

1. 在支持 TTY 的终端会话中使用上述命令之一启动 QEMU。
   - 必须使用交互式 PTY，否则后续无法和 QEMU 串口交互。
2. 等待启动日志到达 `Please press Enter to activate this console.`。
3. 向 PTY 写入换行符 `\n` 激活控制台（等同于按 Enter），等待 `root@dragonos:~#` prompt 出现。
4. 逐条输入命令（见下方[命令投递规则](#命令投递规则)）。快速验证命令：

   ```sh
   pwd
   uname -a
   ls /
   ```

5. 如果在目标模式下验证 dunitest 或 gVisor 测例时，在 guest 内运行相关的已安装测试二进制：

   ```sh
   # dunitest
   cd /opt/tests/dunitest/bin/normal/
   ./xxx_test
   ```
   ```
   # gVisor
   cd /opt/gvisor/tests/
   ./xxx_test
   # 如果是特定测试的某些部分，可以通过正则表达式匹配，如：
   ./xxx_test --gtest_filter=*Process*:*Pid*
   ```

   将 `xxx_test` 替换为变更对应的具体测试二进制，例如 `kill_test`。

6. 向 PTY 写入 `\x01x`（即 按 Ctrl-A 然后跟着按 x）退出 QEMU nographic 模式。

## 命令投递规则

- 一次只写一条命令，等 `root@dragonos:~#` prompt 回来后再发下一条。
- 不要把多条命令一次性塞给 guest（串口会吞字、错行、把后续命令打坏）。
- 不要把"命令执行了"误写成"功能通过了"——执行成功和结果正确是两回事。
- 如果 shell 卡住，先判断是否命令本身阻塞（如 `cat` 无输入等待），不要立即判成死锁。
- 如果写文件时报 `Function not implemented`、`Permission denied`、`No such file or directory`，要原样记录。

## sudo 处理

- `make qemu-nographic` 会直接使用现有的 `bin/kernel/kernel.elf` 和 `bin/disk-image-x86_64.img`，无需sudo。
- `make run-nographic` 可能需要宿主机权限，因为镜像更新路径会挂载/写入磁盘镜像。
- 若不是 root 用户，可以向用户索要密码。如果用户已经提供密码，用下面命令预热 sudo：

  ```bash
  printf '%s\n' "$PASSWORD" | sudo -S -v
  ```

  在同一个 PTY 会话中紧接着运行 `make run-nographic`，确保 sudo 时间戳在写盘阶段仍有效。

- 如果写盘耗时特别长（如首次构建大型 rootfs），sudo 可能过期。此时应告知用户，而不是自动启动 keepalive 循环。
- 如果看到 `sudo: The "no new privileges" flag is set`，说明是宿主机/沙箱权限边界，不是 DragonOS 回归；应改为在沙箱外执行同一命令。

## 失败分类

失败可能发生在不同阶段，不要跨阶段误判：

| 失败阶段 | 典型症状 | 正确判断 | 常见误判 |
|---|---|---|---|
| **编译** (make kernel) | `error[E...]`、linker error | 编译错误，返回首个真正报错点 | ❌ 不要误判为内核运行时 bug |
| **写盘** (rootfs/disk image) | `cp: cannot stat ...`、`write_diskimage` 失败、`Bad message` | 宿主机文件系统/镜像构建问题 | ❌ 不要误判为 guest 启动失败 |
| **sudo 提权** | `sudo: timed out`、`Permission denied` | 宿主机权限问题 | ❌ 不要误判为内核回归 |
| **QEMU 启动** | 串口无输出、kernel panic、卡在早期启动 | 内核/启动问题，保留串口日志第一处异常 | ❌ 不要误判为编译错误或未使用新编译的内核 |
| **guest 运行** | 命令返回错误、测试失败、shell 卡住 | guest 内功能问题，原样记录错误信息 | ❌ 不要把"命令执行了"当成"功能通过了" |

## 实用注意事项

- Nographic 串口输出也会写入 DragonOS 仓库根目录的 `serial_opt.txt`。当终端输出过长或有溢出上下文的风险时，使用有针对性的 `rg`/`grep`/`tail` 命令检查该文件，而不是读取整个终端缓冲区。

  ```sh
  rg -n "Please press Enter|root@dragonos|ERROR|WARN|panic|Boot with specified init" serial_opt.txt
  tail -n 200 serial_opt.txt
  ```

- 如果 `make run-nographic` 在写入磁盘镜像时失败，记录实际错误。纯交互测试的有效回退方案是 `make qemu-nographic`，但不要将其视为 rootfs 更新测试。
- 已观察到的一种非权限失败情况：在成功构建内核后，在 `bin/mnt/disk-image-x86_64` 下反复出现 `cp: cannot stat '.../bin/...': Bad message`，随后 `write_diskimage` 失败。在这种情况下，`qemu-nographic` 可能仍能用现有镜像启动，但完整的镜像更新路径未成功。
- 测试后避免在后台留下 QEMU 运行。始终用 `\x01x` 关闭，除非用户要求保持会话打开。
