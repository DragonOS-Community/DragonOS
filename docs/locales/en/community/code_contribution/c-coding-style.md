:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: community/code_contribution/c-coding-style.md

- Translation time: 2025-05-19 01:42:01

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# C Language Code Style

&emsp;&emsp;This document will briefly introduce the C language code style used in DragonOS. It is completely normal for each person to have their own code style. However, for the maintainability of an open-source project, we hope to establish some code standards so that every developer, including you, can feel more comfortable when reading the code. A project filled with various code styles is difficult to maintain.

&emsp;&emsp;We propose some recommendations here, and we hope you will follow them as much as possible. These recommendations are similar to those of Linux, but with some differences. DragonOS uses Linux's style for variable naming; for indentation, DragonOS uses Microsoft's style.

## 0. Code Formatter

&emsp;&emsp;Before we present the following recommendations, we recommend that you use the `C/C++ Extension Pack` plugin in Visual Studio Code as a code formatter during development. These plugins provide good auto-formatting functionality, ensuring that your code's basic format meets DragonOS's requirements.

&emsp;&emsp;Pressing `Ctrl+shift+I` or your set code formatting shortcut frequently while coding can help you maintain good code formatting consistently.

## 1. Indentation

&emsp;&emsp;The width of a tab is equal to 4 spaces. Code indentation is based on the tab width (usually 4 characters in most editors).

&emsp;&emsp;This makes your code more readable and helps better identify the control structures in the code. This can avoid many unnecessary troubles!

For example: In a switch statement, place the switch and case on the same indentation level. And indent each case's code by one tab to the right. This improves code readability.

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

## 2. Line Breaks

&emsp;&emsp;We recommend that each line should not exceed 120 characters. If it does, unless there is a necessary reason, it should be split into two lines.

&emsp;&emsp;When breaking lines, we need to indent the second line by one level from the first line's starting part to indicate that it is a sub-line. Using the code formatting shortcut can quickly accomplish this.

&emsp;&emsp;For log strings, we do not recommend breaking them into multiple lines for easier retrieval.

&emsp;&emsp;For code line breaks, do not try to place several statements on the same line, as this provides no benefit to code readability:

```c
// 错误示范(1)
if(a) return 1;

// 错误示范(2)
if(b)
    do_a(),do_b();
```

## 3. Braces and Spaces

### 3.1 Braces

&emsp;&emsp;The placement of braces is a matter of personal preference, mainly due to habit rather than technical reasons. We recommend placing the opening and closing braces on new lines, as shown below:

```c
while(i<10)
{
    ++i;
}
```

&emsp;&emsp;This rule applies to all code blocks.

&emsp;&emsp;The reason for this choice is that, in some editors, placing the braces in this way will result in **a semi-transparent vertical line appearing in the editor, with the line ends being the braces**. This helps developers better understand the hierarchical relationships of the code blocks.

Let's demonstrate this with some examples:

&emsp;&emsp;In the following code block, we need to note that the `else if` statement should be on a new line, not after the previous `}`. This is because we require `{` to be at the start of each line and maintain the indentation level.

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

&emsp;&emsp;When there are multiple simple statements in a loop, braces should be used.

```c
while (condition) 
{
    if (test)
        do_something();
}
```

&emsp;&emsp;When there is only one simple statement, we do not need to use braces.

```c
if(a)
    return 1;
```

### 3.2 Spaces

&emsp;&emsp;For most keywords, we need to add a space after them to improve code readability.

&emsp;&emsp;Please add a space after all of these keywords:

```c
if, switch, case, for, do, while
```

&emsp;&emsp;Keywords such as sizeof, typeof, alignof, and __attribute__ do not require a space after them, as they are used like functions.

&emsp;&emsp;For pointer-type variables, the asterisk should be close to the variable name rather than the type name. As shown below:

```c
char *a;
void *func(char* s, int **p);
```

&emsp;&emsp;Use a space on both sides of most binary and ternary operators, as shown below:

```c
=  +  -  <  >  *  /  %  |  &  ^  <=  >=  ==  !=  ?  :
```

&emsp;&emsp;There is no space after these unary operators:

```c
&  *  +  -  ~  !  sizeof  typeof  alignof  __attribute__  defined
```

&emsp;&emsp;Special cases: no space is needed before or after the following operators:

```c
++  -- . ->
```

## 4. Naming

&emsp;&emsp;DragonOS does not use the camelCase naming convention for function names, but instead uses concise and clear names like `tmp`.

&emsp;&emsp;Note that this refers to our entire project not using the camelCase naming convention. It does not mean that programmers can use obscure abbreviations for variable names.

&emsp;&emsp;For global variables or globally visible functions and structures, we need to follow the following naming conventions:

- The name should be easy to understand and not ambiguous. For example, for a function that calculates folder size, we recommend using `count_folder_size()` instead of `cntfs()`, which can confuse others.
- For global, non-static names, unless there is a special need, the naming should follow the format: `模块名缩写前缀_函数/变量名`. This naming convention helps others distinguish which module the name belongs to and reduces the risk of naming conflicts.
- Global names that do not need to be visible to other code files must be prefixed with the `static` modifier.

&emsp;&emsp;For local variables within functions, the naming convention should be concise. Long names for local variables have little significance.

[Document not completed, to be continued]
