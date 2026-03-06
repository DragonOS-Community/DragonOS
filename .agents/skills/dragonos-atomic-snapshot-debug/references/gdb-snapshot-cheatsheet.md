# GDB Snapshot Cheatsheet

## 1. 确认当前实例

先确认你连的是当前正在跑的内核，而不是上一个挂住实例：

1. 查看终端 banner 中的版本号和构建时间
2. 读取当前 GDB 端口文件
3. 再去连接 GDB

示例：

```bash
read bin/vmstate/gdb
```

```bash
rust-gdb -n bin/kernel/kernel.elf -batch \
  -ex 'target remote localhost:12581' \
  -ex 'interrupt' \
  -ex 'info threads'
```

## 2. 定位快照符号

使用 `nm -C` 先找出原子快照/计数器的地址：

```bash
nm -C "bin/kernel/kernel.elf" | rg "DRAGONOS_(TCP_DEBUG_(SEND|RECV)_WAIT|LOOPBACK_DEBUG|NAPI_DEBUG)_"
```

如果只想看某一层：

```bash
nm -C "bin/kernel/kernel.elf" | rg "DRAGONOS_TCP_DEBUG_"
nm -C "bin/kernel/kernel.elf" | rg "DRAGONOS_LOOPBACK_DEBUG_"
nm -C "bin/kernel/kernel.elf" | rg "DRAGONOS_NAPI_DEBUG_"
```

## 3. 先看 CPU 在做什么

```bash
rust-gdb -n bin/kernel/kernel.elf -batch \
  -ex 'target remote localhost:12581' \
  -ex 'interrupt' \
  -ex 'set pagination off' \
  -ex 'info threads' \
  -ex 'thread 1' \
  -ex 'bt 6' \
  -ex 'thread 2' \
  -ex 'bt 6'
```

如果两个 CPU 都停在 `arch_idle_func()`，优先考虑“等待者全睡了，但最后一次推进/唤醒没发生”。

## 4. 读取 TCP wait 快照

如果某个子系统有成组的 wait 快照，拿到符号地址后按连续内存块读取：

```bash
rust-gdb -n bin/kernel/kernel.elf -batch \
  -ex 'target remote localhost:12581' \
  -ex 'interrupt' \
  -ex 'x/14gx <tcp-recv-wait-base-addr>' \
  -ex 'x/2gx <tcp-recv-state-addr>'
```

发送侧同理：

```bash
rust-gdb -n bin/kernel/kernel.elf -batch \
  -ex 'target remote localhost:12581' \
  -ex 'interrupt' \
  -ex 'x/10gx <tcp-send-wait-base-addr>' \
  -ex 'x/2gx <tcp-send-state-addr>'
```

建议记录这些字段：

- `ACTIVE`
- `PID`
- `TOTAL`
- `TARGET`
- `EVENTS`
- `CAN_SEND`
- `CAN_RECV`
- `MAY_RECV`
- `SEND_QUEUE`
- `RECV_QUEUE`
- `POLL_AT_US`

## 5. 读取下层计数器

对任意下层子系统，只要有一组连续的计数器，就可以统一读取：

```bash
rust-gdb -n bin/kernel/kernel.elf -batch \
  -ex 'target remote localhost:12581' \
  -ex 'interrupt' \
  -ex 'x/8gx <subsystem-debug-base-addr>'
```

优先看这些类型的计数：

- `produce_count`
- `consume_count`
- `wake_count`
- `sleep_count`
- `queue_len`
- `max_queue_len`
- `retry_count`
- `deadline/timer`

判读：

- `produce_count > consume_count` 或 `queue_len > 0`：事件已经产生，但没被完全消费
- `produce_count == consume_count`：不能证明没问题，只能说明源/汇之间表面平衡，需要继续往更高层看

## 6. 读取调度/worker 计数

对于 worker、workqueue、softirq、poller、NAPI 一类“负责继续推进”的执行者，重点看调度行为：

```bash
rust-gdb -n bin/kernel/kernel.elf -batch \
  -ex 'target remote localhost:12581' \
  -ex 'interrupt' \
  -ex 'x/8gx <worker-debug-base-addr>'
```

重点看：

- `schedule_count`
- `run_count` / `poll_count`
- `WAKE_COUNT`
- `sleep_count`
- `requeue_count`
- `global_recheck_count`

判读：

- `schedule_count` 持续增长但 `run_count` 不动：worker 可能没真的继续跑
- `run_count` 在增长但等待者仍卡住：问题可能已经上移到更高层语义
- `sleep_count` 增长过快：可能过早判定“无事可做”

## 7. 通用判读模板

### 模式 A：等待者睡住，但源事件不再增长

- `WAIT_ACTIVE=1`
- 下层生产计数不再增长
- CPU 全 idle

含义：

- 真正睡住的是等待者
- 没有新的生产者/推进者在运行
- 优先检查丢唤醒、timer 未触发、worker 未被重新调度

### 模式 B：事件已经产生，但没被消费干净

- `produce_count > consume_count`
- 或 `queue_len > 0`
- 等待者仍睡着

含义：

- 问题在事件交接或继续推进条件
- 优先检查：
  - “还有工作”的返回值语义
  - requeue/retry/continue 条件
  - 可见性与状态位更新顺序

### 模式 C：下层计数看起来健康，但上层调用仍错误阻塞

- 生产/消费计数基本匹配
- worker 运行正常
- 等待者在新的固定 chunk/轮次/重试上睡住

含义：

- 更像高层语义错误，而不是下层推进丢失
- 常见方向：
  - 一次 syscall 被错误拆成多个底层阻塞操作
  - chunking/retry 改坏了返回语义
  - 唤醒条件与返回条件不一致

## 8. 经验法则

1. 先抓现场，再改代码。
2. 先加原子，再加日志。
3. 先看 wait 点，再看下层计数。
4. 一旦下层计数正常，就怀疑更高层语义，而不是继续堆某个具体子系统补丁。
