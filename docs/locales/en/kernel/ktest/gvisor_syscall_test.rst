.. note:: AI Translation Notice

   This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

   - Source document: kernel/ktest/gvisor_syscall_test.rst

   - Translation time: 2025-12-23 04:28:32

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

Execute the `make test-syscall` command. This command will launch DragonOS and automatically execute the gvisor syscall test suite. After testing completes, QEMU will exit. The command returns success or failure based on the test case success rate - if the success rate is not 100%, it will return failure. The execution flow of this command is as follows:

The corresponding workflow configuration file is `test-x86.yml`

Manual Testing
==========

1. Enter the test directory:

   .. code-block:: bash

      cd user/apps/tests/syscall/gvisor

2. Run whitelist tests in Linux (automatically downloads the test suite):

   .. code-block:: bash

      make test

3. If you need to run tests, you can quickly modify configurations via script:

   .. code-block:: bash

      # Enable gVisor tests (uncomment blocklist configuration)
      bash user/apps/tests/syscall/gvisor/toggle_compile_gvisor.sh enable

      # Restore default blocking after testing
      bash user/apps/tests/syscall/gvisor/toggle_compile_gvisor.sh disable

4. Run tests within the DragonOS system:

   Navigate to the installation directory and run the test program:

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

The test framework enables whitelist mode by default, running only the test programs specified in `_translated_label__`whitelist.txt`_en`. This allows for gradual validation of DragonOS's system call implementation.

Blacklist Filtering
-----------

For each test program, specific test cases can be blocked through files in the `_translated_label__`blocklists/`_en` directory. This is particularly useful for skipping tests that are not currently supported or are unstable.

Test Case Fixing Guide
============

This section explains how to fix gvisor test cases, helping developers quickly participate in system call repair work.

Preparation
--------

1. **Initial Installation of Test Cases**

   Before starting repair work, execute `make test-syscall` once to ensure the test cases are installed in the DragonOS system:

   .. code-block:: bash

      make test-syscall

   This command downloads the test suite, compiles DragonOS, writes to the image, and automatically runs the tests. After the first execution, the test cases are installed in the `_translated_label__`/opt/tests/gvisor`_en` directory of DragonOS.

2. **Clone the Test Case Source Code Repository**

   The source code of the test cases is located in the gvisor repository, which needs to be cloned locally for viewing and fixing:

   .. code-block:: bash

      git clone https://cnb.cool/DragonOS-Community/gvisor.git
      cd gvisor
      git checkout dragonos/release-20250616.0

   The test case code is located in the `_translated_label__`test/syscalls/linux`` 目录下，使用gtest框架编写，每个系统调用对应一个或多个 ``.cc`_en` file.

Test Execution Flow
------------

1. **Enter the DragonOS System**

   Use the following command to start DragonOS in non-graphical mode:

   .. code-block:: bash

      make run-nographic

2. **Execute Tests to View Errors**

   Within the DragonOS system, navigate to the test case directory and execute the specific test program:

   .. code-block:: bash

      cd /opt/tests/gvisor
      ./<test_name>

   For example, to test socket-related system calls:

   .. code-block:: bash

      ./socket_test

   After executing the test, you will see the test results. Failed test cases will display detailed error information, including:

   - Name of the failed test case
   - Error reason (such as system call returning an error code, behavior not meeting expectations, etc.)

   If the test case hangs (no response or long delay), you need to view stack trace information to locate the problem:

   .. code-block:: bash

      # In a new terminal window while DragonOS is running
      # Execute in the DragonOS project root directory
      make gdb

   In gdb, first select the corresponding vcpu thread, then view the stack trace:

   .. code-block:: gdb

      # View all threads
      info threads

      # Select the corresponding vcpu thread (e.g., thread 2)
      thread 2

      # View stack trace
      bt

      # Or view complete stack trace (including local variables)
      bt full

   **Note**: DragonOS is a multi-core system and may have multiple vcpu threads. Select the correct thread to view the stack trace based on the actual situation.

3. **Record Failed Test Cases**

   Record the names of failed test cases, error information, and stack trace information, which will be used for subsequent repair work.

Code Repair Process
------------

1. **Copy Test Case Source Code**

   From the cloned gvisor repository, find the corresponding test case source code file (usually `_translated_label__`<test_name>.cc`_en`), and copy it to the DragonOS project root directory:

   .. code-block:: bash

      cp /path/to/gvisor/test/syscalls/linux/<test_name>.cc /path/to/DragonOS/

   For example, to fix socket_test:

   .. code-block:: bash

      cp gvisor/test/syscalls/linux/socket_test.cc /home/jinlong/code/DragonOS/

2. **Use AI Assistant for Repair**

   Provide the copied `_translated_label__`.cc`_en` file to an AI assistant (such as Cursor's AI function), and explain:

   - The specific error information of the test failure
   - Request the AI to follow Linux semantics to fix the test case
   - Characteristics of DragonOS's system call implementation (refer to Linux 6.6 semantics)

   The AI assistant will analyze the code, identify the issues, and provide repair suggestions or directly fix the code.

   Some small suggestions:

   - If the problem is complex, we recommend first using Plan mode to develop a repair plan, then proceed with the fix.
   - If the AI assistant has difficulty locating the issue, ask it to add minimal logging at appropriate kernel locations to assist in diagnosis
   - If terminal logs are truncated, check serial_opt.txt for complete logs and share them with the AI assistant
   - If the AI assistant struggles to locate the issue, have it write simple test programs under `user/apps/c_unitest` to address the problem. These test programs will be compiled and installed in DragonOS's bin directory.
   - Always remind the AI assistant to look for reusable code and establish reasonable, appropriate abstractions. Otherwise, the code may become redundant!

3. **Verify Repair Effectiveness**

   After repairs are complete, recompile DragonOS and test again:

   .. code-block:: bash

      # In the DragonOS project root directory
      make run-nographic

   Then re-execute the test within DragonOS to confirm the repair was successful.

Whitelist Management
----------

Fixed test cases need to be added to the whitelist to be executed by the test framework.

1. **Edit the Whitelist File**

   Open the `_translated_label__`user/apps/tests/syscall/gvisor/whitelist.txt`_en` file and add the test program name:

   .. code-block:: text

      # File system related tests
      open_test
      stat_test
      socket_test  # Newly added test

2. **Whitelist Format Explanation**

   - One test program name per line (without path or extension)
   - Lines starting with `_translated_label__`#`_en` are comments and will be ignored
   - Empty lines are ignored
   - Can be organized by functionality, using comments for grouping

3. **Notes**

   - The test program name must exactly match the executable file name
   - After adding, you need to re-run `make test-syscall` for changes to take effect
   - It's recommended to verify the test runs properly before adding

Blacklist Management
----------

If certain test cases within a test program cannot be fixed temporarily or don't need to be supported, you can create a blocklist file to block these test cases.

1. **When to Create a Blocklist**

   - Test cases depend on features not yet supported by DragonOS (such as IPv6, certain filesystem features, etc.)
   - Test cases have known instability or race conditions
   - Test cases require special permissions or environment configurations not currently available

2. **Create a Blocklist File**

   Create a file with the same name as the test program in the `_translated_label__`user/apps/tests/syscall/gvisor/blocklists/`_en` directory:

   .. code-block:: bash

      # For example, to create a blocklist for socket_test
      touch user/apps/tests/syscall/gvisor/blocklists/socket_test

3. **Blocklist File Format**

   Add the names of the test cases to be blocked in the file, one per line:

   .. code-block:: text

      # This is a comment line and will be ignored

      # Block IPv6 related tests
      SocketTest.IPv6*
      SocketTest.IPv6Socket*

      # Block tests requiring special permissions
      SocketTest.PrivilegedSocket

      # Block known unstable tests
      SocketTest.UnimplementedFeature

4. **Format Explanation**

   - Supports wildcard `_translated_label__`*`_en` to match any characters
   - Test case names typically follow the format `_translated_label__`TestSuite.TestCase`_en`
   - Lines starting with `_translated_label__`#`_en` are comments
   - Empty lines are ignored
   - **Important**: Must note the reason for blocking in the file for future maintenance

5. **Examples**

   Refer to existing blocklist files as examples:

   - `_translated_label__`user/apps/tests/syscall/gvisor/blocklists/socket_test`_en` - Blacklist for socket tests
   - `_translated_label__`user/apps/tests/syscall/gvisor/blocklists/epoll_test`_en` - Blacklist for epoll tests

Complete Workflow Example
--------------

The following is a complete repair workflow example, using `_translated_label__`open_test`_en` as an example:

1. **Preparation Work**

   .. code-block:: bash

      # Initial installation of test cases (if not already installed)
      make test-syscall

      # Clone the test case source code
      git clone https://cnb.cool/DragonOS-Community/gvisor.git
      cd gvisor
      git checkout dragonos/release-20250616.0

2. **Identify the Problem**

   .. code-block:: bash

      # Enter DragonOS
      make run-nographic

      # Execute the test within DragonOS
      cd /opt/tests/gvisor
      ./open_test

      # Identify some failing test cases and record error information

3. **Fix the Code**

   .. code-block:: bash

      # Copy the source code to the DragonOS root directory
      cp gvisor/test/syscalls/linux/open_test.cc /home/jinlong/code/DragonOS/

      # Use AI assistant to fix (open the file in Cursor and provide error information)
      # The AI will analyze the code and provide a fix

4. **Verify the Fix**

   .. code-block:: bash

      # Recompile
      make kernel
      make run-nographic

      # Test again
      cd /opt/tests/gvisor
      ./open_test

      # Confirm all test cases pass

5. **Add to Whitelist**

   .. code-block:: bash

      # Edit the whitelist file
      vim user/apps/tests/syscall/gvisor/whitelist.txt

      # Add: open_test

6. **Handle Unfixable Test Cases**

   If some test cases cannot be fixed temporarily (such as those depending on unimplemented features), create a blocklist:

   .. code-block:: bash

      # Create a blocklist file
      vim user/apps/tests/syscall/gvisor/blocklists/open_test

      # Add content:
      # Block tests depending on O_TMPFILE (not supported by DragonOS)
      OpenTest.Tmpfile*
      # Reason: O_TMPFILE requires specific filesystem support, currently not implemented

      # Block tests requiring special permissions
      OpenTest.Privileged*

      # Block known unstable tests
      OpenTest.UnstableFeature

7. **Final Verification**

   .. code-block:: bash

      # Recompile and run the complete test suite
      make test-syscall

      # Confirm open_test runs properly, with failed cases blocked by blocklist

Notes
--------

- **Comply with Linux Semantics**: Ensure system call behavior matches Linux 6.6 semantics during fixes; avoid using workarounds
- **In-depth Analysis**: Before fixing, conduct thorough root cause analysis, comparing test case code, DragonOS implementation, and Linux behavior
- **Test Validation**: Retest after each fix to ensure effectiveness and no new issues introduced
- **Documentation**: Record blocking reasons in detail in blocklists for future maintenance and re-enabling
- **Code Quality**: Ensure repaired code meets DragonOS coding standards; run `_translated_label__`make fmt`_en` for formatting

More Detailed Information
==============

For detailed usage methods, configuration options, and development guides for gVisor system call testing, please refer to the README.md document in the test directory:

- Document location: `user/apps/tests/syscall/gvisor/README.md`
