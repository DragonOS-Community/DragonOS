.. note:: AI Translation Notice

   This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

   - Source document: kernel/trace/index.rst

   - Translation time: 2025-06-14 09:35:32

   - Translation model: `Qwen/Qwen3-8B`


   Please report issues via `Community Channel <https://github.com/DragonOS-Community/DragonOS/issues>`_

Kernel Tracing Mechanism
====================================

   Kernel tracing facilities make system runtime behavior observable. They include predefined tracepoints, dynamic kprobes, eBPF-based extensibility, and runtime text patching for low-overhead feature switches. This chapter describes the tracing facilities currently implemented by DragonOS and how they work.

.. toctree::
   :maxdepth: 1
   :caption: Contents

   tracepoint
   text_patching
   kprobe
   eBPF
