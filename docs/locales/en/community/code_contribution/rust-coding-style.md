:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: community/code_contribution/rust-coding-style.md

- Translation time: 2025-05-19 01:41:39

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Rust Language Code Style

&emsp;&emsp;This document will introduce the Rust language code style used in DragonOS. As development progresses, these styles may change, but we will strive to maintain consistency in the style.

## 1. Naming

&emsp;&emsp;This section is based on the naming conventions from the Rust language bible, [Naming Guide](https://course.rs/practice/naming.html). For parts not mentioned in this document, please refer to the [Naming Guide](https://course.rs/practice/naming.html) in the Rust language bible.

## 2. Formatting

### 2.1 Indentation

&emsp;&emsp;Please use the `cargo fmt` command to format the code before submitting it.

### 2.2 Function Return Values

&emsp;&emsp;Although Rust allows returning the value of the last line of a function, this approach can reduce code readability. Therefore, we recommend using the `return` statement as the last line of the function, rather than directly returning the value.

```rust
// 不推荐
fn foo() -> i32 {
    1 + 2
}

// 推荐
fn foo() -> i32 {
    return 1 + 2;
}
```
### 2.3 Error Handling

&emsp;&emsp;DragonOS uses returning POSIX error codes as the **inter-module error handling** method. To ensure consistency in error handling code across modules, we recommend returning the `SystemError` type when an error occurs. This approach is especially beneficial when calling functions across modules, as it allows direct return of a generic error code, thereby reducing the coupling of error handling code.

```rust
// 函数跨越模块边界时（由其他模块调用当前函数），不推荐
fn foo() -> Result<(), CustomErr> {
    if 1 + 2 == 3 {
        return Ok(());
    } else {
        return Err(CustomErr::error);
    }
}

// 函数跨越模块边界时（由其他模块调用当前函数），推荐
fn foo() -> Result<(), SystemError> {
    if 1 + 2 == 3 {
        return Ok(());
    } else {
        return Err(SystemError::EINVAL);
    }
}
```

&emsp;&emsp;Within **modules**, you can either use a custom error enum or return the `SystemError` type. However, we recommend using a custom error enum for error handling within modules, as it makes the error handling code clearer.

&emsp;&emsp;**TODO**: Convert existing code that uses i32 as an error code to use `SystemError`.

## 3. Comments

&emsp;&emsp;The commenting style in DragonOS is consistent with the official Rust style. We also recommend adding as many meaningful comments as possible in your code to help others understand your code. Additionally, variable and function declarations should follow the naming conventions mentioned in Section 1, making them "self-documenting."

### 3.1 Function Comments

&emsp;&emsp;Function comments should include the following:

- The function's purpose
- The function's parameters
- The function's return value
- The function's error handling
- Any side effects or other information that needs to be explained

&emsp;&emsp;The format for function comments is as follows:

```rust
/// # 函数的功能
/// 
/// 函数的详细描述
/// 
/// ## 参数
/// 
/// - 参数1: 参数1的说明
/// - 参数2: 参数2的说明
/// - ...
/// 
/// ## 返回值
/// - Ok(返回值类型): 返回值的说明
/// - Err(错误值类型): 错误的说明
/// 
/// ## Safety
/// 
/// 函数的安全性说明
```
