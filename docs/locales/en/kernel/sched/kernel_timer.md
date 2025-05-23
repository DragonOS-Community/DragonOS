:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/sched/kernel_timer.md

- Translation time: 2025-05-19 01:41:48

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Kernel Timer

## 1. Introduction

&emsp;&emsp;The kernel timer is a type of timer within the kernel. The working mechanism of the kernel timer is: adding the timer to a queue and setting the expiration time for each timer. When the timer expires, the function corresponding to the timer is executed.

## 2. Design Concept

&emsp;&emsp;The timer type is a structure of `Timer`, and `Timer` is composed of `SpinLock<InnerTimer>`. A global queue `TIMER_LIST` with element type `Arc<Timer>` is used to store the timers created by the system. When creating a timer, you should call `Timer::new(timer_func,expire_jiffies)`, where timer_func is the function to be executed by the timer, expire_jiffies is the expiration time of the timer, and the type of `timer_func` parameter is a structure that implements the `TimerFunction` characteristic. After creating the timer, you should use `Timer::activate()` to insert the timer into `TIMER_LIST`.

&emsp;&emsp;**If you only want the current PCB to sleep for a certain period, you should call `schedule_timeout(timeout)`, and timeout specifies the duration of the PCB sleep.**

## 3. Features That the Timer Should Implement

&emsp;&emsp;The function to be executed by the timer should implement the `TimerFunction` characteristic, and its definition is as follows:

```rust
/// 定时器要执行的函数的特征
pub trait TimerFunction: Send + Sync {
    fn run(&mut self);
}
```

&emsp;&emsp;A typical implementation method is: creating a zero-length structure, implementing the `TimerFunction` characteristic, and then implementing the operation to be performed by the timer in the `run` function.

## 4. Timer API

### 4.1. Timer API

#### 4.1.1. Create a Timer
```rust
pub fn new(timer_func: Box<dyn TimerFunction>, expire_jiffies: u64) -> Arc<Self>
```

**Parameters**

- timer_func: A structure corresponding to the function that the timer needs to execute, which implements the `TimerFunction` characteristic

- expire_jiffies: The expiration time of the timer (unit: **jiffies**)

**Return**

- Pointer to the timer structure

#### 4.1.2. Insert the Timer into the Timer List

```rust
pub fn activate(&self)
```

### 4.2. Other APIs

&emsp;&emsp;**If you want to use the following functions in a .c module, please add rs_ before the function name.**

#### 4.2.1. Make the Process Sleep for a Certain Period

```rust
pub fn schedule_timeout(mut timeout: i64) -> Result<i64, SystemError>
```

**Function**

&emsp;&emsp;Make the process sleep for timeout jiffies

**Parameters**

- timeout: The time to sleep (unit: **jiffies**)

**Return Value**

- Ok(i64): Remaining time to sleep (unit: **jiffies**)
- Err(SystemError): Error code

#### 4.2.2. Get the Expiration Time of the First Timer in the Queue

```rust
pub fn timer_get_first_expire() -> Result<u64, SystemError>
```

**Function**

&emsp;&emsp;Get the expiration time of the first timer in the queue, i.e., the expiration time of the earliest expiring timer

**Return Value**

- Ok(i64): Expiration time of the earliest expiring timer (unit: **jiffies**)
- Err(SystemError): Error code

#### 4.2.3. Get the Current System Time

```rust
pub fn clock() -> u64 
```

**Function**

&emsp;&emsp;Get the current system time (unit: **jiffies**)

#### 4.2.4. Calculate the Timer Time Slice Corresponding to the Next n Milliseconds or Microseconds

##### 4.2.4.1. Milliseconds

```rust
pub fn next_n_ms_timer_jiffies(expire_ms: u64) -> u64
```

**Function**

&emsp;&emsp;Calculate the timer time slice corresponding to the next n **milliseconds**

**Parameters**

- expire_ms: n milliseconds

**Return Value**

&emsp;&emsp;The corresponding timer time slice (unit: **milliseconds**)

##### 4.2.4.2. Microseconds

```rust
pub fn next_n_us_timer_jiffies(expire_us: u64) -> u64
```

**Function**

&emsp;&emsp;Calculate the timer time slice corresponding to the next n **microseconds**

**Parameters**

- expire_ms: n microseconds

**Return Value**

&emsp;&emsp;The corresponding timer time slice (unit: **microseconds**)

## 5. Creating a Timer Instance

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
