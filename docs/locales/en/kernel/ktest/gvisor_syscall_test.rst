.. note:: AI Translation Notice

   This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

   - Source document: kernel/ktest/gvisor_syscall_test.rst

   - Translation time: 2025-09-24 08:27:04

   - Translation model: `hunyuan-turbos-latest`


   Please report issues via `Community Channel <https://github.com/DragonOS-Community/DragonOS/issues>`_

==============================
gVisor System Call Testing
==============================

DragonOS integrates the gVisor system call test suite to verify the compatibility and correctness of the operating system's system call implementation.

Overview
========

gVisor is a container runtime sandbox developed by Google, which includes a comprehensive set of system call compatibility tests. These tests are designed to validate whether an operating system's system call implementation complies with Linux standards.

Key Features:

- **Comprehensive Test Coverage**: Contains hundreds of system call test cases
- **Whitelist Mechanism**: By default, only verified tests are executed, with support gradually expanding
- **Blacklist Filtering**: Allows blocking specific test cases for each test program
- **Automated Execution**: Provides Makefiles and scripts to simplify the testing process

Quick Start
==========

1. Navigate to the test directory:

   .. code-block:: bash

      cd user/apps/tests/syscall/gvisor

2. Run whitelist tests on Linux (automatically downloads the test suite):

   .. code-block:: bash

      make test

3. If you need to run the tests, first modify the configuration file:

   Edit `config/app-blocklist.toml`, and comment out the following content:

   .. code-block:: toml

      # Block gvisor system call tests
      # [[blocked_apps]]
      # name = "gvisor syscall tests"
      # reason = "Blocked due to large file size. To enable system call tests, uncomment these lines"

4. Run the tests within the DragonOS system:

   Navigate to the installation directory and execute the test program:

   .. code-block:: bash

      cd /opt/tests/gvisor
      ./gvisor-test-runner --help

   Use `_translated_label__`./gvisor-test-runner`_en` to run specific test cases.

5. View detailed documentation:

   Refer to `user/apps/tests/syscall/gvisor/README.md` for complete usage instructions.

Testing Mechanism
==========

Whitelist Mode
-----------

The test framework defaults to whitelist mode, executing only the test programs specified in `_translated_label__`whitelist.txt`_en`. This allows for gradual validation of DragonOS's system call implementation.

Blacklist Filtering
-----------

For each test program, specific test cases can be blocked through files in the `_translated_label__`blocklists/`_en` directory. This is particularly useful for skipping tests that are not yet supported or are unstable.

More Details
==============

For detailed usage instructions, configuration options, and development guides regarding gVisor system call testing, please consult the README.md document in the test directory:

- Documentation Location: `user/apps/tests/syscall/gvisor/README.md`
