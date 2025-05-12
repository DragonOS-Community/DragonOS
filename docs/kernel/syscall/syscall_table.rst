系统调用表实现方案
====================

.. note::
    Author: longjin <longjin@dragonos.org>

    Date: 2025/05/13

概述
----

.. mermaid::
   :align: center
   :caption: 系统调用表架构

   classDiagram
      class Syscall {
         <<trait>>
         +num_args() usize
         +handle(args, from_user) Result<usize, SystemError>
         +entry_format(args) Vec<FormattedSyscallParam>
      }

      class SyscallHandle {
         +nr: usize
         +inner_handle: &dyn Syscall
      }

      class SyscallTable {
         -entries: [Option<&SyscallHandle>; 512]
         +get(nr) Option<&dyn Syscall>
      }

      Syscall <|.. SysXXXXXXHandle
      SyscallHandle "1" *-- "1" Syscall
      SyscallTable "1" *-- "512" SyscallHandle

相比于将原本集中在一个大match中的系统调用分发，本方案采用基于trait和系统调用表的实现。主要优势包括：

- 降低栈内存使用：避免单个大函数占用过多栈空间
- 支持参数打印：通过统一的参数格式化接口
- 更好的扩展性：新增系统调用无需修改分发逻辑

核心设计
--------

Syscall Trait
~~~~~~~~~~~~~

所有系统调用处理函数都需要实现 `Syscall` trait：

.. code-block:: rust

    pub trait Syscall: Send + Sync + 'static {
        fn num_args(&self) -> usize;
        fn handle(&self, args: &[usize], from_user: bool) -> Result<usize, SystemError>;
        fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam>;
    }

- `num_args()`: 返回该系统调用需要的参数数量
- `handle()`: 实际执行系统调用处理
- `entry_format()`: 格式化参数用于调试打印

SyscallHandle
~~~~~~~~~~~~~

`SyscallHandle` 结构体将系统调用号与处理函数关联：

.. code-block:: rust

    pub struct SyscallHandle {
        pub nr: usize,  // 系统调用号
        pub inner_handle: &'static dyn Syscall,  // 处理函数
        pub name: &'static str,
    }

SyscallTable
~~~~~~~~~~~~

`SyscallTable` 管理所有系统调用：

- 固定大小512项
- 编译时初始化
- 通过系统调用号快速查找处理函数

使用方式
--------

实现系统调用
~~~~~~~~~~~~

1. 定义实现``Syscall`` trait的结构体
2. 实现``handle()``和``entry_format()``方法
3. 使用``declare_syscall!``宏注册

参考实现：`sys_write.rs <sys_write_>`_

.. _sys_write:
   https://github.com/DragonOS-Community/DragonOS/blob/master/kernel/src/filesystem/vfs/syscall/sys_write.rs

注册系统调用
~~~~~~~~~~~~

使用``declare_syscall!``宏注册系统调用：

.. code-block:: rust

    syscall_table_macros::declare_syscall!(SYS_WRITE, SysWriteHandle);

参数说明：

1. 系统调用名称（用于生成符号）
2. 实现``Syscall`` trait的结构体

初始化流程
----------

1. 内核启动时调用``syscall_table_init()``
2. 从链接器符号``_syscall_table``加载所有注册的系统调用
3. 填充系统调用表
