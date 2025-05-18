:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/trace/kprobe.md

- Translation time: 2025-05-19 01:54:55

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# kprobe

> Author: Chen Linfeng  
> Email: chenlinfeng25@outlook.com

## Overview

The Linux kprobes debugging technology is a lightweight kernel debugging technique specifically designed by kernel developers to facilitate tracking the execution status of kernel functions. Using kprobes technology, kernel developers can dynamically insert probe points in most specified functions of the kernel to collect the required debugging status information, with minimal impact on the original execution flow of the kernel.

The kprobes technology relies on hardware architecture support, mainly including CPU exception handling and single-step debugging mechanisms. The former is used to cause the program's execution flow to enter the user-registered callback function, while the latter is used for single-step execution of the probed instruction. It is worth noting that on some architectures, the hardware does not support the single-step debugging mechanism, which can be resolved through software simulation methods (such as RISC-V).

## kprobe Workflow

<img src="/kernel/trace/kprobe_flow.png" style="zoom: 67%;"  alt="xxx"/>

1. After registering a kprobe, each registered kprobe corresponds to a kprobe structure, which records the location of the probe point and the original instruction at that location.
2. The location of the probe point is replaced with an exception instruction. When the CPU executes to this location, it will enter an exception state. On x86_64, the instruction is int3 (if the kprobe is optimized, the instruction is jmp).
3. When the exception instruction is executed, the system checks whether it is an exception installed by kprobe. If it is, the pre_handler of the kprobe is executed. Then, using the CPU's single-step debugging (single-step) feature, the relevant registers are set, and the next instruction is set to the original instruction at the probe point, returning from the exception state.
4. The system enters the exception state again. The previous step has set the single-step related registers, so the original instruction is executed and the system will enter the exception state again. At this point, the single-step is cleared, and the post_handler is executed, and the system safely returns from the exception state.
5. When unloading the kprobe, the original instruction at the probe point is restored.

The kernel currently supports x86 and riscv64. Since riscv64 does not have a single-step execution mode, we use the break exception to simulate it. When saving the probe point instruction, we additionally fill in a break instruction, allowing the execution of the original instruction to trigger the break exception again on the riscv64 architecture.

## kprobe Interfaces

```rust
pub fn register_kprobe(kprobe_info: KprobeInfo) -> Result<LockKprobe, SystemError>;
pub fn unregister_kprobe(kprobe: LockKprobe) -> Result<(), SystemError>;

impl KprobeBasic {
    pub fn call_pre_handler(&self, trap_frame: &dyn ProbeArgs) 
    pub fn call_post_handler(&self, trap_frame: &dyn ProbeArgs)
    pub fn call_fault_handler(&self, trap_frame: &dyn ProbeArgs)
    pub fn call_event_callback(&self, trap_frame: &dyn ProbeArgs) 
    pub fn update_event_callback(&mut self, callback: Box<dyn CallBackFunc>) 
    pub fn disable(&mut self) 
    pub fn enable(&mut self) 
    pub fn is_enabled(&self) -> bool
    pub fn symbol(&self) -> Option<&str>
}
```

- `call_pre_handler` Calls the user-defined callback function before the probe point instruction is executed.
- `call_post_handler` Calls the user-defined callback function after the probe point instruction has been executed in single-step mode.
- `call_fault_handler` Calls the user-defined callback function if the first two callback functions fail.
- `call_event_callback` Used to call eBPF-related callback functions, usually called in the same way as `call_post_handler` after the probe point instruction is executed in single-step mode.
- `update_event_callback` Used to update the callback function during runtime.
- `disable` and `enable` are used to dynamically disable the kprobe. After calling `disable`, the kprobe will not execute the callback function when triggered.
- `symbol` Returns the function name of the probe point.
