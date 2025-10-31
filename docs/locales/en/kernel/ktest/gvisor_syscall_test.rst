.. note:: AI Translation Notice

   This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

   - Source document: kernel/ktest/gvisor_syscall_test.rst

   - Translation time: 2025-10-09 14:36:26

   - Translation model: `hunyuan-turbos-latest`


   Please report issues via `Community Channel <https://github.com/DragonOS-Community/DragonOS/issues>`_

==============================
gVisor System Call Testing
==============================

DragonOS integrates the gVisor system call test suite to verify the compatibility and correctness of the operating system's system call implementations.

Overview
========

gVisor is a container runtime sandbox developed by Google, which includes a comprehensive set of system call compatibility tests. These tests are designed to validate whether an operating system's system call implementation complies with Linux standards.

Key Features:

- **Comprehensive Test Coverage**: Contains hundreds of system call test cases
- **Whitelist Mechanism**: By default, only verified tests are executed, with support gradually expanding
- **Blacklist Filtering**: Allows blocking specific test cases for each test program
- **Automated Execution**: Provides Makefile and scripts to simplify the testing process

Automated Testing
==========

Execute the `make test-syscall` command. This command will launch DragonOS and automatically execute the gvisor syscall test suite. After testing completes, it will exit qemu. The return status will be success or failure based on the test case success rate - returning failure if the success rate is not 100%. The execution flow of this command is as follows:

1. Execute the configuration in `enable_compile_gvisor.sh` comment `app-blocklist.toml` regarding blocking the gvisor test suite
2. Compile DragonOS
3. Write the image
4. Start DragonOS in qemu background mode (without graphics), while setting environment variables `AUTO_TEST` (auto-test option, currently only supports syscall testing) and `SYSCALL_TEST_DIR` (test suite directory). These environment variables will be passed to DragonOS as command-line parameters. Then when the busybox init process executes the rcS script, this script will execute the corresponding test through the `AUTO_TEST` option
5. Execute `monitor_test_results.sh` to periodically check the qemu serial output content and determine success or failure return based on test results
6. Execute `disable_compile_gvisor.sh` to uncomment the configuration in `app-blocklist.toml` regarding blocking the gvisor test suite

The corresponding workflow configuration file is `test-x86.yml`

Manual Testing
==========

1. Enter the test directory:

   .. code-block:: bash

      cd user/apps/tests/syscall/gvisor

2. Run whitelist tests on Linux (automatically downloads the test suite):

   .. code-block:: bash

      make test

3. If you need to run tests, first modify the configuration file:

   Edit `config/app-blocklist.toml`, and comment out the following content:

   .. code-block:: toml

      # Block gvisor system call tests
      # [[blocked_apps]]
      # name = "gvisor syscall tests"
      # reason = "Blocked due to large file size. To allow system call tests, uncomment these lines"

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

The test framework enables whitelist mode by default, running only the test programs specified in `_translated_label__`whitelist.txt`_en`. This allows for gradual verification of DragonOS's system call implementations.

Blacklist Filtering
-----------

For each test program, specific test cases can be blocked through files in the `_translated_label__`blocklists/`_en` directory. This is particularly useful for skipping unsupported or unstable tests.

More Detailed Information
==============

For detailed usage methods, configuration options, and development guides regarding gVisor system call testing, please consult the README.md documentation in the test directory:

- Documentation location: `user/apps/tests/syscall/gvisor/README.md`
