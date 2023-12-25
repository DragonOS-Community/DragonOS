# unified-init 统一初始化库

:::{note}
本文作者：龙进 <longjin@DragonOS.org>

2023年12月25日
:::

## 1. 简介

该库位于`kernel/crates/unified-init`中.
提供统一初始化宏,用于将函数注册到统一初始化列表中. 便于统一进行初始化.

需要注意的是，初始化器的数组是no_mangle的，因此其命名应当遵守`模块_初始化器`的规则，防止重名导致意想不到的错误.


## 2. 用法


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

## 3.开发

需要测试的时候可以在`main.rs`写测试代码，
然后在当前目录执行 `cargo expand --bin unified-init-expand`
就可以看到把proc macro展开后的代码了.

## 4. Maintainer

龙进 <longjin@DragonOS.org>


