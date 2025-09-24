.. note:: AI Translation Notice

   This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

   - Source document: kernel/ktest/index.rst

   - Translation time: 2025-09-24 08:26:52

   - Translation model: `hunyuan-turbos-latest`


   Please report issues via `Community Channel <https://github.com/DragonOS-Community/DragonOS/issues>`_

====================================
Kernel Testing
====================================

This chapter introduces how to test the kernel, including both manual and automated testing.

We need to conduct as comprehensive testing of the kernel as possible to better ensure its stability and reduce the debugging difficulty of other modules.

Establishing well-designed test cases helps us detect issues as much as possible, preventing us from being "ambushed" by deeply hidden bugs in existing modules when developing new ones.

Since it is difficult to debug using tools like GDB, manual testing in the kernel is more challenging than application testing.

For some modules, we can write code for unit testing and output exception information. Unfortunately, not all modules can be unit tested. For example, common modules like memory management and process management cannot be unit tested.
