:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/debug/traceback.md

- Translation time: 2025-05-19 01:41:10

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Kernel Stack Traceback

## Introduction

&emsp;&emsp;The functionality of the kernel stack traceback is located in the `kernel/debug/traceback/` folder. It provides traceback capabilities for the kernel mode, printing the call stack to the screen.

---

## API

### `void traceback(struct pt_regs * regs)`

#### Purpose

&emsp;&emsp;This interface is defined in `kernel/debug/traceback/traceback.h`, which will perform a traceback on the given kernel stack and print the trace results to the screen.

#### Parameters

##### regs

&emsp;&emsp;The first stack frame of the kernel stack to start the tracing (i.e., the bottom of the stack)

---

## Implementation Principle

&emsp;&emsp;After the kernel is linked for the first time, the Makefile will run the `kernel/debug/kallsyms` program to extract the symbol table of the kernel file, and then generate `kernel/debug/kallsyms.S`. The rodata segment of this file stores the symbol table of the functions in the text segment. Then, this file will be compiled into `kallsyms.o`. Finally, the Makefile will again call the `ld` command to link the kallsyms.o into the kernel file.

&emsp;&emsp;When the `traceback` function is called, it will traverse the symbol table to find the corresponding symbols and output them.

---

## Future Development Directions

- Add the capability to write to a log file
