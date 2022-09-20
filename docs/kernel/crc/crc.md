# crc校验（查表法）

## 简介

&emsp;&emsp;由于计算机更方便实现固定位数的读取，比起传统的逐位循环冗余校验，crc的查表法更有优势

## 相关名词（建议百度）

&emsp;&emsp;为了方便解释，给一个参数模型的例子

&emsp;&emsp;参数模型：CRC16/MAXIM    x16+x15+x2+1

&emsp;&emsp;宽度：16

&emsp;&emsp;多项式（Hex）：0x8005

&emsp;&emsp;初始值（Hex）：0x0000

&emsp;&emsp;结果异或值（Hex）：0xFFFF

### 宽度

&emsp;&emsp;简单来说就是规定了其他参数的的位数。比如crc16,宽度就是16，其计算出来的结果、多项式、初始值和结果异或值都是个16位的二进制数

### 多项式

&emsp;&emsp;“x16+x15+x2+1”为例子中的多项式，将其写成二进制时其实是17位数，按照约定会忽略最高位，然后将剩下的16位写作形如“0x8005”这样的十六进制数

### 初始值

&emsp;&emsp;即crc一开始被初始化的值，可以自定义，也可以按照一些参数模型来赋值

### 结果异或值

&emsp;&emsp;即crc最后一步被异或的值，可以自定义，也可以按照一些参数模型来赋值

### 表

&emsp;&emsp;表是通过将多项式与各个ASCII按某种规则计算后得出的一个数组，可方便后续crc计算。至于生成方法，由于有现成的表，所以直接copy，哈哈哈哈

## 使用函数与参数介绍

&emsp;&emsp;继续用crc16作为例子

### crc16_table

**`uint16_t const crc16_table[256]`**

&emsp;&emsp;这就是表，有256个元素，因为ASCII有256个

### uint16_t crc16

**`uint16_t crc16(uint16_t crc, uint8_t const *buffer, size_t len)`**

### 描述

&emsp;&emsp;用于计算余数，输出结果为对应宽度的余数

#### 参数

**crc**

&emsp;&emsp;传入的初始值

**buffer**

&emsp;&emsp;被校验数据

**len**

&emsp;&emsp;被校验数据长度



