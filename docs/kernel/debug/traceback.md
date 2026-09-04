# Kernel Stack Traceback

## Introduction

The functionality of the kernel stack traceback is located in the `kernel/debug/traceback/` folder. It provides traceback capabilities for the kernel mode, printing the call stack to the screen.

---

## API

### `void traceback(struct pt_regs * regs)`

#### Purpose

This interface is defined in `kernel/debug/traceback/traceback.h`, which will perform a traceback on the given kernel stack and print the trace results to the screen.

#### Parameters

##### regs

The first stack frame of the kernel stack to start the tracing (i.e., the bottom of the stack)

---

## Implementation Principle

After the kernel is linked for the first time, the Makefile will run the `kernel/debug/kallsyms` program to extract the symbol table of the kernel file, and then generate `kernel/debug/kallsyms.S`. The rodata segment of this file stores the symbol table of the functions in the text segment. Then, this file will be compiled into `kallsyms.o`. Finally, the Makefile will again call the `ld` command to link the kallsyms.o into the kernel file.

When the `traceback` function is called, it will traverse the symbol table to find the corresponding symbols and output them.

---

## Future Development Directions

- Add the capability to write to a log file
