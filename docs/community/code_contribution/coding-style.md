# 代码风格

&emsp;&emsp;这份文档将会简要的介绍DragonOS的代码风格。每个人的代码风格都各不相同，这是一件非常正常的事情。但是，对于一个开源项目的可维护性而言，我们希望制定一些代码规范，以便包括您在内的每个开发者都能在看代码的时候更加舒服。一个充斥着各种不同代码风格的项目，是难以维护的。

&emsp;&emsp;我们在这里提出一些建议，希望您能够尽量遵循这些建议。这些建议与Linux的代码规范相似，但又略有不同。



## 0. 代码格式化工具

&emsp;&emsp;在提出下面的建议之前，我们建议您在开发的时候使用Visual Studio Code的`C/C++ Extension Pack`插件作为代码格式化工具。这些插件能为您提供较好自动格式化功能，使得您的代码的基本格式符合DragonOS的要求。

&emsp;&emsp;当您在编码时，经常性的按下`Ctrl+shift+I`或您设置的代码格式化快捷键，能帮助您始终保持良好的代码格式。

## 1. 缩进

&emsp;&emsp;一个制表符的宽度等于4个空格。代码的缩进是按照制表符宽度(在多数编辑器上为4个字符)进行缩进的。

&emsp;&emsp;这样能够使得您的代码变得更加容易阅读，也能更好的看出代码的控制结构。这样能避免很多不必要的麻烦！

举个例子：在switch语句中，将switch和case放置在同一缩进级别。并且将每个case的代码往右推进一个tab。这样能让代码可读性变得更好。

```c
switch (cmd)
{
case AHCI_CMD_READ_DMA_EXT:
    pack->blk_pak.end_handler = NULL;
    pack->blk_pak.cmd = AHCI_CMD_READ_DMA_EXT;
    break;
case AHCI_CMD_WRITE_DMA_EXT:
    pack->blk_pak.end_handler = NULL;
    pack->blk_pak.cmd = AHCI_CMD_WRITE_DMA_EXT;
    break;
default:
    pack->blk_pak.end_handler = NULL;
    pack->blk_pak.cmd = cmd;
    break;
}
```

## 2. 分行

&emsp;&emsp;我们建议，每行不要超过120个字符。如果超过了，除非有必要的理由，否则应当将其分为两行。

&emsp;&emsp;在分行时，我们需要从被分出来的第二行开始，比第一行的起始部分向右进行一个缩进，以表明这是一个子行。使用代码格式化的快捷键能让你快速完成这件事。

&emsp;&emsp;对于一些日志字符串而言，为了能方便的检索到他们，我们不建议对其进行分行。

&emsp;&emsp;对于代码的分行，请不要试图通过以下的方式将几个语句放置在同一行中，这样对于代码可读性没有任何好处：

```c
// 错误示范(1)
if(a) return 1;

// 错误示范(2)
if(b)
    do_a(),do_b();
```

## 3. 大括号和空格

### 3.1 大括号

&emsp;&emsp;大括号的放置位置的选择是因人而异的，主要是习惯原因，而不是技术原因。我们推荐将开始括号和结束括号都放置在一个新的行首。如下所示：


```c
while(i<10)
{
    ++i;
}
```

&emsp;&emsp;这种规定适用于所有的代码块。

&emsp;&emsp;这么选择的原因是，在一些编辑器上，这样放置括号，**编辑器上将会出现辅助的半透明竖线，且竖线两端均为括号**。这样能帮助开发者更好的定位代码块的层次关系。

下面通过一些例子来演示：

&emsp;&emsp;在下面这个代码块中，我们需要注意的是，`else if`语句需要另起一行，而不是跟在上一个`}`后方。这是因为我们规定`{`必须在每行的起始位置，并且还要保持缩进级别的缘故。

```c
if (*fmt == '*')
{
    ++fmt;
}
else if (is_digit(*fmt))
{
    field_width = skip_and_atoi(&fmt);
}
```

&emsp;&emsp;当循环中有多个简单的语句的时候，需要使用大括号。

```c
while (condition) 
{
    if (test)
        do_something();
}
```

&emsp;&emsp;当语句只有1个简单的子句时，我们不必使用大括号。

```c
if(a)
    return 1;
```

### 3.2 空格

&emsp;&emsp;对于大部分关键字，我们需要在其后添加空格，以提高代码的可读性。

&emsp;&emsp;请您在所有这些关键字后面输入一个空格：

```c
if, switch, case, for, do, while
```

&emsp;&emsp;关键字sizeof、typeof、alignof、__atrribute__的后面则不需要添加空格，因为使用他们的时候，就像是使用函数一样。


&emsp;&emsp;对于指针类型的变量，`*`号要贴近变量名而不是贴近类型名。如下所示：
```c
char *a;
void *func(char* s, int **p);
```

&emsp;&emsp;在大多数二元和三元运算符周围（在每一侧）使用一个空格，如下所示：

```c
=  +  -  <  >  *  /  %  |  &  ^  <=  >=  ==  !=  ?  :
```

&emsp;&emsp;这些一元运算符后方没有空格

```c
&  *  +  -  ~  !  sizeof  typeof  alignof  __attribute__  defined
```

&emsp;&emsp;特殊的例子，以下运算符的前后都不需要空格：
```c
++  -- . ->
```

【文档未完成，待继续完善】