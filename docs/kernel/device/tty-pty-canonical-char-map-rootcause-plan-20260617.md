# PTY canonical read hang 根因定位与修复计划

## 背景

PR #1981 启用 `normal/tty_pty_hangup` 后，DragonOS guest 在
`TtyPtyHangup.CanonicalReaderDoesNotMissLineWakeup` 中挂起。宿主 Linux 上同一
gtest 用例可通过，因此问题位于 DragonOS 内核行为，而不是 CubeSandbox 或测试程序
本身。

该用例的关键链路是：

1. `openpty()` 创建 pty master/slave；
2. 测试把 slave 端 termios 设置为 `ICANON`，并关闭 `ECHO`；
3. child 通过匿名 pipe 告诉 parent 自己已经准备读 slave；
4. parent 向 pty master 写入 `"line\n"`；
5. child 应从 slave canonical read 返回 `"line\n"`。

## 已证伪方向

### CubeSandbox

本地 DragonOS QEMU guest 中直接运行 dunitest 可复现，不依赖 CubeSandbox。

### 通用匿名 pipe/waitqueue

为隔离 ready pipe 链路，新增最小 dunitest：

- `normal/pipe_waitqueue_wakeup`

该用例保留 “父进程阻塞读 pipe，子进程写 ready 字节后继续阻塞” 的同步模型，但移除
PTY、termios 与 gtest runner 输出收集。它已在宿主 Linux 与 DragonOS guest 中通过。

进一步的 syscall 临时 trace 显示：

- child 的 `write(ready_pipe[1], "r", 1)` 成功返回；
- parent 的第一次 `read(ready_pipe[0], ..., 1)` 成功返回；
- parent 随后 `write(ptmx, "line\n", 5)` 成功返回；
- child 卡在 `read(pts, ..., 32)`；
- parent 卡在第二轮 `read(ready_pipe[0], ..., 1)`，原因是 child 没有完成第一轮
  slave read 并进入下一轮 ready write。

因此失败不是 ready pipe 没唤醒，也不是通用 waitqueue/scheduler 丢唤醒。

## 当前现场证据

临时 N_TTY trace 捕获到 parent 写入 pty master 后的数据流：

```text
TTY_HANG_LDISC recv enter pid=20 tty=pts0 count=5 bytes=[108, 105, 110, 101, 10] icanon=true head=0 canon=0 tail=0
TTY_HANG_LDISC recv exit  pid=20 tty=pts0 recved=5 icanon=true head=5 canon=0 tail=0
TTY_HANG_LDISC read sleep pid=21 tty=pts0 icanon=true head=5 canon=0 tail=0
```

这说明：

- pty master 到 slave 的数据路由正确；
- slave line discipline 已处于 `ICANON`；
- 五个字节已经进入 `read_buf`，`read_head` 从 0 前进到 5；
- `canon_head` 仍为 0，说明换行没有被当作 canonical line delimiter 提交；
- child read 的等待条件仍不满足，因此继续睡眠。

根因边界已经收敛到 N_TTY canonical receive 的特殊字符识别。

## Linux 6.6 参考语义

Linux `drivers/tty/n_tty.c` 的关键逻辑：

- `n_tty_set_termios()` 在以下任一模式开启时重建 `ldata->char_map`：
  - `ISTRIP`
  - `IUCLC`
  - `IGNCR`
  - `ICRNL`
  - `INLCR`
  - `ICANON`
  - `IXON`
  - `ISIG`
  - `ECHO`
  - `PARMRK`
- 在 `ICANON` 模式下，它显式把 `'\n'` 加入 `char_map`；
- 最后清除 `__DISABLED_CHAR` 位；
- 非 raw 接收路径只有 `char_map` 命中的字符才会进入 special path；
- `'\n'` 在 `n_tty_receive_char_canon()` 中调用
  `n_tty_receive_handle_newline()`，设置 line flag、推进 `canon_head`，并唤醒
  `read_wait`。

因此 Linux 语义要求：即使 slave 只开启 `ICANON`、不开 `ECHO/ISIG/IXON`，换行也
必须被加入 `char_map` 并触发 canonical line 提交。

## DragonOS 根因

DragonOS `NTtyLinediscipline::set_termios()` 中，决定是否重建 `char_map` 的条件漏掉
了 Linux 中的三个关键输入：

- `InputMode::ICRNL`
- `InputMode::INLCR`
- `LocalMode::ICANON`

当前条件只在 `ISTRIP/IUCLC/IGNCR/IXON/ISIG/ECHO/PARMRK` 等模式开启时才进入非 raw
派生状态重建分支。测试中的 pty slave 初始 termios 比较接近 raw，随后只打开
`ICANON` 并关闭 `ECHO`。由于条件漏掉 `ICANON`，N_TTY 派生状态没有按 canonical
模式重建，`char_map` 不会包含 `'\n'`。

接收路径随后执行：

- 在非 raw canonical 接收路径中，`receive_buf_standard()` 查询 `char_map['\n']`；
- 因为该位为 false，`'\n'` 会走普通字符接收路径；
- 普通路径只推进 `read_head`，不会推进 `canon_head`；
- canonical read 看到 `canon_head == read_tail`，继续睡眠。

此外，DragonOS 当前把 `ControlCharIndex::DISABLE_CHAR` 在 `char_map` 中置为 true，而
Linux 在重建结束后会清除 disabled char 位。这个差异会让值为 disabled marker 的
控制字符被误认为特殊字符，也应一并修正。

## 修复计划

1. 在 `kernel/src/driver/tty/tty_ldisc/ntty.rs` 的 `set_termios()` 中，对齐 Linux
   `n_tty_set_termios()` 的 `char_map` 重建条件：
   - 增加 `InputMode::ICRNL`；
   - 增加 `InputMode::INLCR`；
   - 增加 `LocalMode::ICANON`。
2. 在 `char_map` 构建完成后，把 disabled char 位清除，而不是置为 true：
   - `ControlCharIndex::DISABLE_CHAR` 不能作为普通特殊字符触发路径；
   - EOF 在 canonical special path 中转换成 disabled marker 并提交一行，该转换不依赖
     disabled marker 自身命中 `char_map`；
   - `VEOL/VEOL2` 仍按各自控制字符命中 `char_map` 并作为 line delimiter 处理。
3. 清理所有临时 trace：
   - `kernel/src/syscall/mod.rs` 的 syscall trace；
   - `kernel/src/driver/tty/tty_ldisc/ntty.rs` 的 N_TTY trace；
   - 不保留全局高频日志或测试特化代码。
4. 每次 `set_termios()` 都按当前 `ECHO` 位更新 `ldata.echo`，避免关闭 ECHO 后派生状态
   陈旧。
5. 保留 `normal/pipe_waitqueue_wakeup` 作为已证伪通用 pipe/waitqueue 问题的回归覆盖；
   它不参与修复 PTY bug，但能防止后续误判 ready pipe 链路。
6. 保留并运行 `normal/tty_pty_hangup`，确保 canonical read 与 pty hangup 语义恢复。

本计划只修复当前证据指向的 `char_map` 与 N_TTY 派生状态问题，不声称完整补齐 Linux
`n_tty_set_termios()` 的所有行为。例如 Linux 在清除 `IXON` 时会 `start_tty()` 并处理
echoes，DragonOS 当前仍有对应 TODO；该问题不阻塞本次 canonical read hang 修复，应在
独立问题中继续对齐。

## 设计审查点

- 修复点位于 N_TTY line discipline 的 termios 派生状态维护，职责正确；
- 不修改 pty master/slave 数据路由，因为现场证明数据已经正确进入 `pts0`；
- 不修改 waitqueue/scheduler，因为最小 pipe 用例与 syscall trace 已证伪该方向；
- 不增加延时、重试、额外 wakeup 或测试专用逻辑；
- 对齐 Linux 6.6 的 `char_map` 构建语义，属于根因修复；
- disabled char 清除是 Linux 语义的一部分，避免引入新的控制字符误判。
- `ldata.echo` 按 termios 当前值覆盖更新，避免 termios 切换后使用过期派生状态。

## 验证计划

本地宿主侧：

1. `make -C user/apps/tests/dunitest bin/normal/tty_pty_hangup_test`
2. `make -C user/apps/tests/dunitest bin/normal/pipe_waitqueue_wakeup_test`
3. 在宿主 Linux 运行两个二进制，确认用户态测试语义本身可通过。

DragonOS 构建：

1. `make kernel`
2. 启动 DragonOS QEMU guest，确认 banner 是新构建内核。

DragonOS guest 内验证：

1. `/opt/tests/dunitest/bin/normal/tty_pty_hangup_test`
2. `/opt/tests/dunitest/bin/normal/pipe_waitqueue_wakeup_test`
3. `/opt/tests/dunitest/bin/normal/pipe_release_wakeup_test`

若 `tty_pty_hangup_test` 仍失败，需要回到 N_TTY canonical read 路径继续检查：

- `char_map['\n']` 是否已设置；
- newline special path 是否推进 `canon_head`；
- `read_wq` 是否唤醒；
- `input_available()` 是否与 canonical 提交条件一致。

## 回归风险

- raw/real_raw 模式：新增 `ICANON` 条件只会让 canonical 模式进入非 raw 分支，符合
  Linux 语义；
- `ICRNL/INLCR`：新增条件让 CR/LF 映射在非 canonical 模式也走 special path，符合
  Linux 语义；
- disabled char：从置位改为清位可能改变当前错误行为，但目标是匹配 Linux，避免被禁用
  的控制字符误触发 special path。
