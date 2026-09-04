# unified-init Unified Initialization Library

::: info Author
Longjin `<longjin@DragonOS.org>`

December 25, 2023
:::


## 1. Introduction

This library is located in `kernel/crates/unified-init`.
It provides unified initialization macros to register functions into a unified initialization list. It facilitates unified initialization.

It is important to note that the array of initializers is no_mangle, so its naming should follow the rules of `模块_初始化器` to prevent naming conflicts that could lead to unexpected errors.

## 2. Usage

```rust
use system_error::SystemError;
use unified_init::define_unified_initializer_slice;
use unified_init_macros::unified_init;

/// 初始化函数都将会被放到这个列表中
define_unified_initializer_slice!(INITIALIZER_LIST);

#[unified_init(INITIALIZER_LIST)]
fn init1() -> Result<(), SystemError> {
   Ok(())
}

#[unified_init(INITIALIZER_LIST)]
fn init2() -> Result<(), SystemError> {
   Ok(())
}

fn main() {
    assert_eq!(INITIALIZER_LIST.len(), 2);
}

```

## 3. Development

When testing, you can write test code in `main.rs`,
and then run `cargo expand --bin unified-init-expand` in the current directory to see the code after the proc macro has been expanded.

## 4. Maintainer

Longjin `<longjin@DragonOS.org>`
