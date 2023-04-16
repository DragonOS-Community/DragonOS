# 内核定时器

## 设计思路

&emsp;&emsp;定时器类型为`Timer`结构体，而`Timer`由`SpinLock<InnerTimer>`组成。全局中使用元素类型为`Arc<Timer>`的队列`TIMER_LIST`存储系统创建的定时器。创建定时器时，应调用`Timer::new(timer_func,expire_jiffies)`，timer_func为定时器要执行的操作，expire_jiffies为定时器的结束时间，`timer_func`参数的类型是实现了`TimerFunction`特性的结构体。在创建定时器后，应使用`Timer::activate()`将定时器插入到`TIMER_LIST`中。

&emsp;&emsp;**如果只是希望当前pcb休眠一段时间，应调用`schedule_timeout(timeout)`，timeout指定pcb休眠的时间长度。**

## InnerTimer的数据结构

```rust
pub struct InnerTimer {
    /// 定时器结束时刻 (单位：jiffies)
    pub expire_jiffies: u64,
    /// 定时器需要执行的函数结构体
    pub timer_func: Box<dyn TimerFunction>,
    /// 指向已加锁的定时器的弱指针，不需要用户指定
    self_ref: Weak<Timer>,
}
```

## 定时器API

### Timer的API

```rust
pub fn new(timer_func: Box<dyn TimerFunction>, expire_jiffies: u64) -> Arc<Self>
```

- 功能：创建一个定时器

- 参数：
  
  - timer_func：定时器需要执行的函数对应的结构体，其实现了`TimerFunction`特性
  
  - expire_jiffies：定时器结束时刻（单位：**jiffies**）

- 返回：定时器结构体指针

```rust
pub fn activate(&self)
```

- 功能：将定时器插入到定时器链表中

### 其余API

&emsp;&emsp;**若想要在.c的模块中使用以下函数，请在函数名之前加上rs_**

```rust
pub fn schedule_timeout(mut timeout: i64) -> Result<i64, SystemError>
```

- 功能：让pcb休眠timeout个jiffies

- 参数：
  
  - timeout：需要休眠的时间 （单位：**jiffies**）

- 返回：
  
  - Ok(i64)：剩余需要休眠的时间 （单位：**jiffies**）
  
  - Err(SystemError)：错误码

```rust
pub fn timer_get_first_expire() -> Result<u64, SystemError>
```

- 功能：获取队列中第一个定时器的结束时间，即最早结束的定时器的结束时间

- 返回：
  
  - Ok(i64)：最早结束的定时器的结束时间 （单位：**jiffies**）

    - Err(SystemError)：错误码

```rust
pub fn update_timer_jiffies(add_jiffies: u64) -> u64
```

- 功能：更新系统时间TIMER_JIFFIES

- 参数：
  
  - add_jiffies：需要增加的时间长度（单位：**jiffies**）

- 返回：更新后的系统时间（单位：**jiffies**）

```rust
pub fn clock() -> u64 
```

- 功能：获取当前系统时间

- 返回：当前系统时间（单位：**jiffies**）

```rust
pub fn next_n_ms_timer_jiffies(expire_ms: u64) -> u64
```

- 功能：计算接下来n**毫秒**对应的定时器时间片

- 参数：
  
  - expire_ms：n毫秒

- 返回：对应的定时器时间片（单位：**毫秒**）

```rust
pub fn next_n_us_timer_jiffies(expire_us: u64) -> u64
```

- 功能：计算接下来n**微秒**对应的定时器时间片

- 参数：
  
  - expire_ms：n微秒

- 返回：对应的定时器时间片（单位：**微秒**）

## 创建定时器实例

```rust
struct TimerExample {
    /// 结构体的成员对应函数的形参
    example_parameter: i32,
}
impl TimerExample {
    pub fn new(para: i32) -> Box<TimerExample> {
        return Box::new(TimerExample {
            example_parameter: para,
        });
    }
}
/// 为结构体实现TimerFunction特性
impl TimerFunction for TimerExample {
    /// TimerFunction特性中的函数run
    fn run(&mut self) {
        // 定时器需要执行的操作
        example_func(self.example_parameter);
    }
}
fn example_func(para: i32) {
    println!("para is {:?}", para);
}
fn main() {
    let timer_example: Box<TimerExample> = TimerExample::new(1);
    // 创建一个定时器
    let timer: Arc<Timer> = Timer::new(timer_example, 1);
    // 将定时器插入队列
    timer.activate();
}
```
