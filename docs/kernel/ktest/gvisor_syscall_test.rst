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

执行`make test-syscall`命令。该命令将启动DragonOS并自动执行gvisor syscall测试套，测试完成后会退出qemu。同时根据测试用例成功率选择是成功返回还是失败返回，成功率不等于100%则失败返回。该命令的执行流程如下：

1. 执行`enable_compile_gvisor.sh`注释`app-blocklist.toml`中有关于屏蔽gvisor测试套的配置
2. 编译DragonOS
3. 写入镜像
4. 后台qemu无图形模式启动DragonOS，同时设置环境变量`AUTO_TEST`（自动测试选项，目前仅支持syscall测试）和`SYSCALL_TEST_DIR`（测试套所在目录），这两个环境变量会通过命令行参数传递到DragonOS。然后当busybox init进程执行rcS脚本时，该脚本会通过`AUTO_TEST`选项执行对应的测试
5. 执行`monitor_test_results.sh`定时查看qemu串口输出内容，并根据测试结果选择成功返回还是失败返回
6. 执行`disable_compile_gvisor.sh`取消`app-blocklist.toml`中有关于屏蔽gvisor测试套的配置注释

对应的workflow配置文件为`test-x86.yml`

手动测试
==========

1. 进入测试目录：

   .. code-block:: bash

      cd user/apps/tests/syscall/gvisor

2. 在Linux运行白名单测试（自动下载测试套件）：

   .. code-block:: bash

      make test

3. 如果需要运行测试，请先修改配置文件：

   编辑 `config/app-blocklist.toml`，注释掉以下内容：

   .. code-block:: toml

      # 屏蔽gvisor系统调用测试
      # [[blocked_apps]]
      # name = "gvisor syscall tests"
      # reason = "由于文件较大，因此屏蔽。如果要允许系统调用测试，则把这几行取消注释即可"

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

更多详细信息
==============

关于 gVisor 系统调用测试的详细使用方法、配置选项和开发指南，请查看测试目录下的 README.md 文档：

- 文档位置：`user/apps/tests/syscall/gvisor/README.md`

