# 使用DADK对内核进行性能分析

## 1. 概述

本文将教你使用DADK，对DragonOS内核进行性能分析，以识别和解决潜在的性能瓶颈。

### 1.1 准备工作

::: {note}
在开始之前，请确保你已经安装了DADK，并且已经配置好了DragonOS内核的编译环境。
:::

### 1.2 什么是火焰图？

如果你没有听说过火焰图，可以先阅读这篇文章：[《如何读懂火焰图？- 阮一峰》](https://www.ruanyifeng.com/blog/2017/09/flame-graph.html)

简单的说，火焰图是基于性能采样结果产生的 SVG 图片，用来展示 CPU 的调用栈。

![](https://web-static2.dragonos.org.cn//longjin/flame2.svg?imageSlim)

x 轴表示抽样数，如果一个函数在 x 轴占据的宽度越宽，就表示它被抽到的次数多，即执行的时间长。注意，x 轴不代表时间，而是所有的调用栈合并后，按字母顺序排列的。

火焰图就是看顶层的哪个函数占据的宽度最大。只要有"平顶"（plateaus），就表示该函数可能存在性能问题。

颜色没有特殊含义，因为火焰图表示的是 CPU 的繁忙程度，所以一般选择暖色调。

## 2. 配置DragonOS内核

由于性能分析需要详尽的符号表数据，因此我们需要在编译内核时，需要进行以下配置：

在`kernel/Cargo.toml`中的`[profile.release]`部分，设置以下两项：

```toml
[profile.release]
debug = true
opt-level = 1
```

这样，编译出来的内核就会包含符号表数据，方便我们进行性能分析。

## 3. 使用DADK进行性能分析

### 3.1 启动内核

首先，我们需要启动DragonOS内核。

```shell
# 使用你喜欢的方式启动内核，例如：
make run
# 或者
make build && make qemu-nographic
```

### 3.2 运行你的工作负载

在启动内核后，我们需要运行一些工作负载，以便进行性能分析。

这可以是一个应用程序，也可以是别的东西。甚至你可以什么都不运行，只是单纯看看DragonOS内核在空闲时的调用栈情况。

### 3.3 启动DADK进行性能分析

在DragonOS项目目录下，运行以下命令：

```shell
dadk profile sample --format flamegraph  --output flame.svg --interval 200ms --duration 20s  --cpu-mask 0x1 --kernel bin/x86_64/kernel/kernel.elf
```

上面的命令，将会对DragonOS内核进行性能分析，并生成一个火焰图。

详细解释：

- `--format flamegraph`：指定输出格式为火焰图。
- `--output flame.svg`：指定输出文件名为`flame.svg`。
- `--interval 200ms`：指定采样间隔为200ms。
- `--duration 20s`：指定采样时间为20s。
- `--cpu-mask 0x1`：指定采样的CPU为0号CPU。（这是个按位掩码，也就是说，如果要采样0和1号CPU，那么cpu-mask为0x3）

*更多参数请参考`dadk profile sample --help`.*

::: {note}
由于采样时会暂停vCPU，因此采样时间不宜过短，否则会影响系统的正常运行。
:::

经过一段时间的等待，你将会得到一个`flame.svg`文件。

### 3.4 分析火焰图

使用浏览器打开`flame.svg`文件，你将会看到一个火焰图。

你可以通过点击火焰图中的某个函数，来查看它的调用栈。

**你可以右键下面的图片，在新的标签页打开，体验交互效果。**

![](https://web-static2.dragonos.org.cn//longjin/flame2.svg?imageSlim)

