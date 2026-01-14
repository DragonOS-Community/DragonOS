## Tasklet

Tasklet 是一种基于 softirq 的 bottom-half 机制，用于在软中断上下文中执行轻量级、不可睡眠的回调逻辑。

### 语义要点

- 可在 hardirq/softirq/task context 调度。
- 同一个 tasklet 同时只会有一个执行实例（自串行）。
- 重复 schedule 会去重，不会无限入队。
- 回调运行在 softirq 上下文，不允许睡眠。

### 数据传递

Tasklet 的回调通过 `TaskletFunc` trait 进行抽象，等价的函数签名为：

```rust
fn(usize, Option<Arc<dyn TaskletData>>)
```

- `usize` 适用于简单数值或索引。
- `Option<Arc<dyn TaskletData>>` 适用于需要安全共享的复杂数据。

`TaskletData` 约束为 `Send + Sync`，通过 `Arc` 安全共享，避免传入裸指针。
