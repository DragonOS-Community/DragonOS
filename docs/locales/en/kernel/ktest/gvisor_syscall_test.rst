.. note:: AI Translation Notice

   This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

   - Source document: kernel/ktest/gvisor_syscall_test.rst

   - Translation time: 2025-11-25 16:01:36

   - Translation model: `hunyuan-turbos-latest`


   Please report issues via `Community Channel <https://github.com/DragonOS-Community/DragonOS/issues>`_

==============================
gVisor System Call Testing
==============================

DragonOS integrates the gVisor system call test suite to verify the compatibility and correctness of the operating system's system call implementations.

Overview
========

gVisor is a container runtime sandbox developed by Google, which includes a comprehensive set of system call compatibility tests. These tests are designed to validate whether an operating system's system call implementations comply with Linux standards.

Key Features:

- **Comprehensive Test Coverage**: Contains hundreds of system call test cases
- **Whitelist Mechanism**: By default, only verified tests are executed, with support gradually expanding
- **Blacklist Filtering**: Allows blocking specific test cases for each test program
- **Automated Execution**: Provides Makefiles and scripts to simplify the testing process

Automated Testing
==========

Execute the `make test-syscall` command. This command will launch DragonOS and automatically execute the gvisor syscall test suite. After testing completes, it will exit qemu. The command will return success or failure based on the test case success rate - returning failure if the success rate is not 100%. The execution flow of this command is as follows:

1. Execute `toggle_compile_gvisor.sh enable` to comment out the gvisor test suite-related blocking configurations in `app-blocklist.toml`
2. Compile DragonOS
3. Write the image
4. Start DragonOS in the background in qemu's non-graphical mode, while setting environment variables `AUTO_TEST` (auto-test option, currently only supports syscall testing) and `SYSCALL_TEST_DIR` (directory where the test suite is located). These two environment variables will be passed to DragonOS as command-line parameters. Then, when the busybox init process executes the rcS script, this script will execute the corresponding tests through the `AUTO_TEST` option
5. Execute `monitor_test_results.sh` to periodically check the qemu serial port output content and determine whether to return success or failure based on the test results
6. Execute `toggle_compile_gvisor.sh disable` to uncomment the relevant configurations in `app-blocklist.toml`, restoring the default blocking state

The corresponding workflow configuration file is `test-x86.yml`

Manual Testing
==========

1. Enter the test directory:

   .. code-block:: bash

      cd user/apps/tests/syscall/gvisor

2. Run whitelist tests in Linux (automatically downloads the test suite):

   .. code-block:: bash

      make test

3. If you need to run tests, you can quickly modify configurations via scripts:

   .. code-block:: bash

      # Enable gVisor testing (comment out blocklist configurations)
      bash user/apps/tests/syscall/gvisor/toggle_compile_gvisor.sh enable

      # Restore default blocking after testing
      bash user/apps/tests/syscall/gvisor/toggle_compile_gvisor.sh disable

4. Run tests within the DragonOS system:

   Enter the installation directory and run the test program:

   .. code-block:: bash

      cd /opt/tests/gvisor
      ./gvisor-test-runner --help

   Use `_translated_label__`./gvisor-test-runner`_en` to run specific test cases.

5. View detailed documentation:

   Please refer to `user/apps/tests/syscall/gvisor/README.md` for complete usage instructions.

Testing Mechanism
==========

Whitelist Mode
-----------

The test framework enables whitelist mode by default, running only the test programs specified in `_translated_label__`whitelist.txt`_en`. This allows for gradual validation of DragonOS's system call implementations.

Blacklist Filtering
-----------

For each test program, specific test cases can be blocked through files in the `_translated_label__`blocklists/`_en` directory. This is particularly useful for skipping tests that are not yet supported or are unstable.

More Detailed Information
==============

For detailed usage methods, configuration options, and development guides regarding gVisor system call testing, please consult the README.md documentation in the test directory:

- Documentation location: `user/apps/tests/syscall/gvisor/README.md`
