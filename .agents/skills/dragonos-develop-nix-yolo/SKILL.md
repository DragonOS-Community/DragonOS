---
name: dragonos-develop-nix-yolo-boot-check
description: 专用于按照 docs/introduction/develop_nix.md 的流程，通过 Nix dev shell / yolo 命令启动 DragonOS，并在 QEMU nographic 串口中做启动烟雾检查或实时轮询回贴输出。当用户要求“按 develop_nix 跑 yolo”“用 nix yolo 启动 QEMU 看输出”“边跑边轮询输出”“进 guest 后检查 /proc、/sys/fs/cgroup、mount 是否正常”时使用。
---

# DragonOS Develop Nix Yolo Boot Check

## 目标

按项目文档的推荐路径启动 DragonOS：

1. 走 `develop_nix` 对应的 Nix 环境
2. 运行 `nix run .#yolo-x86_64 -- -nographic`
3. 在 QEMU 串口里观察启动日志
4. 进入 guest shell 做最小烟雾检查
5. 把成功信号、失败点和原始报错带回给用户
6. 如果用户要求“边跑边看/实时输出”，持续轮询 PTY 并回贴最新输出块

## 何时使用

- 用户明确提到 `develop_nix`、`nix develop`、`yolo-x86_64`
- 用户要求“启动 DragonOS 看输出”
- 用户要求“进 QEMU 里手动检查”
- 内核改动后，需要快速确认系统是否还能完整启动到用户态

## 前置检查

1. 先读 `docs/introduction/develop_nix.md`，确认文档仍然推荐：
   - `nix develop`
   - `make kernel`
   - `nix run .#rootfs-x86_64`
   - `nix run .#start-x86_64`
   - 以及一键命令 `nix run .#yolo-x86_64`
2. 先看 `git status --short`，记住当前工作树是脏还是干净。
3. 确认当前用户可以访问 `/dev/kvm`（`[ -w /dev/kvm ]`）。整个流程不需要 `sudo`；`/dev/kvm` 不可写时脚本会提示并回退到 TCG（很慢），应先告诉用户加入 `kvm` 组。

## 推荐执行顺序

### 1) 用 PTY 启动 yolo

必须用带 TTY 的终端会话运行：

```bash
nix run .#yolo-x86_64 -- -nographic
```

要点：

- 必须使用交互式 PTY，否则后续无法和 QEMU 串口交互。
- 这条命令会顺序执行：
  - `make kernel`
  - 准备 rootfs 磁盘镜像（userland 无变化时输出 `Disk image unchanged, skip`）
  - 启动 QEMU（参数透传给 `dragonos-run`）
- 允许出现 host CPU / KVM feature warning，只要系统继续启动，不把这些 warning 当成失败。

### 2) 如果用户要求实时输出，持续轮询并回贴原始输出块

如果用户明确要求“边跑边轮询”“实时输出”“一边跑一边给我看”，不要只在最后总结；应在整个过程中轮询 PTY 并回贴最新输出。

要点：

- 这个界面不会自动把 PTY 原始流直接推给用户；需要手动轮询会话并回贴输出块。
- 活跃阶段（编译、rootfs 写盘、串口刷屏）可用 1-5 秒轮询；长时间静默构建阶段可放宽到 10-30 秒。
- 优先贴“最新一块原始输出 + 一句简短说明”，不要只做抽象总结。
- 对真正的错误、warning、panic、mount 失败，尽量保留原文，不要改写掉关键报错。
- 如果是大段宿主机构建日志，抓关键窗口即可；如果是 guest 串口异常，优先贴第一处异常附近的原始日志。
- 如果用户没有要求实时输出，仍然要在关键阶段给出简洁进度，但不必高频贴日志。

### 3) 观察启动阶段的关键成功信号

重点盯以下日志：

- `Kernel Build Done.`
- `Disk image unchanged, skip` 或 `Build complete!`
- `Step 3: Starting DragonOS...`
- `DragonOS release ...`
- `ProcFS mounted at /proc`
- `SysFS mounted.`
- `Cgroup2 mounted at /sys/fs/cgroup`
- `Successfully migrate rootfs to ext4!`
- `Boot with specified init process`
- `root@dragonos:~#` 或等价 shell prompt

如果看到 panic、`init` 启动失败、mount 失败、卡死在早期阶段，要把原始日志摘出来。

### 4) 激活 guest 控制台

有些镜像会提示：

```text
Please press Enter to activate this console.
```

这时向 PTY 写入一个换行：

```text
\n
```

直到出现 shell prompt。

### 5) guest 内最小烟雾检查

默认执行下面几条，逐条记录结果：

```bash
cat /proc/self/cgroup
cat /proc/mounts | grep cgroup
ls /sys/fs/cgroup
```

如果这次任务和 cgroup/mount 相关，再补：

```bash
mkdir /sys/fs/cgroup/testcg
ls /sys/fs/cgroup/testcg
cat /sys/fs/cgroup/testcg/cgroup.procs
```

注意：

- 如果你在通过 PTY/串口逐条喂命令，优先“一次写一条命令，等 prompt 回来后再发下一条”。不要把多条命令一次性塞给 guest，否则串口可能吞字、错行或把后续命令打坏。
- 不要把“命令执行了”误写成“功能通过了”。
- 如果写文件时报 `Function not implemented`、`Permission denied`、`No such file or directory`，要原样记录。
- 如果 shell 卡住，先看是不是命令本身阻塞，不要立即判成内核 panic。

### 6) 退出 QEMU

在 nographic 模式下，退出序列是：

```text
Ctrl+A 然后 x
```

向 PTY 写入：

```text
\u0001x
```

## 默认报告格式

```markdown
## Develop Nix Yolo Boot Check

### 宿主机阶段
- 是否成功进入 Nix 路径
- 是否成功完成 kernel / rootfs / disk image / QEMU 启动

### QEMU 启动结果
- 是否进入用户态 shell
- 关键日志

### Guest 烟雾检查
- `cat /proc/self/cgroup` => ...
- `cat /proc/mounts | grep cgroup` => ...
- `ls /sys/fs/cgroup` => ...
- 额外检查 => ...

### 结论
- 启动是否通过
- 哪些子路径通过
- 哪些子路径失败，原始错误是什么
```

如果用户要求实时输出，可在过程中的每一轮更新里采用：

````markdown
最新输出块：

```text
...
```

一句话说明当前阶段 / 下一步。
````

## 失败处理

- 如果出现 `/dev/kvm is not writable` 或 vsock 相关 WARN：属于宿主机权限/模块配置问题，不是内核回归。
- 如果失败发生在 `make kernel`：返回编译错误摘要和首个真正报错点。
- 如果失败发生在 `rootfs` / 磁盘镜像：返回宿主机构建错误，不要误判为 guest 启动失败。
- 如果失败发生在 QEMU 内：优先保留串口日志里的第一处异常。

## 边界约束

- 默认使用 `x86_64`，除非用户明确指定其他架构。
- 默认遵循 `docs/introduction/develop_nix.md`，不要擅自切回旧的非 Nix 路径。
- 如果只是做“能否编译”的快速检查，优先 `nix develop -c make kernel`；只有用户要求真实启动或需要 guest 内验证时才走 yolo。
