# 软中断

&emsp;&emsp;软件中断，也可以被称为中断的下半部，用于延迟处理硬中断（中断上半部）未完成的工作。将中断分为两个阶段可以有效解决中断处理时间过长和中断丢失的问题。

## 设计思路

&emsp;&emsp;每个cpu都有自己的pending，软中断是“哪个cpu发起，就哪个cpu执行”，vectors可以在每个cpu上跑，并且每个vector都可以并行的在多个cpu上跑，因为每个cpu的pending不共享。每个cpu的pending存储在`__CPU_PENDING`中。vectors存储在`__SORTIRQ_VECTORS`指向的`Softirq`结构体中。

## 软中断向量号

```rust
pub enum SoftirqNumber {
    /// 时钟软中断信号
    TIMER = 0,
    /// 帧缓冲区刷新软中断
    VideoRefresh = 1, 
}
```

## 软中断API

```rust
pub fn softirq_vectors() -> &'static mut Softirq 
```

- 作用：获取中断向量表的静态可变引用

- 返回：中断向量表的静态可变引用

```rust
fn cpu_pending(cpu_id: usize) -> &'static mut VecStatus
```

- 作用：获取对应cpu的pending

- 参数：
  
  - cpu_id：cpu的id

- 返回：对应cpu的pending的静态可变引用

### Softirq的API

```rust
pub fn register_softirq(&self,
        softirq_num: SoftirqNumber,
        handler: Arc<dyn SoftirqVec>,
    ) -> Result<i32, SystemError>
```

- 作用：注册软中断向量

- 参数：
  
  - softirq_num：中断向量号
  
  - hanlder：中断函数对应的结构体，需要指向实现了`SoftirqVec`特征的结构体变量

- 返回：
  
  - Ok(i32)：0
  
  - Err(SystemError)：错误码

```rust
pub fn unregister_softirq(&self, softirq_num: SoftirqNumber)
```

- 作用：解注册软中断向量

- 参数：
  
  - softirq_num：中断向量号

```rust
pub fn do_softirq(&self)
```

- 作用：执行软中断函数（**只在硬中断执行后调用**）

```rust
pub fn clear_softirq_pending(&self, softirq_num: SoftirqNumber)
```

- 作用：不需要执行中断号为softirq_num的中断

- 参数：
  
  - softirq_num：中断向量号

```rust
pub fn raise_softirq(&self, softirq_num: SoftirqNumber)
```

- 作用：需要执行中断号为softirq_num的中断

- 参数：
  
  - softirq_num：中断向量号

### 使用实例

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

### 为C提供的接口

```c
extern void rs_softirq_init();
extern void rs_raise_softirq(uint32_t sirq_num);
extern void rs_unregister_softirq(uint32_t sirq_num);
extern void rs_do_softirq();
extern void rs_clear_softirq_pending(uint32_t softirq_num);
```
