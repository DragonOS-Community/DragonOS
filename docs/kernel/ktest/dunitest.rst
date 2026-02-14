==============================
dunitest 用户态测试框架
==============================

dunitest 是 DragonOS 的用户态单元测试框架，用于运行基于 Google Test 的 C++ 测例并输出结构化报告。


现状
====

- runner 自动发现 ``bin/`` 下的可执行文件并执行
- 默认超时 60 秒，可通过 ``--timeout-sec`` 覆盖
- 测例过滤顺序：white list -> block list -> --pattern
- 汇总统计默认按 gtest 的测试用例数聚合（不是按测试程序个数）
- 测试失败或超时时，runner 返回非 0

目录与职责
==========

.. code-block:: text

   user/apps/tests/dunitest/
   ├── runner/                 # Rust 测试运行器
   ├── suites/                 # 测试源码（按 suite 分目录）
   ├── bin/                    # 编译产物（runner 自动发现）
   ├── whitelist.txt           # 默认白名单
   ├── scripts/run_tests.sh    # 系统内执行入口
   └── Makefile

关键规则
========

1. 源码位置：``suites/<suite>/*.cc``
2. 编译输出：``bin/<suite>/<case>_test``
3. runner 用例名：``<suite>/<case>``（自动去掉 ``_test`` 后缀）

示例：

- 二进制：``bin/demo/gtest_demo_test``
- 用例名：``demo/gtest_demo``
- white list 条目：``demo/gtest_demo``

如何新增测例
============

推荐：普通功能测试优先放在 ``normal`` suite
---------------------------------------

- 普通/通用功能测例建议统一放在 ``suites/normal/`` 下，便于集中维护
- 示例：``suites/normal/capability.cc``
- 在 ``whitelist.txt`` 中对应条目写作：``normal/capability``

1. 新增 gtest 源码
-----------------

新增文件，例如：

.. code-block:: text

   suites/normal/capability.cc

2. 把 suite 加入 Makefile
-------------------------

编辑 ``user/apps/tests/dunitest/Makefile`` 的 ``SUITES``：

.. code-block:: makefile

   # 如果新增了目录，需要在这里加入
   SUITES = demo normal

3. 构建并运行（支持并行）
----------------------

在仓库根目录：

.. code-block:: bash

   make test-dunit-local

或在 dunitest 目录：

.. code-block:: bash

   make run -j$(nproc)

构建日志示例：

.. code-block:: text

   编译测例: suites/normal/capability.cc -> bin/normal/capability_test

4. 加入 white list
--------------------------

编辑 ``whitelist.txt``，每行一个用例名：

.. code-block:: text

   demo/gtest_demo
   normal/capability

Runner 参数
===========

.. code-block:: text

   dunitest-runner [OPTIONS]

     --bin-dir <PATH>       测试二进制目录（默认: bin）
     --timeout-sec <SEC>    单测超时秒数（默认: 60）
     --whitelist <PATH>     white list 路径（默认: whitelist.txt）
     --blocklist <PATH>     block list 路径（默认: blocklist.txt）
     --results-dir <PATH>   报告目录（默认: results）
     --list                 仅列出测例，不执行
     --verbose              详细输出
     --pattern <PATTERN>    名称子串过滤（可多次指定）

报告输出
========

执行后在 ``results/`` 下生成：

- ``test_report.txt``：文本报告
- ``summary.json``：JSON 汇总
- ``failed_cases.txt``：失败/超时列表
- ``<case>.log``：单测日志

终端汇总口径说明：

- ``总测试数/通过/失败/跳过`` 按 gtest 用例数统计
- 当某个程序没有产出 gtest 统计信息时，才按测试程序粒度回退统计

安装说明
========

在 ``user/apps/tests/dunitest/`` 目录下执行 ``make install`` 即可
