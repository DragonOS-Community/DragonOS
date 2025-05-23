.. note:: AI Translation Notice

   This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

   - Source document: kernel/syscall/syscall_table.rst

   - Translation time: 2025-05-19 01:41:32

   - Translation model: `Qwen/Qwen3-8B`


   Please report issues via `Community Channel <https://github.com/DragonOS-Community/DragonOS/issues>`_

System Call Table Implementation Plan
=====================================

.. note::
    Author: longjin <longjin@dragonos.org>

    Date: 2025/05/13

Overview
--------

.. mermaid::
   :align: center
   :caption: System Call Table Architecture

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

Compared to the original approach of dispatching system calls in a single large match statement, this approach uses a trait-based and system call table-based implementation. The main advantages include:

- Reduced stack memory usage: Avoids a single large function consuming excessive stack space
- Support for parameter printing: Through a unified parameter formatting interface
- Better extensibility: Adding new system calls does not require modifying the dispatch logic

Core Design
-----------

Syscall Trait
~~~~~~~~~~~~~

All system call handler functions must implement the `Syscall` trait:

.. code-block:: rust

    pub trait Syscall: Send + Sync + 'static {
        fn num_args(&self) -> usize;
        fn handle(&self, args: &[usize], from_user: bool) -> Result<usize, SystemError>;
        fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam>;
    }

- `num_args()`: Returns the number of arguments required by the system call
- `handle()`: Executes the actual system call handling
- `entry_format()`: Formats the parameters for debugging printing

SyscallHandle
~~~~~~~~~~~~~

The `SyscallHandle` struct associates a system call number with its handler:

.. code-block:: rust

    pub struct SyscallHandle {
        pub nr: usize,  // System call number
        pub inner_handle: &'static dyn Syscall,  // Handler function
        pub name: &'static str,
    }

SyscallTable
~~~~~~~~~~~~

The `SyscallTable` manages all system calls:

- Fixed size of 512 entries
- Initialized at compile time
- Allows quick lookup of the handler function by system call number

Usage
-----

Implementing a System Call
~~~~~~~~~~~~~~~~~~~~~~~~~~

1. Define the implementation of `Syscall` for the specific system call
2. Register the system call in the system call table
3. Load all registered system calls during system initialization
