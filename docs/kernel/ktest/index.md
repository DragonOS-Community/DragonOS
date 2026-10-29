# Kernel Testing

   This section will introduce how to test the kernel, including both manual and automated testing.

   We need to conduct as comprehensive testing of the kernel as possible to better ensure its stability and reduce the debugging difficulty of other modules.

   Setting up well-designed test cases can help us detect issues to the greatest extent, preventing us from being "ambushed" by deeply hidden bugs in existing modules when developing new ones.

   Since it is difficult to use debugging tools like GDB, manual testing in the kernel is more challenging than application testing.

   For some modules, we can write code for unit testing and output exception information. Unfortunately, not all modules can be unit tested. For example, common modules like memory management and process management cannot be unit tested.
