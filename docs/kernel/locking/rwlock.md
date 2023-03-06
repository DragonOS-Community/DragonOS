# RwLock读写锁
:::{note}
本文作者: sujintao

Email: <sujintao@dragonos.org>
:::

## 1. 简介
&emsp;&emsp;读写锁是一种在并发环境下保护多进程间共享数据的机制.  相比于普通的spinlock,读写锁将对
共享数据的访问分为读和写两种类型: 只读取共享数据的访问使用读锁控制,修改共享数据的访问使用
写锁控制. 读写锁设计允许同时存在多个"读者"(只读取共享数据的访问)和一个"写者"(修改共享数据
的访问), 对于一些大部分情况都是读访问的共享数据来说,使用读写锁控制访问可以一定程度上提升性能.

## 2. DragonOS中读写锁的实现
### 2.1 读写锁的机理
&emsp;&emsp;读写锁的目的是维护多线程系统中的共享变量的一致性. 数据会被包裹在一个RwLock的数据结构中, 一切的访问必须通过RwLock的数据结构进行访问和修改. 每个要访问共享数据的会获得一个守卫(guard), 只读进程获得READER(读者守卫),需要修改共享变量的进程获得WRITER(写者守卫),作为RwLock的"影子", 线程都根据guard来进行访问和修改操作.

&emsp;&emsp;在实践中, 读写锁除了READER, WRITER, 还增加了UPGRADER; 这是一种介于READER和WRITER之间的守卫, 这个守卫的作用就是防止WRITER的饿死(Staration).当进程获得UPGRADER时,进程把它当成READER来使用;但是UPGRADER可以进行升级处理,升级后的UPGRADER相当于是一个WRITER守卫,可以对共享数据执行写操作.

&emsp;&emsp;所有守卫都满足rust原生的RAII机理,当守卫所在的作用域结束时,守卫将自动释放.

### 2.2 读写锁守卫之间的关系
&emsp;&emsp;同一时间点, 可以存在多个READER, 即可以同时有多个进程对共享数据进行访问;同一时间只能存在一个WRITER,而且当有一个进程获得WRITER时,不能存在READER和UPGRADER;进程获得UPGRADER的前提条件是,不能有UPGRADER或WRITER存在,但是当有一个进程获得UPGRADER时,进程无法成功申请READER.

### 2.3 设计的细节

#### 2.3.1 RwLock数据结构
```rust
pub struct RwLock<T> {
    lock: AtomicU32,//原子变量
    data: UnsafeCell<T>,
}
```
#### 2.3.2 READER守卫的数据结构
```rust
pub struct RwLockReadGuard<'a, T: 'a> {
    data: *const T,
    lock: &'a AtomicU32,
}
```

#### 2.3.3 UPGRADER守卫的数据结构
```rust
pub struct RwLockUpgradableGuard<'a, T: 'a> {
    data: *const T,
    inner: &'a RwLock<T>,
}
```

#### 2.3.4 WRITER守卫的数据结构
```rust
pub struct RwLockWriteGuard<'a, T: 'a> {
    data: *mut T,
    inner: &'a RwLock<T>,
}
```

#### 2.3.5 RwLock的lock的结构介绍
lock是一个32位原子变量AtomicU32, 它的比特位分配如下:
```
                                                       UPGRADER_BIT     WRITER_BIT
                                                         ^                   ^
OVERFLOW_BIT                                             +------+    +-------+
  ^                                                             |    |
  |                                                             |    |
+-+--+--------------------------------------------------------+-+--+-+--+
|    |                                                        |    |    |
|    |                                                        |    |    |
|    |             The number of the readers                  |    |    |
|    |                                                        |    |    |
+----+--------------------------------------------------------+----+----+
  31  30                                                    2   1    0
```

&emsp;&emsp;(从右到左)第0位表征WRITER是否有效,若WRITER_BIT=1, 则存在一个进程获得了WRITER守卫; 若UPGRADER_BIT=1, 则存在一个进程获得了UPGRADER守卫,第2位到第30位用来二进制表示获得READER守卫的进程数; 第31位是溢出判断位, 若OVERFLOW_BIT=1, 则不再接受新的读者守卫的获得申请.


## 3.  读写锁的主要API
### 3.1 RwLock的主要API
```rust
///功能:  输入需要保护的数据类型data,返回一个新的RwLock类型.
pub const fn new(data: T) -> Self
```
```rust
///功能: 获得READER守卫
pub fn read(&self) -> RwLockReadGuard<T>
```
```rust
///功能: 尝试获得READER守卫
pub fn try_read(&self) -> Option<RwLockReadGuard<T>>
```
```rust
///功能: 获得WRITER守卫
pub fn write(&self) -> RwLockWriteGuard<T>
```
```rust
///功能: 尝试获得WRITER守卫
pub fn try_write(&self) -> Option<RwLockWriteGuard<T>>
```
```rust
///功能: 获得UPGRADER守卫
pub fn upgradeable_read(&self) -> RwLockUpgradableGuard<T>
```
```rust
///功能: 尝试获得UPGRADER守卫
pub fn try_upgradeable_read(&self) -> Option<RwLockUpgradableGuard<T>>
```
### 3.2 WRITER守卫RwLockWriteGuard的主要API
```rust
///功能: 将WRITER降级为READER
pub fn downgrade(self) -> RwLockReadGuard<'rwlock, T>
```
```rust
///功能: 将WRITER降级为UPGRADER
pub fn downgrade_to_upgradeable(self) -> RwLockUpgradableGuard<'rwlock, T>
```
### 3.3 UPGRADER守卫RwLockUpgradableGuard的主要API
```rust
///功能: 将UPGRADER升级为WRITER
pub fn upgrade(mut self) -> RwLockWriteGuard<'rwlock, T> 
```
```rust
///功能: 将UPGRADER降级为READER
pub fn downgrade(self) -> RwLockReadGuard<'rwlock, T>
```

## 4. 用法实例
```rust
static LOCK: RwLock<u32> = RwLock::new(100 as u32);

fn t_read1() {
    let guard = LOCK.read();
    let value = *guard;
    let readers_current = LOCK.reader_count();
    let writers_current = LOCK.writer_count();
    println!(
        "Reader1: the value is {value}
    There are totally {writers_current} writers, {readers_current} readers"
    );
}

fn t_read2() {
    let guard = LOCK.read();
    let value = *guard;
    let readers_current = LOCK.reader_count();
    let writers_current = LOCK.writer_count();
    println!(
        "Reader2: the value is {value}
    There are totally {writers_current} writers, {readers_current} readers"
    );
}

fn t_write() {
    let mut guard = LOCK.write();
    *guard += 100;
    let writers_current = LOCK.writer_count();
    let readers_current = LOCK.reader_count();
    println!(
        "Writers: the value is {guard}
    There are totally {writers_current} writers, {readers_current} readers",
        guard = *guard
    );
    let read_guard=guard.downgrade();
    let value=*read_guard;
    println!("After downgraded to read_guard: {value}");
}

fn t_upgrade() {
    let guard = LOCK.upgradeable_read();
    let value = *guard;
    let readers_current = LOCK.reader_count();
    let writers_current = LOCK.writer_count();
    println!(
        "Upgrader1 before upgrade: the value is {value}
    There are totally {writers_current} writers, {readers_current} readers"
    );
    let mut upgraded_guard = guard.upgrade();
    *upgraded_guard += 100;
    let writers_current = LOCK.writer_count();
    let readers_current = LOCK.reader_count();
    println!(
        "Upgrader1 after upgrade: the value is {temp}
    There are totally {writers_current} writers, {readers_current} readers",
        temp = *upgraded_guard
    );
    let downgraded_guard=upgraded_guard.downgrade_to_upgradeable();
    let value=*downgraded_guard;
    println!("value after downgraded: {value}");
    let read_guard=downgraded_guard.downgrade();
    let value_=*read_guard;
    println!("value after downgraded to read_guard: {value_}");
}

fn main() {
    let r2=thread::spawn(t_read2);
    let r1 = thread::spawn(t_read1);
    let t1 = thread::spawn(t_write);
    let g1 = thread::spawn(t_upgrade);
    r1.join().expect("r1");
    t1.join().expect("t1");
    g1.join().expect("g1");
    r2.join().expect("r2");
}
```