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
3. 如果写磁盘镜像会触发 `sudo`，而用户已经给了密码，可以先预热 sudo；如果没有给密码，要先向用户说明会卡在提权步骤。
4. 注意 `yolo` 的 `rootfs`/QEMU 阶段都可能在较晚时再次调用 `sudo`。单次 `sudo -v` 可能在长时间依赖下载或 rootfs 构建后过期，不要假设一次预热就够。

## 推荐执行顺序

### 1) 预热 sudo，并优先使用 keepalive

如果用户已经提供密码，优先在和 `yolo` 相同的 PTY 会话里维持 sudo 时间戳：

```bash
sudo -v
while true; do sudo -n true; sleep 60; done 2>/dev/null &
keeper=$!
trap 'kill $keeper' EXIT
nix run .#yolo-x86_64 -- -nographic
```

如果只是单独预热：

```bash
printf '%s\n' "$PASSWORD" | sudo -S -v
```

也不要把它当成足够稳妥的方案，因为 `rootfs` 写盘和 `start-x86_64` 往往会在较晚阶段再次触发 `sudo`。

如果没有密码，不要假设；直接告诉用户这一步会阻塞在提权提示。

如果在沙箱里看到类似：

```text
sudo: The "no new privileges" flag is set
```

这说明是宿主机/沙箱权限边界，不是 DragonOS 回归；应改为在沙箱外执行同一命令。

### 2) 用 PTY 启动 yolo

必须用带 TTY 的终端会话运行：

```bash
nix run .#yolo-x86_64 -- -nographic
```

要点：

- 必须使用交互式 PTY，否则后续无法和 QEMU 串口交互。
- 如果用户提供了密码且预期会构建较久，优先使用“同 PTY 的 sudo keepalive + yolo”这一条组合命令，而不是裸跑 `nix run .#yolo-x86_64 -- -nographic`。
- 这条命令会顺序执行：
  - `make kernel`
  - `nix run .#rootfs-x86_64`
  - `nix run .#start-x86_64 -- -nographic`
- 允许出现 host CPU / KVM feature warning，只要系统继续启动，不把这些 warning 当成失败。

### 3) 如果用户要求实时输出，持续轮询并回贴原始输出块

如果用户明确要求“边跑边轮询”“实时输出”“一边跑一边给我看”，不要只在最后总结；应在整个过程中轮询 PTY 并回贴最新输出。

要点：

- 这个界面不会自动把 PTY 原始流直接推给用户；需要手动轮询会话并回贴输出块。
- 活跃阶段（编译、rootfs 写盘、串口刷屏）可用 1-5 秒轮询；长时间静默构建阶段可放宽到 10-30 秒。
- 优先贴“最新一块原始输出 + 一句简短说明”，不要只做抽象总结。
- 对真正的错误、warning、panic、mount 失败，尽量保留原文，不要改写掉关键报错。
- 如果是大段宿主机构建日志，抓关键窗口即可；如果是 guest 串口异常，优先贴第一处异常附近的原始日志。
- 如果用户没有要求实时输出，仍然要在关键阶段给出简洁进度，但不必高频贴日志。

### 4) 观察启动阶段的关键成功信号

重点盯以下日志：

- `Kernel Build Done.`
- `Build complete!`
- `Step 3: Starting DragonOS...`
- `DragonOS release ...`
- `ProcFS mounted at /proc`
- `SysFS mounted.`
- `Cgroup2 mounted at /sys/fs/cgroup`
- `Successfully migrate rootfs to ext4!`
- `Boot with specified init process`
- `root@dragonos:~#` 或等价 shell prompt

如果看到 panic、`init` 启动失败、mount 失败、卡死在早期阶段，要把原始日志摘出来。

### 5) 激活 guest 控制台

有些镜像会提示：

```text
Please press Enter to activate this console.
```

这时向 PTY 写入一个换行：

```text
\n
```

直到出现 shell prompt。

### 6) guest 内最小烟雾检查

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

### 7) 退出 QEMU

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

- 如果失败发生在 `sudo` 提权：明确说明是宿主机权限问题，不是内核回归。
- 如果失败是 `sudo: timed out reading password`，且位置在 rootfs 写盘或 QEMU 启动之前/之中，优先判定为“长流程中的 sudo 交互超时”；应改为“同 PTY keepalive + 重跑”，不要误判成 rootfs 或内核 bug。
- 如果失败发生在 `make kernel`：返回编译错误摘要和首个真正报错点。
- 如果失败发生在 `rootfs` / 磁盘镜像：返回宿主机构建错误，不要误判为 guest 启动失败。
- 如果失败发生在 QEMU 内：优先保留串口日志里的第一处异常。

## 边界约束

- 默认使用 `x86_64`，除非用户明确指定其他架构。
- 默认遵循 `docs/introduction/develop_nix.md`，不要擅自切回旧的非 Nix 路径。
- 如果只是做“能否编译”的快速检查，优先 `nix develop -c make kernel`；只有用户要求真实启动或需要 guest 内验证时才走 yolo。
