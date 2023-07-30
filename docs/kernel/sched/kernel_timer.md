# 内核定时器

## 1. 简介

&emsp;&emsp;内核定时器是内核中的一种定时器，内核定时器的工作方式是：添加定时器到队列，为每个定时器设置到期时间。当定时器到期时，会执行定时器对应的函数。

## 2. 设计思路

&emsp;&emsp;定时器类型为`Timer`结构体，而`Timer`由`SpinLock<InnerTimer>`组成。全局中使用元素类型为`Arc<Timer>`的队列`TIMER_LIST`存储系统创建的定时器。创建定时器时，应调用`Timer::new(timer_func,expire_jiffies)`，timer_func为定时器要执行的操作，expire_jiffies为定时器的结束时间，`timer_func`参数的类型是实现了`TimerFunction`特性的结构体。在创建定时器后，应使用`Timer::activate()`将定时器插入到`TIMER_LIST`中。

&emsp;&emsp;**如果只是希望当前pcb休眠一段时间，应调用`schedule_timeout(timeout)`，timeout指定pcb休眠的时间长度。**

## 3. 定时器应实现的特性

&emsp;&emsp;定时器要执行的函数应实现`TimerFunction`特性，其定义如下：

```rust
/// 定时器要执行的函数的特征
pub trait TimerFunction: Send + Sync {
    fn run(&mut self);
}
```

&emsp;&emsp;一种典型的实现方式是：新建一个零长的结构体，实现`TimerFunction`特性，然后在`run`函数中实现定时器要执行的操作。

## 4. 定时器API

### 4.1. Timer的API

#### 4.1.1. 创建一个定时器
```rust
pub fn new(timer_func: Box<dyn TimerFunction>, expire_jiffies: u64) -> Arc<Self>
```

**参数**
  
- timer_func：定时器需要执行的函数对应的结构体，其实现了`TimerFunction`特性

- expire_jiffies：定时器结束时刻（单位：**jiffies**）

**返回**

- 定时器结构体指针

#### 4.1.2. 将定时器插入到定时器链表中

```rust
pub fn activate(&self)
```

### 4.2. 其余API

&emsp;&emsp;**若想要在.c的模块中使用以下函数，请在函数名之前加上rs_**

#### 4.2.1. 让进程休眠一段时间

```rust
pub fn schedule_timeout(mut timeout: i64) -> Result<i64, SystemError>
```

**功能**

&emsp;&emsp;让进程休眠timeout个jiffies

**参数**
  
- timeout：需要休眠的时间 （单位：**jiffies**）

**返回值**
  
- Ok(i64)：剩余需要休眠的时间 （单位：**jiffies**）
- Err(SystemError)：错误码

#### 4.2.2. 获取队列中第一个定时器的结束时间

```rust
pub fn timer_get_first_expire() -> Result<u64, SystemError>
```

**功能**

&emsp;&emsp;获取队列中第一个定时器的结束时间，即最早结束的定时器的结束时间

**返回值**
  
- Ok(i64)：最早结束的定时器的结束时间 （单位：**jiffies**）
- Err(SystemError)：错误码

#### 4.2.3. 获取当前系统时间

```rust
pub fn clock() -> u64 
```

**功能**

&emsp;&emsp;获取当前系统时间（单位：**jiffies**）

#### 4.2.4. 计算接下来n毫秒或者微秒对应的定时器时间片

##### 4.2.4.1. 毫秒

```rust
pub fn next_n_ms_timer_jiffies(expire_ms: u64) -> u64
```

**功能**

&emsp;&emsp;计算接下来n**毫秒**对应的定时器时间片

**参数**

- expire_ms：n毫秒

**返回值**

&emsp;&emsp;对应的定时器时间片（单位：**毫秒**）

##### 4.2.4.2. 微秒

```rust
pub fn next_n_us_timer_jiffies(expire_us: u64) -> u64
```

**功能**

&emsp;&emsp;计算接下来n**微秒**对应的定时器时间片

**参数**
  
- expire_ms：n微秒

**返回值**

&emsp;&emsp;对应的定时器时间片（单位：**微秒**）

## 5. 创建定时器实例

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
