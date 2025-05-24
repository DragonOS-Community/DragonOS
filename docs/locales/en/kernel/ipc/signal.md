:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/ipc/signal.md

- Translation time: 2025-05-19 01:41:43

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Signal Signal

:::{note}
This document Maintainer: Longjin

Email: <longjin@RinGoTek.cn>
:::

&emsp;&emsp;Signals are a mechanism for inter-process communication. When a signal is sent to a specific process, it can trigger a specific behavior (such as exiting the program or running a signal handler). Signals are asynchronous notifications sent to a process or a specific thread within the same process, used to notify it that an event has occurred. Common uses of signals include interrupting, suspending, terminating, or ending a process. When sending a signal, the operating system interrupts the normal execution flow of the target process to deliver the signal. Execution can be interrupted at any non-atomic instruction. If the process has previously registered a signal handler, the handler routine is executed. Otherwise, the default signal handler is executed.

&emsp;&emsp;Signals are similar to interrupts, with the difference being that interrupts are mediated by the CPU and handled by the kernel, while signals are generated within the kernel (and can also be generated through system calls) and are handled by the default handlers of individual processes or the kernel.

## 1. Overview of Signal Handling

### 1.1 Signal Sending

&emsp;&emsp;When process A wants to send a signal to process B, it uses the `kill(pid, signal)` interface to send the signal. Then, it enters the `sys_kill()` function in the kernel for processing. The kernel will then add the signal to the `sigpending` in the target process's PCB.

Illustration:

```text
   ┌────────────┐
   │ Process A: │
   │            │
   │  sys_kill  │
   └──────┬─────┘
          │
          │
   ┌──────▼──────┐    ┌────────────────────┐
   │ Send Signal ├────►Add to sigpending of│
   └─────────────┘    │   process B.       │
                      └────────────────────┘

```

### 1.2 Signal Handling

&emsp;&emsp;When a process exits the kernel mode, it jumps into the `do_signal()` function to check if there are any signals that need to be handled. If there are, the signal handling process is initiated.

Signal handling process illustration:

```text

 ┌───────────────────────┐
 │       Process B:      │
 │                       ◄─────────────────────────────────┐
 │ Return from syscall...│                                 │
 └─────────┬─────────────┘                                 │
           │                                               │
           │                                               │
           │                ┌────────────────┐             │
     ┌─────▼─────┐ default  │                │             │
     │ do_signal ├────────► │ stop process B.│             │
     └─────┬─────┘  action  │                │             │
           │                └────────────────┘             │
           │ custom action                                 │
    ┌──────▼───────┐                                       │
    │ setup signal │                                       │
    │    frame     │                                       │
    └──────┬───────┘                                       │
           │jump to                                        │
    ┌──────▼───────┐ ┌────────────┐ sys_sigreturn ┌────────┴────────┐
    │   userland   ├─►sa_restorer ├──────────────►│Restore the stack│
    │ sig handler  │ └────────────┘               │    frame.       │
    └──────────────┘                              └─────────────────┘

```

- If the kernel checks and finds that the process has not specified a signal handler and the signal handling action is not "ignore", the process will be terminated.
- If the kernel finds that the signal is not ignored, it will:
    - Save the current kernel stack
    - Set up the user-mode stack frame for signal handling
    - Return to user mode and execute the signal handler
    - After the signal handler finishes, it will enter the __sa_restorer__ provided by libc, initiating the `sys_sigreturn()` system call to return to kernel mode
    - The kernel restores the kernel stack before handling the signal.
    - The signal handling process ends, and the kernel continues with the process of returning to user mode.
- If the kernel finds that the current signal is being ignored, it checks the next signal.
- If no signals need to be handled, it returns to user mode.

## 2. Other Issues

&emsp;&emsp;None at present.
