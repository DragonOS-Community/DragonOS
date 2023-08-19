# 文本显示框架（textui）

:::{note}
作者: 周瀚杰 <2625553453@qq.com>
:::
&emsp;&emsp;文本框架主要用于DragonOS的文本的窗口渲染显示，往屏幕窗口中输出打印文本信息，往窗口显示文本分成两种情况：一种是当内存管理单元（mm）未被初始化时，不能进行动态内存分配，限制颇多（例如不能使用vec,mpsc等），所以直接往窗口的帧缓冲区输出打印信息，不使用虚拟行等复杂结构体；另一种是当内存管理单元（mm）已经初始化，可以进行动态内存分配，便可以使用一些复杂的结构体来处理要打印的文本信息。


## 主要API
### rs_textui_init() -textui框架初始化
#### 原型
```rust
pub extern "C" fn rs_textui_init() -> i32
```
#### 说明
&emsp;&emsp;rs_textui_init()主要是初始化一些textui框架要使用到的一些全局变量信息（例如TEXTUIFRAMEWORK，TEXTUI_PRIVATE_INFO等），以及将textui框架注册到scm中。

### textui_putchar（） -往textui框架中的当前使用的窗口打印文本信息
#### 原型
```rust
pub extern "C" fn rs_textui_putchar(character: u8, fr_color: u32, bk_color: u32) -> i32

pub fn textui_putchar(
    character: char,
    fr_color: FontColor,
    bk_color: FontColor,
) -> Result<(), SystemError> 
```
#### 说明
&emsp;&emsp;textui_putchar()要处理两种情况：一种是当内存管理单元（mm）未被初始化时，不能进行动态内存分配，限制颇多（例如不能使用vec,mpsc等），所以直接往窗口的帧缓冲区输出打印信息，不使用虚拟行等复杂结构体；另一种是当内存管理单元（mm）已经初始化，可以进行动态内存分配，便可以使用一些复杂的结构体来处理要打印的文本信息。


