.. note:: AI Translation Notice

   This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

   - Source document: kernel/ktest/dunitest.rst

   - Translation time: 2026-02-14 16:07:53

   - Translation model: `hunyuan-turbos-latest`


   Please report issues via `Community Channel <https://github.com/DragonOS-Community/DragonOS/issues>`_

==============================
dunitest User-space Testing Framework
==============================

dunitest is a user-space unit testing framework for DragonOS, designed to execute C++ test cases based on Google Test and generate structured reports.

Current Status
==============

- The runner automatically discovers and executes executable files under `_translated_label__`bin/`_en`
- Default timeout is 60 seconds, which can be overridden via `_translated_label__`--timeout-sec`_en`
- Test case filtering order: white list -> block list -> --pattern
- Summary statistics are aggregated by the number of gtest test cases by default (not by test program count)
- The runner returns a non-zero value when tests fail or time out

Directory Structure and Responsibilities
========================================

.. code-block:: text

   user/apps/tests/dunitest/
   ├── runner/                 # Rust test runner
   ├── suites/                 # Test source code (organized by suite in directories)
   ├── bin/                    # Compiled outputs (automatically discovered by runner)
   ├── whitelist.txt           # Default whitelist
   ├── scripts/run_tests.sh    # System execution entry point
   └── Makefile

Key Rules
=========

1. Source code location: `_translated_label__`suites/<suite>/*.cc`_en`
2. Compilation output: `_translated_label__`bin/<suite>/<case>_test`_en`
3. Runner test case names: `_translated_label__`<suite>/<case>``（自动去掉 ``_test`_en` suffix)

Example:

- Binary: `_translated_label__`bin/demo/gtest_demo_test`_en`
- Test case name: `_translated_label__`demo/gtest_demo`_en`
- Whitelist entry: `_translated_label__`demo/gtest_demo`_en`

How to Add New Test Cases
=========================

Recommended: Place general functional tests under the `_translated_label__`normal`_en` suite first
---------------------------------------

- General/common functional test cases are recommended to be uniformly placed under `_translated_label__`suites/normal/`_en` for centralized maintenance
- Example: `_translated_label__`suites/normal/capability.cc`_en`
- In `_translated_label__`whitelist.txt`` 中对应条目写作：``normal/capability`_en`

1. Add gtest source code
-----------------

Add a new file, for example:

.. code-block:: text

   suites/normal/capability.cc

2. Add the suite to Makefile
-------------------------

Edit `_translated_label__`user/apps/tests/dunitest/Makefile`` 的 ``SUITES`_en`:

.. code-block:: makefile

   # If new directories are added, include them here
   SUITES = demo normal

3. Build and run (supports parallel execution)
----------------------

At the repository root:

.. code-block:: bash

   make test-dunit-local

Or in the dunitest directory:

.. code-block:: bash

   make run -j$(nproc)

Build log example:

.. code-block:: text

   Compiling test case: suites/normal/capability.cc -> bin/normal/capability_test

4. Add to whitelist
--------------------------

Edit `_translated_label__`whitelist.txt`_en`, one test case name per line:

.. code-block:: text

   demo/gtest_demo
   normal/capability

Runner Parameters
=================

.. code-block:: text

   dunitest-runner [OPTIONS]

     --bin-dir <PATH>       Test binary directory (default: bin)
     --timeout-sec <SEC>    Single test timeout in seconds (default: 60)
     --whitelist <PATH>     Whitelist path (default: whitelist.txt)
     --blocklist <PATH>     Blocklist path (default: blocklist.txt)
     --results-dir <PATH>   Report directory (default: results)
     --list                 List test cases only, do not execute
     --verbose              Verbose output
     --pattern <PATTERN>    Name substring filter (can be specified multiple times)

Report Output
=============

After execution, the following are generated under `_translated_label__`results/`_en`:

- `_translated_label__`test_report.txt`_en`: Text report
- `_translated_label__`summary.json`_en`: JSON summary
- `_translated_label__`failed_cases.txt`_en`: List of failures/timeouts
- `_translated_label__`<case>.log`_en`: Individual test logs

Terminal summary explanation:

- `_translated_label__`总测试数/通过/失败/跳过`_en` is counted by the number of gtest test cases
- When a program does not produce gtest statistical information, it falls back to counting by test program granularity

Installation Instructions
=========================

Execute in `_translated_label__`user/apps/tests/dunitest/`` 目录下执行 ``make install`_en` to complete installation.
