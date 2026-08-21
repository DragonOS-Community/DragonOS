内核跟踪机制
====================================

   内核跟踪机制用于观察系统运行状态，包括预定义的 tracepoint、动态探测的 kprobe、扩展可观测性的 eBPF，以及支撑低开销开关的运行时文本修补。本章介绍 DragonOS 当前已经实现的跟踪能力及其工作原理。
   
.. toctree::
   :maxdepth: 1
   :caption: 目录

   tracepoint
   text_patching
   kprobe
   eBPF
