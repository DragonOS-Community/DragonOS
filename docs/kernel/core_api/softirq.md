# 软中断

&emsp;&emsp;软件中断，也可以被称为中断的下半部，用于延迟处理硬中断（中断上半部）未完成的工作。将中断分为两个阶段可以有效解决中断处理时间过长和中断丢失的问题。

## 1. 设计思路

&emsp;&emsp;每个cpu都有自己的pending，软中断是“哪个cpu发起，就哪个cpu执行”，每个cpu的pending不共享。同一个软中断向量可以在多核上同时运行。

&emsp;&emsp;当我们需要注册一个新的软中断时，需要为软中断处理程序实现`SoftirqVec`特征，然后调用`register_softirq`函数，将软中断处理程序注册到软中断机制内。

&emsp;&emsp;请注意，由于软中断的可重入、可并发性，所以软中断处理程序需要自己保证线程安全。

## 2. 软中断向量号

```rust
pub enum SoftirqNumber {
    /// 时钟软中断信号
    TIMER = 0,
    /// 帧缓冲区刷新软中断
    VideoRefresh = 1, 
}
```

## 3. 软中断API

### 3.1. SoftirqVec特征

```rust
pub trait SoftirqVec: Send + Sync + Debug {
    fn run(&self);
}
```

&emsp;&emsp;软中断处理程序需要实现的特征，需要实现`run`函数，用于处理软中断。当软中断被执行时，会调用`run`函数。

### 3.2. Softirq的API

#### 3.2.1. 注册软中断向量
```rust
pub fn register_softirq(&self,
        softirq_num: SoftirqNumber,
        handler: Arc<dyn SoftirqVec>,
    ) -> Result<i32, SystemError>
```

- 参数：
  
  - softirq_num：中断向量号
  
  - hanlder：中断函数对应的结构体，需要指向实现了`SoftirqVec`特征的结构体变量

- 返回：
  
  - Ok(i32)：0
  
  - Err(SystemError)：错误码

#### 3.2.2. 解注册软中断向量

```rust
pub fn unregister_softirq(&self, softirq_num: SoftirqNumber)
```

- 参数：
  
  - softirq_num：中断向量号


#### 3.2.3. 软中断执行

```rust
pub fn do_softirq(&self)
```

- 作用：执行软中断函数（**只在硬中断执行后调用**）

#### 3.2.4. 清除软中断的pending标志

```rust
pub unsafe fn clear_softirq_pending(&self, softirq_num: SoftirqNumber)
```

- 作用：清除当前CPU上，指定软中断的pending标志。请注意，这个函数是unsafe的，因为它会直接修改pending标志，而没有加锁。

- 参数：
  
  - softirq_num：中断向量号

#### 3.2.5. 标志软中断需要执行

```rust
pub fn raise_softirq(&self, softirq_num: SoftirqNumber)
```

- 作用：标志当前CPU上，指定的软中断需要执行

- 参数：
  
  - softirq_num：中断向量号

### 3.3. 使用实例

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

### 3.4. 为C提供的接口

```c
extern void rs_softirq_init();
extern void rs_raise_softirq(uint32_t sirq_num);
extern void rs_unregister_softirq(uint32_t sirq_num);
extern void rs_do_softirq();
extern void rs_clear_softirq_pending(uint32_t softirq_num);
```
