export const zhSidebar = [
    {
      text: '入门',
      collapsed: false,
      items: [
      { text: 'DragonOS简介', link: '/zh/introduction/' },
      { text: '构建DragonOS', link: '/zh/introduction/build_system' },
      { text: '使用 nix 开发 DragonOS', link: '/zh/introduction/develop_nix' },
      { text: '使用 devcontainer 开发 DragonOS', link: '/zh/introduction/devcontainer' },
      { text: 'DragonOS镜像站', link: '/zh/introduction/mirrors' }
      ]
    },
    {
      text: '内核层',
      collapsed: false,
      items: [
      {
        text: '内核编译配置',
        collapsed: true,
        link: '/zh/kernel/configuration/',
        items: [
        { text: '内核编译配置说明', link: '/zh/kernel/configuration/config' },
        { text: '目标架构配置', link: '/zh/kernel/configuration/arch' }
        ]
      },
      {
        text: '引导加载',
        collapsed: true,
        link: '/zh/kernel/boot/',
        items: [
        { text: '引导加载程序', link: '/zh/kernel/boot/bootloader' },
        { text: '内核启动命令行参数', link: '/zh/kernel/boot/cmdline' }
        ]
      },
      {
        text: '核心API文档',
        collapsed: true,
        link: '/zh/kernel/core_api/',
        items: [
        { text: 'DragonOS内核核心API', link: '/zh/kernel/core_api/kernel_api' },
        { text: '原子变量', link: '/zh/kernel/core_api/atomic' },
        { text: '类型转换库API', link: '/zh/kernel/core_api/casting' },
        { text: 'Notifier Chain 通知链', link: '/zh/kernel/core_api/notifier_chain' },
        { text: '软中断', link: '/zh/kernel/core_api/softirq' }
        ]
      },
      {
        text: '中断与下半部',
        collapsed: true,
        link: '/zh/kernel/interrupt/',
        items: [
        { text: 'tasklet', link: '/zh/kernel/interrupt/tasklet' },
        { text: 'DragonOS Workqueue 机制设计文档', link: '/zh/kernel/interrupt/workqueue' }
        ]
      },
      {
        text: '锁',
        collapsed: true,
        link: '/zh/kernel/locking/',
        items: [
        { text: '锁的类型及其规则', link: '/zh/kernel/locking/locks' },
        { text: '自旋锁', link: '/zh/kernel/locking/spinlock' },
        { text: 'RwLock读写锁', link: '/zh/kernel/locking/rwlock' },
        { text: 'mutex互斥量', link: '/zh/kernel/locking/mutex' },
        { text: 'RwSem 读写信号量', link: '/zh/kernel/locking/rwsem' }
        ]
      },
      {
        text: 'RCU',
        collapsed: true,
        link: '/zh/kernel/rcu/',
        items: [
        { text: 'DragonOS RCU 架构', link: '/zh/kernel/rcu/architecture' },
        { text: 'RCU 分段回调队列', link: '/zh/kernel/rcu/segmented-callback-queues' },
        { text: 'DragonOS SRCU 设计原理', link: '/zh/kernel/rcu/srcu-design' }
        ]
      },
      {
        text: '进程管理模块',
        collapsed: true,
        link: '/zh/kernel/process_management/',
        items: [
        { text: 'kthread 内核线程', link: '/zh/kernel/process_management/kthread' },
        { text: '加载程序', link: '/zh/kernel/process_management/load_binary' },
        { text: 'DragonOS 多线程 Exec (De-thread) 机制原理', link: '/zh/kernel/process_management/de_thread' }
        ]
      },
      {
        text: 'DragonOS调度',
        collapsed: true,
        link: '/zh/kernel/sched/',
        items: [
        { text: '进程调度器相关的api', link: '/zh/kernel/sched/core' },
        { text: '完全公平调度器相关的api', link: '/zh/kernel/sched/cfs' },
        { text: 'FIFO调度器', link: '/zh/kernel/sched/fifo' },
        { text: '实时进程调度器相关的api', link: '/zh/kernel/sched/rt' },
        { text: '内核定时器', link: '/zh/kernel/sched/kernel_timer' },
        { text: 'DragonOS 等待队列机制', link: '/zh/kernel/sched/wait_queue' }
        ]
      },
      {
        text: '进程间通信',
        collapsed: true,
        link: '/zh/kernel/ipc/',
        items: [
        { text: 'Signal信号', link: '/zh/kernel/ipc/signal' },
        { text: 'IPC Namespace', link: '/zh/kernel/ipc/ipc_namespace' },
        { text: 'Restartable Sequences (rseq) 机制', link: '/zh/kernel/ipc/rseq' }
        ]
      },
      {
        text: '内存管理',
        collapsed: true,
        link: '/zh/kernel/memory_management/',
        items: [
        { text: '内存管理模块简介', link: '/zh/kernel/memory_management/intro' },
        { text: '内存分配指南', link: '/zh/kernel/memory_management/allocate-memory' },
        { text: 'MMIO', link: '/zh/kernel/memory_management/mmio' },
        { text: '异常表安全内存拷贝方案设计', link: '/zh/kernel/memory_management/extable_safe_copy_design' },
        { text: 'OOM Killer 设计说明', link: '/zh/kernel/memory_management/oom_killer' }
        ]
      },
      {
        text: '文件系统',
        collapsed: true,
        link: '/zh/kernel/filesystem/',
        items: [
        { text: '概述', link: '/zh/kernel/filesystem/overview' },
        { text: 'DragonOS inotify：用户语义与架构设计', link: '/zh/kernel/filesystem/inotify' },
        { text: 'DragonOS FUSE 架构设计', link: '/zh/kernel/filesystem/fuse' },
        { text: 'Virtiofs 基准测试运行手册', link: '/zh/kernel/filesystem/virtiofs_benchmark_runbook' },
        {
          text: 'VFS虚拟文件系统',
          collapsed: true,
          link: '/zh/kernel/filesystem/vfs/',
          items: [
          { text: '设计', link: '/zh/kernel/filesystem/vfs/design' },
          { text: '挂载传播性机制', link: '/zh/kernel/filesystem/vfs/mount_propagation' },
          { text: 'VFS API文档', link: '/zh/kernel/filesystem/vfs/api' },
          { text: '可挂载文件系统', link: '/zh/kernel/filesystem/vfs/mountable_fs' }
          ]
        },
        {
          text: 'ProcFS',
          collapsed: true,
          link: '/zh/kernel/filesystem/proc/',
          items: [
          { text: 'Proc 挂载导出接口', link: '/zh/kernel/filesystem/proc/mounts' }
          ]
        },
        { text: 'SysFS', link: '/zh/kernel/filesystem/sysfs' },
        { text: 'KernFS', link: '/zh/kernel/filesystem/kernfs' },
        {
          text: '联合文件系统',
          collapsed: true,
          link: '/zh/kernel/filesystem/unionfs/',
          items: [
          { text: 'overlayfs', link: '/zh/kernel/filesystem/unionfs/overlayfs' }
          ]
        }
        ]
      },
      {
        text: '内核调试模块',
        collapsed: true,
        link: '/zh/kernel/debug/',
        items: [
        { text: '内核栈traceback', link: '/zh/kernel/debug/traceback' },
        { text: '如何使用GDB调试内核', link: '/zh/kernel/debug/debug-kernel-with-gdb' },
        { text: '使用DADK对内核进行性能分析', link: '/zh/kernel/debug/profiling-kernel-with-dadk' }
        ]
      },
      {
        text: '内核测试',
        collapsed: true,
        link: '/zh/kernel/ktest/',
        items: [
        { text: 'dunitest 用户态测试框架', link: '/zh/kernel/ktest/dunitest' },
        { text: 'gVisor 系统调用测试', link: '/zh/kernel/ktest/gvisor_syscall_test' }
        ]
      },
      {
        text: '处理器架构',
        collapsed: true,
        link: '/zh/kernel/cpu_arch/',
        items: [
        {
          text: 'x86-64相关文档',
          collapsed: true,
          link: '/zh/kernel/cpu_arch/x86_64/',
          items: [
          { text: 'USB Legacy支持', link: '/zh/kernel/cpu_arch/x86_64/usb_legacy_support' }
          ]
        }
        ]
      },
      {
        text: '容器化',
        collapsed: true,
        link: '/zh/kernel/container/',
        items: [
        { text: '联合文件系统', link: '/zh/kernel/filesystem/unionfs/' }
        ]
      },
      {
        text: '其他内核库',
        collapsed: true,
        link: '/zh/kernel/libs/',
        items: [
        { text: '屏幕管理器（SCM）', link: '/zh/kernel/libs/lib_ui/scm' },
        { text: '文本显示框架（textui）', link: '/zh/kernel/libs/lib_ui/textui' },
        { text: 'unified-init 统一初始化库', link: '/zh/kernel/libs/unified-init' },
        { text: 'ID分配', link: '/zh/kernel/libs/id-allocation' }
        ]
      },
      {
        text: '网络子系统',
        collapsed: true,
        link: '/zh/kernel/net/',
        items: [
        { text: 'Internet Protocol Socket', link: '/zh/kernel/net/inet' },
        { text: 'UNIX', link: '/zh/kernel/net/unix' },
        { text: 'ssh支持', link: '/zh/kernel/net/ssh' },
        { text: 'DragonOS NAPI 与 NetNamespace Poll 机制设计说明', link: '/zh/kernel/net/napi_and_netns_poll' }
        ]
      },
      {
        text: '内核跟踪机制',
        collapsed: true,
        link: '/zh/kernel/trace/',
        items: [
        { text: 'Tracepoints', link: '/zh/kernel/trace/tracepoint' },
        { text: 'DragonOS 运行时文本修补：原理、使用与安全边界', link: '/zh/kernel/trace/text_patching' },
        { text: 'kprobe', link: '/zh/kernel/trace/kprobe' },
        { text: 'Uprobe：用户态动态探针', link: '/zh/kernel/trace/uprobe' },
        { text: 'eBPF', link: '/zh/kernel/trace/eBPF' }
        ]
      },
      {
        text: '系统调用',
        collapsed: true,
        link: '/zh/kernel/syscall/',
        items: [
        { text: '系统调用表实现方案', link: '/zh/kernel/syscall/syscall_table' },
        { text: 'sys_capget / sys_capset 设计说明', link: '/zh/kernel/syscall/sys_capget_capset' }
        ]
      },
      {
        text: '设备',
        collapsed: true,
        link: '/zh/kernel/device/',
        items: [
        { text: 'Linux tty设备', link: '/zh/kernel/device/tty' },
        { text: 'Loop Device 架构设计', link: '/zh/kernel/device/loop_device' }
        ]
      }
      ]
    },
    {
      text: '应用层',
      collapsed: false,
      items: [
      {
        text: '用户态构建文档（DADK）',
        collapsed: true,
        link: '/zh/userland/appdev/',
        items: [
        { text: 'Rust应用开发快速入门', link: '/zh/userland/appdev/rust-quick-start' },
        { text: '为DragonOS开发C/C++应用', link: '/zh/userland/appdev/c-cpp-quick-start' },
        { text: '快速使用DADK打包一个应用到DragonOS', link: 'https://docs.dragonos.org.cn/p/dadk/user-manual/quickstart.html' },
        { text: 'DADK完整文档', link: 'https://docs.dragonos.org.cn/p/dadk/' }
        ]
      },
      {
        text: '用户态构建文档（Nix）',
        collapsed: true,
        link: '/zh/userland/rootfs/',
        items: [
        { text: 'DADK RootFS Manifest 配置（非 Nix）', link: '/zh/userland/rootfs/dadk-rootfs-manifest' },
        { text: '从软件包到RootFS镜像', link: '/zh/userland/rootfs/diskgen' },
        { text: '添加程序/添加自定义程序！', link: '/zh/userland/rootfs/add-your-own-app' }
        ]
      }
      ]
    },
    {
      text: '开发指南',
      collapsed: false,
      items: [
      { text: 'Nix 技巧', link: '/zh/dev-skills/nix-skills' },
      { text: '贡献文档', link: '/zh/dev-skills/write-docs' }
      ]
    },
    {
      text: 'Q&A',
      collapsed: false,
      items: [
      {
        text: '常见问题解答',
        collapsed: true,
        link: '/zh/questions/',
        items: [
        { text: '构建错误常见问题解答', link: '/zh/questions/build_errors' }
        ]
      }
      ]
    },
    {
      text: 'DragonOS社区',
      collapsed: false,
      items: [
      {
        text: '参与开发',
        collapsed: true,
        link: '/zh/community/code_contribution/',
        items: [
        { text: '如何参与贡献', link: 'https://community.dragonos.org/contributors/' },
        { text: 'C语言代码风格', link: '/zh/community/code_contribution/c-coding-style' },
        { text: 'Rust语言代码风格', link: '/zh/community/code_contribution/rust-coding-style' },
        { text: '代码提交规范', link: '/zh/community/code_contribution/conventional-commit' }
        ]
      },
      { text: '与社区建立联系', link: '/zh/community/contact/' },
      {
        text: '发行日志',
        collapsed: true,
        link: '/zh/community/ChangeLog/',
        items: [
        { text: 'V0.4.0', link: '/zh/community/ChangeLog/V0.4.x/V0.4.0' },
        { text: 'V0.3.0', link: '/zh/community/ChangeLog/V0.3.x/V0.3.0' },
        { text: 'V0.2.0', link: '/zh/community/ChangeLog/V0.2.x/V0.2.0' },
        { text: 'V0.1.10', link: '/zh/community/ChangeLog/V0.1.x/V0.1.10' },
        { text: 'V0.1.9', link: '/zh/community/ChangeLog/V0.1.x/V0.1.9' },
        { text: 'V0.1.8', link: '/zh/community/ChangeLog/V0.1.x/V0.1.8' },
        { text: 'V0.1.7', link: '/zh/community/ChangeLog/V0.1.x/V0.1.7' },
        { text: 'V0.1.6', link: '/zh/community/ChangeLog/V0.1.x/V0.1.6' },
        { text: 'V0.1.5', link: '/zh/community/ChangeLog/V0.1.x/V0.1.5' },
        { text: 'V0.1.4', link: '/zh/community/ChangeLog/V0.1.x/V0.1.4' },
        { text: 'V0.1.3', link: '/zh/community/ChangeLog/V0.1.x/V0.1.3' },
        { text: 'V0.1.2', link: '/zh/community/ChangeLog/V0.1.x/V0.1.2' },
        { text: 'V0.1.1', link: '/zh/community/ChangeLog/V0.1.x/V0.1.1' },
        { text: 'V0.1.0', link: '/zh/community/ChangeLog/V0.1.x/V0.1.0' }
        ]
      }
      ]
    }
]
