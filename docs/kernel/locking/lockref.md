(_lockref)=
# lockref

&emsp;&emsp;lockref是将自旋锁与引用计数变量融合在连续、对齐的8字节内的一种技术。

&emsp;&emsp;目前，DragonOS中，通过C、Rust各实现了一个版本的lockref。请注意，二者不兼容。对于新的功能模块，请使用Rust版本的lockref。随着代码重构工作的进行，我们将会删除C版本的lockref。

## 1. lockref结构

### 1.1. Rust版本
```rust
/// 仅在x86_64架构下使用cmpxchg
#[cfg(target_arch = "x86_64")]
/// 由于需要cmpxchg，所以整个lockref按照8字节对齐
#[repr(align(8))]
#[derive(Debug)]
pub struct LockRef {
    pub lock: RawSpinlock,
    pub count: i32,
}

/// 除了x86_64以外的架构，不使用cmpxchg进行优化
#[cfg(not(target_arch = "x86_64"))]
pub struct LockRef {
    lock: RawSpinlock,
    count: i32,
}
```

### 1.2. C版本
```c
struct lockref
{
    union
    {
#ifdef __LOCKREF_ENABLE_CMPXCHG__
        aligned_u64 lock_count; // 通过该变量的声明，使得整个lockref的地址按照8字节对齐
#endif
        struct
        {
            spinlock_t lock;
            int count;
        };
    };
};
```

## 2. 特性描述
&emsp;&emsp;由于在高负载的情况下，系统会频繁的执行“锁定-改变引用变量-解锁”的操作，这期间很可能出现spinlock和引用计数跨缓存行的情况，这将会大大降低性能。lockref通过强制对齐，尽可能的降低缓存行的占用数量，使得性能得到提升。

&emsp;&emsp;并且，在x64体系结构下，还通过cmpxchg()指令，实现了无锁快速路径。不需要对自旋锁加锁即可更改引用计数的值，进一步提升性能。当快速路径不存在（对于未支持的体系结构）或者尝试超时后，将会退化成“锁定-改变引用变量-解锁”的操作。此时由于lockref强制对齐，只涉及到1个缓存行，因此性能比原先的spinlock+ref_count的模式要高。

## 3. 关于cmpxchg_loop

&emsp;&emsp;在改变引用计数时，cmpxchg先确保没有别的线程持有锁，然后改变引用计数，同时通过`lock cmpxchg`指令验证在更改发生时，没有其他线程持有锁，并且当前的目标lockref的值与old变量中存储的一致，从而将新值存储到目标lockref。这种无锁操作能极大的提升性能。如果不符合上述条件，在多次尝试后，将退化成传统的加锁方式来更改引用计数。

## 4. Rust版本的API

### 4.1. 引用计数自增

- `pub fn inc(&mut self)`
- `pub fn inc_not_zero(&mut self) -> Result<i32, SystemError>`
- `pub fn inc_not_dead(&mut self) -> Result<i32, SystemError>`

#### 4.1.1. inc

##### 说明

&emsp;&emsp;原子的将引用计数加1。

##### 返回值

&emsp;&emsp;无

#### 4.1.2. inc_not_zero

##### 说明

&emsp;&emsp;原子地将引用计数加1.如果原来的count≤0，则操作失败。

##### 返回值

|   返回值          |      说明    |
|   :---        |   :---      |
| Ok(self.count) | 成功，返回新的引用计数 |
| Err(SystemError::EPERM)       | 失败，返回EPERM |

#### 4.1.3. inc_not_dead

##### 说明

&emsp;&emsp;引用计数自增1。（除非该lockref已经被标记为死亡）

##### 返回值

|   返回值          |      说明    |
|   :---        |   :---      |
| Ok(self.count) | 成功，返回新的引用计数 |
| Err(SystemError::EPERM)       | 失败，返回EPERM |

### 4.2. 引用计数自减
- `pub fn dec(&mut self) -> Result<i32, SystemError>`
- `pub fn dec_return(&mut self) -> Result<i32, SystemError>`
- `pub fn dec_not_zero(&mut self) -> Result<i32, SystemError>`
- `pub fn dec_or_lock_not_zero(&mut self) -> Result<i32, SystemError>`

#### 4.2.1. dec

##### 说明

&emsp;&emsp;原子地将引用计数-1。如果已处于count≤0的状态，则返回Err(SystemError::EPERM)

&emsp;&emsp;本函数与`lockref_dec_return()`的区别在于，当在`cmpxchg()`中检测到`count<=0`或已加锁，本函数会再次尝试通过加锁来执行操作，而`lockref_dec_return()`会直接返回错误

##### 返回值

|   返回值          |      说明    |
|   :---        |   :---      |
| Ok(self.count) | 成功，返回新的引用计数 |
| Err(SystemError::EPERM)       | 失败，返回EPERM |

#### 4.2.2. dec_return

&emsp;&emsp;原子地将引用计数减1。如果处于已加锁或count≤0的状态，则返回SystemError::EPERM

&emsp;&emsp;本函数与`lockref_dec()`的区别在于，当在`cmpxchg()`中检测到`count<=0`或已加锁，本函数会直接返回错误，而`lockref_dec()`会再次尝试通过加锁来执行操作.

:::{note}
若当前处理器架构不支持cmpxchg，则退化为`self.dec()`
:::

##### 返回值

|   返回值          |      说明    |
|   :---        |   :---      |
| Ok(self.count) | 成功，返回新的引用计数 |
| Err(SystemError::EPERM)       | 失败，返回EPERM |

#### 4.2.3. dec_not_zero

##### 说明

&emsp;&emsp;原子地将引用计数减1。若当前的引用计数≤1，则操作失败.

&emsp;&emsp;该函数与`lockref_dec_or_lock_not_zero()`的区别在于，当`cmpxchg()`时发现`old.count≤1`时，该函数会直接返回`Err(-1)`，而`lockref_dec_or_lock_not_zero()`在这种情况下，会尝试加锁来进行操作。

##### 返回值

|   返回值          |      说明    |
|   :---        |   :---      |
| Ok(self.count) | 成功，返回新的引用计数 |
| Err(SystemError::EPERM)       | 失败，返回EPERM |


#### 4.2.4. dec_or_lock_not_zero

##### 说明

&emsp;&emsp;原子地将引用计数减1。若当前的引用计数≤1，则操作失败.

&emsp;&emsp;该函数与`lockref_dec_not_zero()`的区别在于，当cmpxchg()时发现`old.count≤1`时，该函数会尝试加锁来进行操作，而`lockref_dec_not_zero()`在这种情况下，会直接返回`Err(SystemError::EPERM)`.

##### 返回值

|   返回值          |      说明    |
|   :---        |   :---      |
| Ok(self.count) | 成功，返回新的引用计数 |
| Err(SystemError::EPERM)       | 失败，返回EPERM |

### 4.3. 其他
- `pub fn mark_dead(&mut self)`

#### 4.3.1. mark_dead

##### 说明

&emsp;&emsp;将引用计数原子地标记为死亡状态.

## 参考资料

&emsp;&emsp;[Introducing lockrefs - LWN.net, Jonathan Corbet](https://lwn.net/Articles/565734/)
