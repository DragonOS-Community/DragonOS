==============================
gVisor 系统调用测试
==============================

DragonOS 集成了 gVisor 系统调用测试套件，用于验证操作系统系统调用实现的兼容性和正确性。

概述
========

gVisor 是 Google 开发的容器运行时沙箱，包含了大量的系统调用兼容性测试。这些测试用于验证操作系统的系统调用实现是否符合 Linux 标准。

主要特性：

- **全面的测试覆盖**：包含数百个系统调用测试用例
- **白名单机制**：默认只运行已验证的测试，逐步完善支持
- **黑名单过滤**：可针对每个测试程序屏蔽特定的测试用例
- **自动化运行**：提供 Makefile 和脚本简化测试流程

自动测试
==========

执行`make test-syscall`命令。该命令将启动DragonOS并自动执行gvisor syscall测试套件，测试完成后会退出qemu。同时根据测试用例成功率选择是成功返回还是失败返回，成功率不等于100%则失败返回。该命令的执行流程如下：

对应的workflow配置文件为`test-x86.yml`

手动测试
==========

1. 进入测试目录：

   .. code-block:: bash

      cd user/apps/tests/syscall/gvisor

2. 在Linux运行白名单测试（自动下载测试套件）：

   .. code-block:: bash

      make test

3. 如果需要运行测试，可通过脚本快速修改配置：

   .. code-block:: bash

      # 启用 gVisor 测试（注释 blocklist 配置）
      bash user/apps/tests/syscall/gvisor/toggle_compile_gvisor.sh enable

      # 测试完成后恢复默认屏蔽
      bash user/apps/tests/syscall/gvisor/toggle_compile_gvisor.sh disable

4. 在 DragonOS 系统内运行测试：

   进入安装目录并运行测试程序：

   .. code-block:: bash

      cd /opt/tests/gvisor
      ./gvisor-test-runner --help

   使用 ``./gvisor-test-runner`` 可以运行具体的测试用例。

5. 查看详细文档：

   请参阅 `user/apps/tests/syscall/gvisor/README.md` 获取完整的使用说明。

测试机制
==========

白名单模式
-----------

测试框架默认启用白名单模式，只运行 ``whitelist.txt`` 中指定的测试程序。这允许逐步验证 DragonOS 的系统调用实现。

黑名单过滤
-----------

对于每个测试程序，可以通过 ``blocklists/`` 目录下的文件屏蔽特定的测试用例。这对于跳过暂不支持或不稳定的测试非常有用。

测例修复指引
============

本部分介绍如何修复gvisor测例，帮助开发者快速参与系统调用修复工作。

准备工作
--------

1. **首次安装测例**

   在开始修复工作前，需要先执行一次 `make test-syscall` 确保测例已安装到DragonOS系统中：

   .. code-block:: bash

      make test-syscall

   该命令会下载测试套件、编译DragonOS、写入镜像并自动运行测试。首次执行后，测例会安装到DragonOS的 ``/opt/tests/gvisor`` 目录。

2. **克隆测例源代码仓库**

   测例的源代码位于gvisor仓库中，需要克隆到本地以便查看和修复：

   .. code-block:: bash

      git clone https://cnb.cool/DragonOS-Community/gvisor.git
      cd gvisor
      git checkout dragonos/release-20250616.0

   测例代码位于 ``test/syscalls/linux`` 目录下，使用gtest框架编写，每个系统调用对应一个或多个 ``.cc`` 文件。

测试执行流程
------------

1. **进入DragonOS系统**

   使用以下命令以无图形模式启动DragonOS：

   .. code-block:: bash

      make run-nographic

2. **执行测试查看报错**

   在DragonOS系统内，切换到测例目录并执行具体的测试程序：

   .. code-block:: bash

      cd /opt/tests/gvisor
      ./<test_name>

   例如，要测试socket相关的系统调用：

   .. code-block:: bash

      ./socket_test

   执行测试后，会看到测试结果。失败的测试用例会显示详细的错误信息，包括：

   - 失败的测试用例名称
   - 错误原因（如系统调用返回错误码、行为不符合预期等）

   如果测例卡住（无响应或长时间不返回），需要查看堆栈跟踪信息来定位问题：

   .. code-block:: bash

      # 在DragonOS运行的情况下，新开一个终端窗口
      # 在DragonOS项目根目录执行
      make gdb

   在gdb中，首先需要选择对应的vcpu线程，然后查看堆栈跟踪：

   .. code-block:: gdb

      # 查看所有线程
      info threads
      
      # 选择对应的vcpu线程（例如thread 2）
      thread 2
      
      # 查看堆栈跟踪
      bt
      
      # 或者查看完整的堆栈跟踪（包含局部变量）
      bt full

   **注意**：DragonOS是多核系统，可能有多个vcpu线程。需要根据实际情况选择正确的线程来查看堆栈跟踪。

3. **记录失败的测试用例**

   将失败的测试用例名称、错误信息和堆栈跟踪信息记录下来，这些信息将用于后续的修复工作。

代码修复流程
------------

1. **复制测例源代码**

   从克隆的gvisor仓库中，找到对应的测例源代码文件（通常是 ``<test_name>.cc``），复制到DragonOS项目根目录：

   .. code-block:: bash

      cp /path/to/gvisor/test/syscalls/linux/<test_name>.cc /path/to/DragonOS/

   例如，修复socket_test：

   .. code-block:: bash

      cp gvisor/test/syscalls/linux/socket_test.cc /home/jinlong/code/DragonOS/

2. **使用AI助手进行修复**

   将复制的 ``.cc`` 文件提供给AI助手（如Cursor的AI功能），并说明：

   - 测试失败的具体错误信息
   - 要求AI遵循Linux语义来修复测例
   - DragonOS的系统调用实现特点（参考Linux 6.6语义）

   AI助手会分析代码，识别问题所在，并提供修复建议或直接修复代码。

   一些小建议：
   
   - 如果问题较为复杂，我们建议先使用Plan模式制定修复计划，然后再修复。
   - 如果AI助手难以定位问题，可以让他在内核的适当位置添加少量日志辅助定位
   - 如果终端日志被截断，可以到serial_opt.txt去查看完整日志，复制给AI助手
   - 如果AI助手难以定位问题，那么可以让他在`user/apps/c_unitest`下面写简单的测试程序来修复问题。这些测试程序会被编译安装到DragonOS的bin目录下。
   - 时刻注意提醒AI助手寻找是否有可以复用的代码、建立合理且适当的抽象。不然代码会很冗余！

3. **验证修复效果**

   修复完成后，需要重新编译DragonOS并测试：

   .. code-block:: bash

      # 在DragonOS项目根目录
      make run-nographic

   然后在DragonOS内再次执行测试，确认修复是否成功。

白名单管理
----------

修复完成的测例需要添加到白名单中，才能被测试框架执行。

1. **编辑白名单文件**

   打开 ``user/apps/tests/syscall/gvisor/whitelist.txt`` 文件，添加测试程序名称：

   .. code-block:: text

      # 文件系统相关测试
      open_test
      stat_test
      socket_test  # 新添加的测试

2. **白名单格式说明**

   - 每行一个测试程序名称（不带路径和扩展名）
   - 以 ``#`` 开头的行是注释，会被忽略
   - 空行会被忽略
   - 可以按功能分类组织，使用注释分组

3. **注意事项**

   - 测试程序名称必须与可执行文件名完全一致
   - 添加后需要重新`make test-syscall`才能生效
   - 建议在添加前先验证测试能够正常运行

黑名单管理
----------

如果某个测试程序中的部分测试用例暂时无法修复或不需要支持，可以创建blocklist文件来屏蔽这些测试用例。

1. **何时需要创建blocklist**

   - 测试用例依赖DragonOS暂不支持的功能（如IPv6、某些文件系统特性等）
   - 测试用例存在已知的不稳定性或竞争条件
   - 测试用例需要特殊权限或环境配置，当前环境无法满足

2. **创建blocklist文件**

   在 ``user/apps/tests/syscall/gvisor/blocklists/`` 目录下创建与测试程序同名的文件：

   .. code-block:: bash

      # 例如，为socket_test创建blocklist
      touch user/apps/tests/syscall/gvisor/blocklists/socket_test

3. **blocklist文件格式**

   在文件中添加要屏蔽的测试用例名称，每行一个：

   .. code-block:: text

      # 这是注释行，会被忽略
      
      # 屏蔽IPv6相关测试
      SocketTest.IPv6*
      SocketTest.IPv6Socket*
      
      # 屏蔽需要特殊权限的测试
      SocketTest.PrivilegedSocket
      
      # 屏蔽已知不稳定的测试
      SocketTest.UnimplementedFeature

4. **格式说明**

   - 支持通配符 ``*`` 匹配任意字符
   - 测试用例名称格式通常为 ``TestSuite.TestCase``
   - 以 ``#`` 开头的行是注释
   - 空行会被忽略
   - **重要**：必须在文件中注明屏蔽原因，方便后续维护

5. **示例**

   查看现有的blocklist文件作为参考：

   - ``user/apps/tests/syscall/gvisor/blocklists/socket_test`` - socket测试的黑名单
   - ``user/apps/tests/syscall/gvisor/blocklists/epoll_test`` - epoll测试的黑名单

完整工作流示例
--------------

以下是一个完整的修复流程示例，以修复 ``open_test`` 为例：

1. **准备工作**

   .. code-block:: bash

      # 首次安装测例（如果还没安装）
      make test-syscall
      
      # 克隆测例源代码
      git clone https://cnb.cool/DragonOS-Community/gvisor.git
      cd gvisor
      git checkout dragonos/release-20250616.0

2. **发现问题**

   .. code-block:: bash

      # 进入DragonOS
      make run-nographic
      
      # 在DragonOS内执行测试
      cd /opt/tests/gvisor
      ./open_test
      
      # 发现部分测试用例失败，记录错误信息

3. **修复代码**

   .. code-block:: bash

      # 复制源代码到DragonOS根目录
      cp gvisor/test/syscalls/linux/open_test.cc /home/jinlong/code/DragonOS/
      
      # 使用AI助手修复（在Cursor中打开文件并提供错误信息）
      # AI会分析代码并提供修复方案

4. **验证修复**

   .. code-block:: bash

      # 重新编译
      make kernel
      make run-nographic
      
      # 再次测试
      cd /opt/tests/gvisor
      ./open_test
      
      # 确认所有测试用例通过

5. **添加到白名单**

   .. code-block:: bash

      # 编辑白名单文件
      vim user/apps/tests/syscall/gvisor/whitelist.txt
      
      # 添加：open_test

6. **处理无法修复的测试用例**

   如果某些测试用例暂时无法修复（如依赖未实现的功能），创建blocklist：

   .. code-block:: bash

      # 创建blocklist文件
      vim user/apps/tests/syscall/gvisor/blocklists/open_test
      
      # 添加内容：
      # 屏蔽依赖O_TMPFILE的测试（DragonOS暂不支持）
      OpenTest.Tmpfile*
      # 原因：O_TMPFILE需要特定的文件系统支持，当前未实现

7. **最终验证**

   .. code-block:: bash

      # 重新编译并运行完整测试套件
      make test-syscall
      
      # 确认open_test能够正常运行，失败的用例已被blocklist屏蔽

注意事项
--------

- **符合Linux语义**：修复时要确保系统调用行为符合Linux 6.6的语义，不要使用workaround绕过问题
- **深入分析**：修复前要深入分析问题根源，结合测例代码、DragonOS实现和Linux行为进行对比
- **测试验证**：每次修复后都要重新测试，确保修复有效且没有引入新的问题
- **文档记录**：在blocklist中详细记录屏蔽原因，方便后续维护和重新启用
- **代码质量**：修复后的代码要符合DragonOS的代码规范，运行 ``make fmt`` 进行格式化

更多详细信息
==============

关于 gVisor 系统调用测试的详细使用方法、配置选项和开发指南，请查看测试目录下的 README.md 文档：

- 文档位置：`user/apps/tests/syscall/gvisor/README.md`

