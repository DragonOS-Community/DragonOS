:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/core_api/softirq.md

- Translation time: 2025-05-19 01:41:38

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Soft Interrupt

&emsp;&emsp;Software interrupt, also known as the bottom half of an interrupt, is used to delay the processing of work that was not completed by the hard interrupt (the top half of the interrupt). Dividing the interrupt into two stages can effectively solve the problems of long interrupt handling time and interrupt loss.

## 1. Design Philosophy

&emsp;&emsp;Each CPU has its own pending status. Soft interrupts are "initiated by which CPU, executed by which CPU", and the pending status of each CPU is not shared. The same soft interrupt vector can run concurrently on multiple cores.

&emsp;&emsp;When we need to register a new soft interrupt, we need to implement the `SoftirqVec` feature for the soft interrupt handler, and then call the `register_softirq` function to register the soft interrupt handler within the soft interrupt mechanism.

&emsp;&emsp;Please note that due to the reentrancy and concurrency of soft interrupts, the soft interrupt handler must ensure thread safety itself.

## 2. Soft Interrupt Vector Number

```rust
pub enum SoftirqNumber {
    /// 时钟软中断信号
    TIMER = 0,
    /// 帧缓冲区刷新软中断
    VideoRefresh = 1, 
}
```

## 3. Soft Interrupt API

### 3.1. SoftirqVec Feature

```rust
pub trait SoftirqVec: Send + Sync + Debug {
    fn run(&self);
}
```

&emsp;&emsp;The feature that the soft interrupt handler needs to implement. It needs to implement the `run` function to handle the soft interrupt. When the soft interrupt is executed, the `run` function will be called.

### 3.2. Softirq API

#### 3.2.1. Register Soft Interrupt Vector

```rust
pub fn register_softirq(&self,
        softirq_num: SoftirqNumber,
        handler: Arc<dyn SoftirqVec>,
    ) -> Result<i32, SystemError>
```

- Parameters:

  - softirq_num: Interrupt vector number

  - handler: The structure corresponding to the interrupt function, which needs to point to a structure variable that implements the `SoftirqVec` feature

- Return:

  - Ok(i32): 0

  - Err(SystemError): Error code

#### 3.2.2. Unregister Soft Interrupt Vector

```rust
pub fn unregister_softirq(&self, softirq_num: SoftirqNumber)
```

- Parameters:

  - softirq_num: Interrupt vector number

#### 3.2.3. Execute Soft Interrupt

```rust
pub fn do_softirq(&self)
```

- Purpose: Execute the soft interrupt function (**only called after hard interrupt execution**)

#### 3.2.4. Clear the Pending Flag of Soft Interrupt

```rust
pub unsafe fn clear_softirq_pending(&self, softirq_num: SoftirqNumber)
```

- Purpose: Clear the pending flag of the specified soft interrupt on the current CPU. Please note that this function is unsafe because it directly modifies the pending flag without locking.

- Parameters:

  - softirq_num: Interrupt vector number

#### 3.2.5. Mark Soft Interrupt as to be Executed

```rust
pub fn raise_softirq(&self, softirq_num: SoftirqNumber)
```

- Purpose: Mark the specified soft interrupt as to be executed on the current CPU

- Parameters:

  - softirq_num: Interrupt vector number

### 3.3. Usage Example

```rust
#[derive(Debug)]
/// SoftirqExample中断结构体
pub struct SoftirqExample {
    running: AtomicBool,
}
/// SoftirqExample中断需要处理的逻辑
fn softirq_example_func() {
    println!("addressed SoftirqExample");
}
impl SoftirqVec for SoftirqExample {
    fn run(&self) {
        if self.set_run() == false {
            return;
        }

        softirq_example_func();

        self.clear_run();
    }
}
impl SoftirqExample {
    pub fn new() -> SoftirqExample {
        SoftirqExample {
            running: AtomicBool::new(false),
        }
    }

    fn set_run(&self) -> bool {
        let x = self
            .running
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed);
        if x.is_ok() {
            return true;
        } else {
            return false;
        }
    }

    fn clear_run(&self) {
        self.running.store(false, Ordering::Release);
    }
}
fn main() {
    let softirq_example = Arc::new(SoftirqExample::new());
    let softirq_num = 2;
    // 注册SoftirqExample中断
    softirq_vectors()
        .register_softirq(SoftirqNumber::from(softirq_num as u64), softirq_example)
        .expect("failed to register SoftirqExample");

    // 标志SoftirqExample中断需要执行
    softirq_vectors().raise_softirq(SoftirqNumber::from(softirq_num as u64));

    // 标志SoftirqExample中断不需要执行
    softirq_vectors().clear_softirq_pending(SoftirqNumber::from(softirq_num as u64));

    // 解注册SoftirqExample中断
    softirq_vectors().unregister_softirq(SoftirqNumber::from(softirq_num as u64));
}
```
