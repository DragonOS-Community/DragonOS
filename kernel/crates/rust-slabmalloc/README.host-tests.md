## rust-slabmalloc：Linux 主机测试体系

该目录下的 `slabmalloc` 是 **no_std** 库，但在 Linux 主机上我们可以用 `cargo test` 跑单元测试/属性测试，并用一个独立的压力工具配合 Valgrind/Miri 做长稳与 UB 检查。

### 快速开始

在本 crate 目录运行：

```bash
cd /home/jinlong/code/DragonOS/kernel/crates/rust-slabmalloc
cargo test
```

### 单元测试（Unit tests）

单元测试位于 `src/tests.rs`，覆盖：

- 基础分配/释放正确性
- `ObjectPage` 布局与位图行为
- 页链表迁移不变量（full/partial/empty 即时迁移）

### 属性/随机测试（proptest）

属性测试位于 `src/prop_tests.rs`，使用 `proptest` 生成随机 alloc/free 序列，验证：

- 反复 alloc/free 不崩溃
- 最终可回收所有 empty 页到 pager（不泄漏）

运行：

```bash
cargo test prop_
```

如需更“啰嗦”的 proptest 输出：

```bash
PROPTEST_VERBOSE=1 cargo test prop_
```

### 压测/长稳（slab_stress）

该工具是一个 host 可执行程序：`src/bin/slab_stress.rs`

编译并运行（release）：

```bash
cargo run --release --features host --bin slab_stress -- --iters 500000 --max-live 4096 --size 64 --seed 1
```

### Valgrind

先编译 release：

```bash
cargo build --release --features host --bin slab_stress
```

再用 Valgrind 跑：

```bash
valgrind \
  -s \
  --error-exitcode=1 \
  --track-origins=yes \
  --leak-check=full \
  --show-leak-kinds=all \
  --malloc-fill=0xAA --free-fill=0xDD \
  ../../target/release/slab_stress --iters 200000 --size 64 --seed 1
```

说明：如果你看到 `still reachable`（比如来自 Rust std 的线程信息），这通常不是越界/泄漏错误；真正的越界会体现在 `ERROR SUMMARY` 或 `Invalid read/write` 报告里。


