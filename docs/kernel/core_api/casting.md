# Type Conversion Library API

The kernel provides some functions to help you convert between different types. These include the following types:

- Numeric type conversion (using the `num-traits` library)
- Arc type conversion

All functions not specially marked are implemented in `kernel/src/libs/casting.rs`.

## 1. Numeric Type Conversion

### 1.1 Conversion Between Integer Types and Enum Types

You can use macros provided by the `num-traits` library to convert between enum types and integer types.
The SystemError enum type uses this approach, and you can find its usage in `kernel/src/syscall/mod.rs`.

It first inherits the `FromPrimitive, ToPrimitive` two traits, and then performs the conversion like this:

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

These two functions well illustrate how to use these two traits.

## 2. Arc Type Conversion

### 2.1 Conversion from Arc`<dyn U>` to Arc`<T>`

When we need to convert an `Arc<dyn U>` to a specific type pointer of `Arc<T>`, we need to implement the `DowncastArc` trait for `U`. This trait is defined in `kernel/src/libs/casting.rs`. It requires `trait U` to implement the `Any + Sync + Send` trait.

To implement the `DowncastArc` trait for `trait U: Any + Send + Sync`, you need to do the following:

```rust
impl DowncastArc for dyn U {
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any> {
        return self;
    }
}
```

Using the `DowncastArc` trait, we can convert like this:

```rust
let arc: Arc<dyn U> = ...;
let arc_t: Arc<T> = arc.downcast_arc::<T>().unwrap();
```

If the specific type of `arc` is not `Arc<T>`, then `downcast_arc::``<T>``()` will return `None`.

