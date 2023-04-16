# 类型转换库API

&emsp;&emsp;内核提供了一些函数来帮助你在不同的类型之间进行转换。包括以下类型：

- 数值类型转换 （使用`num-traits`库）
- Arc类型转换

&emsp;&emsp;上述没有特殊标明的函数，都是在`kernel/src/libs/casting.rs`中实现的。


## 1. 数值类型转换

### 1.1. 整数类型与枚举类型之间的转换

&emsp;&emsp;您可以使用`num-traits`库提供的宏，实现枚举类型和整数类型之间的转换。
SystemError枚举类型使用了这种方式，您可以在`kernel/src/syscall/mod.rs`中找到它的用法。

&emsp;&emsp;它首先继承了`FromPrimitive, ToPrimitive`两个trait，然后这样转换：

```rust
impl SystemError {
    /// @brief 把posix错误码转换为系统错误枚举类型。
    pub fn from_posix_errno(errno: i32) -> Option<SystemError> {
        // posix 错误码是小于0的
        if errno >= 0 {
            return None;
        }
        return <Self as FromPrimitive>::from_i32(-errno);
    }

    /// @brief 把系统错误枚举类型转换为负数posix错误码。
    pub fn to_posix_errno(&self) -> i32 {
        return -<Self as ToPrimitive>::to_i32(self).unwrap();
    }
}
```

&emsp;&emsp;这两个函数很好的说明了如何使用这两个trait。

## 2. Arc类型转换

### 2.1 从Arc<dyn U>转换为Arc<T>

&emsp;&emsp;当我们需要把一个`Arc<dyn U>`转换为`Arc<T>`的具体类型指针时，我们要为`U`这个trait实现`DowncastArc`trait。这个trait定义在`kernel/src/libs/casting.rs`中。它要求`trait U`实现`Any + Sync + Send`trait.

&emsp;&emsp;为`trait U: Any + Send + Sync`实现`DowncastArc`trait，需要这样做：

```rust
impl DowncastArc for dyn U {
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any> {
        return self;
    }
}
```

&emsp;&emsp;使用`DowncastArc`trait，我们可以这样转换：

```rust
let arc: Arc<dyn U> = ...;
let arc_t: Arc<T> = arc.downcast_arc::<T>().unwrap();
```

&emsp;&emsp;如果`arc`的具体类型不是`Arc<T>`，那么`downcast_arc::<T>()`会返回`None`。
