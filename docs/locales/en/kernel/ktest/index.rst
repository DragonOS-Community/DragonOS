.. note:: AI Translation Notice

   This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

   - Source document: kernel/ktest/index.rst

   - Translation time: 2025-05-19 01:41:16

   - Translation model: `Qwen/Qwen3-8B`


   Please report issues via `Community Channel <https://github.com/DragonOS-Community/DragonOS/issues>`_

====================================
Kernel Testing
====================================

   This chapter will introduce how to test the kernel, including manual testing and automated testing.

   We need to perform thorough testing on the kernel as much as possible, so that we can better ensure the stability of the kernel and reduce the difficulty of debugging other modules.

   Setting up comprehensive test cases can help us detect problems as much as possible, preventing us from being "stabbed" by hidden bugs in existing modules when writing new modules.

   Since it is difficult to debug using tools like GDB, manual testing in the kernel is more challenging compared to testing applications.

   For some modules, we can write code for unit testing and output error messages. Unfortunately, not all modules can be unit tested. For example, common modules such as memory management and process management cannot be unit tested.
