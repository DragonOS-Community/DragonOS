# eBPF

> 作者: 陈林峰
> 
> Email: chenlinfeng25@outlook.com

## 概述

eBPF 是一项革命性的技术，起源于 Linux 内核，它可以在特权上下文中（如操作系统内核）运行沙盒程序。它用于安全有效地扩展内核的功能，而无需通过更改内核源代码或加载内核模块的方式来实现。

从历史上看，由于内核具有监督和控制整个系统的特权，操作系统一直是实现可观测性、安全性和网络功能的理想场所。同时，由于操作系统内核的核心地位和对稳定性和安全性的高要求，操作系统内核很难快速迭代发展。因此在传统意义上，与在操作系统本身之外实现的功能相比，操作系统级别的创新速度要慢一些。

eBPF 从根本上改变了这个方式。通过允许在操作系统中运行沙盒程序的方式，应用程序开发人员可以运行 eBPF 程序，以便在运行时向操作系统添加额外的功能。然后在 JIT 编译器和验证引擎的帮助下，操作系统确保它像本地编译的程序一样具备安全性和执行效率。这引发了一股基于 eBPF 的项目热潮，它们涵盖了广泛的用例，包括下一代网络实现、可观测性和安全功能等领域。

## eBPF In DragonOS

在一个新的OS上添加eBPF的支持需要了解eBPF的运行过程，通常，eBPF需要用户态工具和内核相关基础设施配合才能发挥其功能。而新的OS通常会兼容Linux上的应用程序，这可以进一步简化对用户态工具的移植工作，只要内核实现相关的系统调用和功能，就可以配合已有的工具完成eBPF的支持。

## eBPF的运行流程

![image-20240909165945192](/kernel/trace/ebpf_flow.png)

如图所示，eBPF程序的运行过程分为三个主要步骤：

1. 源代码->二进制
    1. 用户可以使用python/C/Rust编写eBPF程序，并使用相关的工具链编译源代码到二进制程序
    2. 这个步骤中，用户需要合理使用helper函数丰富eBPF程序功能
2. 加载eBPF程序
    1. 用户态的工具库会封装内核提供的系统调用接口，以简化用户的工作。用户态工具对eBPF程序经过预处理后发出系统调用，请求内核加载eBPF程序。
    1. 内核首先会对eBPF程序进行验证，检查程序的正确性和合法性，同时也会对程序做进一步的处理
    1. 内核会根据用户请求，将eBPF程序附加到内核的挂载点上(kprobe/uprobe/trace_point)
    1. 在内核运行期间，当这些挂载点被特定的事件触发， eBPF程序就会被执行
3. 数据交互
    1. eBPF程序可以收集内核的信息，用户工具可以选择性的获取这些信息
    2. eBPF程序可以直接将信息输出到文件中，用户工具通过读取和解析文件中的内容拿到信息
    3. eBPF程序通过Map在内核和用户态之间共享和交换数据



## 用户态支持

用户态的eBPF工具库有很多，比如C的libbpf，python的bcc, Rust的Aya，总体来说，这些工具的处理流程都大致相同。DragonOS当前支持[Aya](https://github.com/aya-rs/aya)框架编写的eBPF程序，以Aya为例，用户态的工具的处理过程如下:

1. 提供eBPF使用的helper函数和Map抽象，方便实现eBPF程序
2. 处理编译出来的eBPF程序，调用系统调用创建Map，获得对应的文件描述符
3. 根据需要，更新Map的值(.data)
4. 根据重定位信息，对eBPF程序的相关指令做修改
5. 根据内核版本，对eBPF程序中的bpf to bpf call进行处理
6. 加载eBPF程序到内核中
7. 对系统调用封装，提供大量的函数帮助访问eBPF的信息并与内核交互

DragonOS对Aya 库的支持并不完整。通过对Aya库的删减，我们实现了一个较小的[tiny-aya](https://github.com/DragonOS-Community/tiny-aya)。为了确保后期对Aya的兼容，tiny-aya只对Aya中的核心工具aya做了修改**，其中一些函数被禁用，因为这些函数的所需的系统调用或者文件在DragonOS中还未实现**。

### Tokio

Aya需要使用异步运行时，通过增加一些系统调用和修复一些错误DragonOS现在已经支持基本的tokio运行时。

### 使用Aya创建eBPF程序

与Aya官方提供的[文档](https://aya-rs.dev/book/start/development/)所述，只需要根据其流程安装对应的Rust工具链，就可以按照模板创建eBPF项目。以当前实现的`syscall_ebf`为例，这个程序的功能是统计系统调用的次数，并将其存储在一个HashMap中。

```
├── Cargo.toml
├── README.md
├── syscall_ebpf
├── syscall_ebpf-common
├── syscall_ebpf-ebpf
└── xtask
```

在user/app目录中，项目结构如上所示：

- `syscall_ebpf-ebpf`是 eBPF代码的实现目录，其会被编译到字节码
- `syscall_ebpf-common` 是公共库，方便内核和用户态进行信息交互
- `syscall_ebpf` 是用户态程序，其负责加载eBPF程序并获取eBPF程序产生的数据
- `xtask` 是一个命令行工具，方便用户编译和运行用户态程序

为了在DragonOS中运行用户态程序，暂时还不能直接使用模板创建的项目：

1. 这个项目不符合DragonOS对用户程序的项目结构要求，当然这可以通过稍加修改完成
2. 因为DragonOS对tokio运行时的支持还不是完整体，需要稍微修改一下使用方式

```
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
```

3. 因为对Aya支持不是完整体，因此项目依赖的aya和aya-log需要换成tiny-aya中的实现。

```
[dependencies]
aya = { git = "https://github.com/DragonOS-Community/tiny-aya.git" }
aya-log = { git = "https://github.com/DragonOS-Community/tiny-aya.git" }
```

只需要稍加修改，就可以利用Aya现有的工具完成eBPF程序的实现。

## 内核态支持

内核态支持主要为三个部分：

1. kprobe实现：位于目录`kernel/crates/kprobe`
2. rbpf运行时：位于目录`kernel/crates/rbpf`
3. 系统调用支持
4. helper函数支持

### rbpf

由于rbpf之前只是用于运行一些简单的eBPF程序，其需要通过一些修改才能运行更复杂的程序。

1. 增加bpf to bpf call 的支持：通过增加新的栈抽象和保存和恢复必要的寄存器数据
2. 关闭内部不必要的内存检查，这通常由内核的验证器完成
3. 增加带所有权的数据结构避免生命周期的限制



### 系统调用

eBPF相关的系统调用都集中在`bpf()` 上，通过参数cmd来进一步区分功能，目前对其支持如下:

```rust
pub fn bpf(cmd: bpf_cmd, attr: &bpf_attr) -> Result<usize> {
    let res = match cmd {
        // Map related commands
        bpf_cmd::BPF_MAP_CREATE => map::bpf_map_create(attr),
        bpf_cmd::BPF_MAP_UPDATE_ELEM => map::bpf_map_update_elem(attr),
        bpf_cmd::BPF_MAP_LOOKUP_ELEM => map::bpf_lookup_elem(attr),
        bpf_cmd::BPF_MAP_GET_NEXT_KEY => map::bpf_map_get_next_key(attr),
        bpf_cmd::BPF_MAP_DELETE_ELEM => map::bpf_map_delete_elem(attr),
        bpf_cmd::BPF_MAP_LOOKUP_AND_DELETE_ELEM => map::bpf_map_lookup_and_delete_elem(attr),
        bpf_cmd::BPF_MAP_LOOKUP_BATCH => map::bpf_map_lookup_batch(attr),
        bpf_cmd::BPF_MAP_FREEZE => map::bpf_map_freeze(attr),
        // Program related commands
        bpf_cmd::BPF_PROG_LOAD => prog::bpf_prog_load(attr),
        // Object creation commands
        bpf_cmd::BPF_BTF_LOAD => {
            error!("bpf cmd {:?} not implemented", cmd);
            return Err(SystemError::ENOSYS);
        }
        ty => {
            unimplemented!("bpf cmd {:?} not implemented", ty)
        }
    };
    res
}
```

其中对创建Map命令会再次细分，以确定具体的Map类型，目前我们对通用的Map基本添加了支持:

```rust
bpf_map_type::BPF_MAP_TYPE_ARRAY 
bpf_map_type::BPF_MAP_TYPE_PERCPU_ARRAY 
bpf_map_type::BPF_MAP_TYPE_PERF_EVENT_ARRAY
bpf_map_type::BPF_MAP_TYPE_HASH 
bpf_map_type::BPF_MAP_TYPE_PERCPU_HASH 
bpf_map_type::BPF_MAP_TYPE_QUEUE 
bpf_map_type::BPF_MAP_TYPE_STACK 
bpf_map_type::BPF_MAP_TYPE_LRU_HASH 
bpf_map_type::BPF_MAP_TYPE_LRU_PERCPU_HASH 

bpf_map_type::BPF_MAP_TYPE_CPUMAP
| bpf_map_type::BPF_MAP_TYPE_DEVMAP
| bpf_map_type::BPF_MAP_TYPE_DEVMAP_HASH => {
    error!("bpf map type {:?} not implemented", map_meta.map_type);
    Err(SystemError::EINVAL)?
}
```

所有的Map都会实现定义好的接口，这个接口参考Linux的实现定义:

```rust
pub trait BpfMapCommonOps: Send + Sync + Debug + CastFromSync {
    /// Lookup an element in the map.
    ///
    /// See https://ebpf-docs.dylanreimerink.nl/linux/helper-function/bpf_map_lookup_elem/
    fn lookup_elem(&mut self, _key: &[u8]) -> Result<Option<&[u8]>> {
        Err(SystemError::ENOSYS)
    }
    /// Update an element in the map.
    ///
    /// See https://ebpf-docs.dylanreimerink.nl/linux/helper-function/bpf_map_update_elem/
    fn update_elem(&mut self, _key: &[u8], _value: &[u8], _flags: u64) -> Result<()> {
        Err(SystemError::ENOSYS)
    }
    /// Delete an element from the map.
    ///
    /// See https://ebpf-docs.dylanreimerink.nl/linux/helper-function/bpf_map_delete_elem/
    fn delete_elem(&mut self, _key: &[u8]) -> Result<()> {
        Err(SystemError::ENOSYS)
    }
    /// For each element in map, call callback_fn function with map,
    /// callback_ctx and other map-specific parameters.
    ///
    /// See https://ebpf-docs.dylanreimerink.nl/linux/helper-function/bpf_for_each_map_elem/
    fn for_each_elem(&mut self, _cb: BpfCallBackFn, _ctx: *const u8, _flags: u64) -> Result<u32> {
        Err(SystemError::ENOSYS)
    }
    /// Look up an element with the given key in the map referred to by the file descriptor fd,
    /// and if found, delete the element.
    fn lookup_and_delete_elem(&mut self, _key: &[u8], _value: &mut [u8]) -> Result<()> {
        Err(SystemError::ENOSYS)
    }
    /// perform a lookup in percpu map for an entry associated to key on cpu.
    fn lookup_percpu_elem(&mut self, _key: &[u8], cpu: u32) -> Result<Option<&[u8]>> {
        Err(SystemError::ENOSYS)
    }
    /// Get the next key in the map. If key is None, get the first key.
    ///
    /// Called from syscall
    fn get_next_key(&self, _key: Option<&[u8]>, _next_key: &mut [u8]) -> Result<()> {
        Err(SystemError::ENOSYS)
    }
    /// Push an element value in map.
    fn push_elem(&mut self, _value: &[u8], _flags: u64) -> Result<()> {
        Err(SystemError::ENOSYS)
    }
    /// Pop an element value from map.
    fn pop_elem(&mut self, _value: &mut [u8]) -> Result<()> {
        Err(SystemError::ENOSYS)
    }
    /// Peek an element value from map.
    fn peek_elem(&self, _value: &mut [u8]) -> Result<()> {
        Err(SystemError::ENOSYS)
    }
    /// Freeze the map.
    ///
    /// It's useful for .rodata maps.
    fn freeze(&self) -> Result<()> {
        Err(SystemError::ENOSYS)
    }
    /// Get the first value pointer.
    fn first_value_ptr(&self) -> *const u8 {
        panic!("value_ptr not implemented")
    }
}
```

联通eBPF和kprobe的系统调用是[`perf_event_open`](https://man7.org/linux/man-pages/man2/perf_event_open.2.html)，这个系统调用在Linux中非常复杂，因此Dragon中并没有按照Linux进行实现，目前只支持其中两个功能:



```rust
match args.type_ {
    // Kprobe
    // See /sys/bus/event_source/devices/kprobe/type
    perf_type_id::PERF_TYPE_MAX => {
        let kprobe_event = kprobe::perf_event_open_kprobe(args);
        Box::new(kprobe_event)
    }
    perf_type_id::PERF_TYPE_SOFTWARE => {
        // For bpf prog output
        assert_eq!(args.config, perf_sw_ids::PERF_COUNT_SW_BPF_OUTPUT);
        assert_eq!(
            args.sample_type,
            Some(perf_event_sample_format::PERF_SAMPLE_RAW)
        );
        let bpf_event = bpf::perf_event_open_bpf(args);
        Box::new(bpf_event)
    }
}
```

- 其中一个`PERF_TYPE_SOFTWARE`是用来创建软件定义的事件，`PERF_COUNT_SW_BPF_OUTPUT` 确保这个事件用来采集bpf的输出。
- `PERF_TYPE_MAX` 通常指示创建kprobe/uprobe事件，也就是用户程序使用kprobe的途径之一，用户程序可以将eBPF程序绑定在这个事件上

同样的，perf不同的事件也实现定义的接口:

```rust
pub trait PerfEventOps: Send + Sync + Debug + CastFromSync + CastFrom {
    fn mmap(&self, _start: usize, _len: usize, _offset: usize) -> Result<()> {
        panic!("mmap not implemented for PerfEvent");
    }
    fn set_bpf_prog(&self, _bpf_prog: Arc<File>) -> Result<()> {
        panic!("set_bpf_prog not implemented for PerfEvent");
    }
    fn enable(&self) -> Result<()> {
        panic!("enable not implemented");
    }
    fn disable(&self) -> Result<()> {
        panic!("disable not implemented");
    }
    fn readable(&self) -> bool {
        panic!("readable not implemented");
    }
}
```

这个接口目前并不稳定。

### helper函数支持

用户态工具通过系统调用和内核进行通信，完成eBPF数据的设置、交换。在内核中，eBPF程序的运行也需要内核的帮助，单独的eBPF程序并没有什么太大的用处，因此其会调用内核提供的`helper` 函数完成对内核资源的访问。

目前已经支持的大多数`helper` 函数是与Map操作相关:

```rust
/// Initialize the helper functions.
pub fn init_helper_functions() {
    let mut map = BTreeMap::new();
    unsafe {
        // Map helpers::Generic map helpers
        map.insert(1, define_func!(raw_map_lookup_elem));
        map.insert(2, define_func!(raw_map_update_elem));
        map.insert(3, define_func!(raw_map_delete_elem));
        map.insert(164, define_func!(raw_map_for_each_elem));
        map.insert(195, define_func!(raw_map_lookup_percpu_elem));
        // map.insert(93,define_func!(raw_bpf_spin_lock);
        // map.insert(94,define_func!(raw_bpf_spin_unlock);
        // Map helpers::Perf event array helpers
        map.insert(25, define_func!(raw_perf_event_output));
        // Probe and trace helpers::Memory helpers
        map.insert(4, define_func!(raw_bpf_probe_read));
        // Print helpers
        map.insert(6, define_func!(trace_printf));

        // Map helpers::Queue and stack helpers
        map.insert(87, define_func!(raw_map_push_elem));
        map.insert(88, define_func!(raw_map_pop_elem));
        map.insert(89, define_func!(raw_map_peek_elem));
    }
    BPF_HELPER_FUN_SET.init(map);
}
```

