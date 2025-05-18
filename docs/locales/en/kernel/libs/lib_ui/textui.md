:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/libs/lib_ui/textui.md

- Translation time: 2025-05-19 01:41:14

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Text UI Framework (textui)

:::{note}
Author: Zhou Hanjie <2625553453@qq.com>
:::
&emsp;&emsp;The text framework is primarily used for rendering and displaying text windows in DragonOS. It outputs printed text information to the screen window. Displaying text in the window is divided into two scenarios: one is when the Memory Management Unit (MM) has not been initialized, which prevents dynamic memory allocation, imposing many restrictions (for example, not being able to use vec, mpsc, etc.), so it directly outputs the printed information to the window's frame buffer without using complex structures like virtual lines; the other is when the Memory Management Unit (MM) has been initialized, allowing dynamic memory allocation, which enables the use of more complex structures to handle the text information to be printed.

## Main APIs
### rs_textui_init() - Text UI framework initialization
#### Prototype
```rust
pub extern "C" fn rs_textui_init() -> i32
```
#### Description
&emsp;&emsp;rs_textui_init() is mainly used to initialize some global variable information that the textui framework needs (such as TEXTUIFRAMEWORK, TEXTUI_PRIVATE_INFO, etc.), and to register the textui framework with scm.

### textui_putchar() - Print text information to the currently used window in the textui framework
#### Prototype
```rust
pub extern "C" fn rs_textui_putchar(character: u8, fr_color: u32, bk_color: u32) -> i32

pub fn textui_putchar(
    character: char,
    fr_color: FontColor,
    bk_color: FontColor,
) -> Result<(), SystemError> 
```
#### Description
&emsp;&emsp;textui_putchar() needs to handle two scenarios: one is when the Memory Management Unit (MM) has not been initialized, which prevents dynamic memory allocation, imposing many restrictions (for example, not being able to use vec, mpsc, etc.), so it directly outputs the printed information to the window's frame buffer without using complex structures like virtual lines; the other is when the Memory Management Unit (MM) has been initialized, allowing dynamic memory allocation, which enables the use of more complex structures to handle the text information to be printed.
