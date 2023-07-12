# 文本显示框架（textui）

:::{note}
作者: 周瀚杰 <2625553453@qq.com>
:::
&emsp;&emsp;文本框架主要用于DragonOS的文本的窗口渲染显示，往屏幕窗口中输出打印文本信息，往窗口显示文本分成两种情况：一种是当内存管理单元（mm）未被初始化时，不能进行动态内存分配，限制颇多（例如不能使用vec,mpsc等），所以直接往窗口的帧缓冲区输出打印信息，不使用虚拟行等复杂结构体；另一种是当内存管理单元（mm）已经初始化，可以进行动态内存分配，便可以使用一些复杂的结构体来处理要打印的文本信息。

## 主要数据结构
### WindowMpsc
&emsp;&emsp;使用mpsc(multi-producer single-consumer)来传递要往当前窗口打印的文本信息，从而减少了每次打印信息时锁的使用，提高系统的运行效率。定义如下：
```rust
pub struct WindowMpsc {
    window_r: mpsc::Receiver<TextuiWindow>,
    window_s: mpsc::Sender<TextuiWindow>,
}
```

### TextuiCharChromatic
&emsp;&emsp;存储textui框架窗口中特定彩色字符对象的信息，定义如下：
```rust
#[derive(Clone, Debug, Copy)]
pub struct TextuiCharChromatic {
    c: u8,

    // 前景色
    frcolor: FontColor, // rgb

    // 背景色
    bkcolor: FontColor, // rgb
}
```
### TextuiCharNormal
&emsp;&emsp;存储textui框架窗口中特定黑白字符对象的信息，定义如下：
```rust
#[derive(Clone, Debug)]
struct TextuiCharNormal {
    c: u8,
}
```
### TextuiVlineNormal
&emsp;&emsp;存储textui框架窗口中由黑白字符对象组成的虚拟行信息，定义如下：
```rust
#[derive(Clone, Debug, Default)]
pub struct TextuiVlineNormal {
    chars: Vec<TextuiCharNormal>, // 字符对象数组
    index: i16,                   // 当前操作的位置
}
```
### TextuiVlineNormal
&emsp;&emsp;存储textui框架窗口中由彩色字符对象组成的虚拟行信息，定义如下：
```rust
#[derive(Clone, Debug, Default)]
pub struct TextuiVlineChromatic {
    chars: Vec<TextuiCharChromatic>, // 字符对象数组
    index: LineIndex,                // 当前操作的位置
}
```
### TextuiWindow
&emsp;&emsp;存储textui框架某个窗口的信息，定义如下：
```rust
#[derive(Clone, Debug)]
pub struct TextuiWindow {
    // 虚拟行是个循环表，头和尾相接
    id: WindowId,
    // 虚拟行总数
    vline_sum: i32,
    // 当前已经使用了的虚拟行总数（即在已经输入到缓冲区（之后显示在屏幕上）的虚拟行数量）
    vlines_used: i32,
    // 位于最顶上的那一个虚拟行的行号
    top_vline: LineId,
    // 储存虚拟行的数组
    vlines: Vec<TextuiVline>,
    // 正在操作的vline
    vline_operating: LineId,
    // 每行最大容纳的字符数
    chars_per_line: i32,
    // 窗口flag
    flags: WindowFlag,
}
```
### TextUiFramework
&emsp;&emsp;储存textui框架信息，定义如下：
```rust
#[derive(Debug)]
pub struct TextUiFramework {
    metadata: ScmUiFrameworkMetadata,
}
```

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
pub extern "C" fn textui_putchar(character: u8, fr_color: u32, bk_color: u32) -> i32
```
#### 说明
&emsp;&emsp;textui_putchar()要处理两种情况：一种是当内存管理单元（mm）未被初始化时，不能进行动态内存分配，限制颇多（例如不能使用vec,mpsc等），所以直接往窗口的帧缓冲区输出打印信息，不使用虚拟行等复杂结构体；另一种是当内存管理单元（mm）已经初始化，可以进行动态内存分配，便可以使用一些复杂的结构体来处理要打印的文本信息。


