:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/interrupt/tasklet.md

- Translation time: 2026-01-09 06:34:08

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

## Tasklet

A tasklet is a bottom-half mechanism based on softirq, used to execute lightweight, non-sleepable callback logic within a softirq context.

### Semantic Highlights

- Can be scheduled in hardirq/softirq/task context.  
- Only one instance of the same tasklet can execute at a time (self-serializing).  
- Repeated scheduling is deduplicated and will not enqueue indefinitely.  
- Callbacks run in a softirq context, where sleeping is not allowed.  

### Data Passing

The tasklet callback is abstracted through the `TaskletFunc` trait, with an equivalent function signature:

```rust
fn(usize, Option<Arc<dyn TaskletData>>)
```

- `usize` is suitable for simple values or indices.  
- `Option<Arc<dyn TaskletData>>` is suitable for complex data that requires safe sharing.  

`TaskletData` is constrained to `Send + Sync`, and is safely shared via `Arc` to avoid passing raw pointers.
