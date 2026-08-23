//! perf_event_open 的 uprobe 分发实现（计划步骤 7 / batch4）。
//!
//! 照 [`crate::perf::kprobe`] 的 `KprobePerfEvent` 结构，为用户态断点探针提供
//! perf 接入：`perf_event_open` 按 event_source sysfs 公布的 uprobe PMU type
//! 分发到本模块；`config1`(name) 为路径，`config2`(offset) 为文件偏移。
//!
//! - BPF 程序经 `PERF_EVENT_IOC_SET_BPF` → [`UprobePerfEvent::do_set_bpf_prog`] JIT 后
//!   注入每个 per-mm 实例的 `event_callback`（评审 F10：复用 `BPF_PROG_TYPE_KPROBE`）。
//! - 命中时由 batch3 的 `#BP` handler 调 `call_event_callback`，本模块的
//!   [`UprobePerfCallBack`] 保证 BPF 入口 `pt_regs.rip = break_address()`（原探针址，
//!   评审 F5，绝不暴露 XOL slot 地址）。

use super::Result;
use crate::arch::interrupt::TrapFrame;
use crate::arch::kprobe::KProbeContext;
use crate::bpf::helper::BPF_HELPER_FUN_SET;
use crate::bpf::prog::BpfProg;
use crate::filesystem::page_cache::PageCache;
use crate::filesystem::vfs::file::File;
use crate::filesystem::vfs::{
    fcntl::AtFlags,
    utils::{user_resolved_path_at, ResolvedPath},
    FilePrivateData, FileSystem, IndexNode, VFS_MAX_FOLLOW_SYMLINK_TIMES,
};
use crate::include::bindings::linux_bpf::bpf_prog_type;
use crate::libs::casting::DowncastArc;
use crate::libs::mutex::MutexGuard;
use crate::mm::ucontext::{
    noop_handler, uprobe_new_consumer_id, uprobe_registry_add, uprobe_registry_attach_callback,
    uprobe_registry_remove_consumer, uprobe_registry_set_enabled, UprobeConsumerReg,
    UprobeConsumerScope, UprobeDefinition, UprobeTaskScope,
};
use crate::perf::util::{PerfProbeArgs, PerfProbeConfig};
use crate::perf::{BasicPerfEbpfCallBack, PerfEventOps};
use crate::process::{ProcessManager, RawPid};
use crate::smp::core::smp_get_processor_id;
use crate::smp::cpu::{smp_cpu_manager, ProcessorId};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::mem::size_of;
use rbpf::EbpfVmRaw;
use system_error::SystemError;
use uprobe::{CallBackFunc, ProbeArgs};

/// 一次 `perf_event_open(uprobe)` 对应一个持久 consumer。per-mm site 由
/// AddressSpace 命中表拥有，consumer 只保存弱索引用于 close 撤销。
pub struct UprobePerfEvent {
    _args: PerfProbeArgs,
    // The mount owner must be released from perf-fd process context, never
    // when an IRQ-side ActiveXol releases its last site reference.
    _resolved_path: ResolvedPath,
    /// 消费者 id（评审 R9）：全局注册表与迟到句柄的归属键。
    consumer_id: u64,
}

impl Drop for UprobePerfEvent {
    /// 消费者关闭（fd 释放）：从注册表移除（杜绝后续迟到安装）+ drop 迟到句柄
    /// （fork/mmap 路径安装的，逐 mm 注销）。直接安装的 `handles` 随本结构
    /// drop 自动注销（评审 R9）。
    fn drop(&mut self) {
        uprobe_registry_remove_consumer(self.consumer_id);
    }
}
impl core::fmt::Debug for UprobePerfEvent {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UprobePerfEvent").finish_non_exhaustive()
    }
}

impl UprobePerfEvent {
    /// JIT 编译 BPF 程序并注入到每个 per-mm 实例的 `event_callback`。
    ///
    /// 同一个 `Arc<UprobePerfCallBack>` 共享给所有句柄（多 mm / 多映射共用一份 JIT
    /// 产物），与 kprobe 的注入路径一致。
    pub fn do_set_bpf_prog(&self, prog_file: Arc<File>) -> Result<()> {
        let file = prog_file
            .inode()
            .downcast_arc::<BpfProg>()
            .ok_or(SystemError::EINVAL)?;
        if file.prog_type() != bpf_prog_type::BPF_PROG_TYPE_KPROBE {
            return Err(SystemError::EINVAL);
        }
        if file.is_sleepable() {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        let prog_slice = file.insns();
        let prog_slice =
            unsafe { core::slice::from_raw_parts(prog_slice.as_ptr(), prog_slice.len()) };
        let mut vm = EbpfVmRaw::new(Some(prog_slice)).map_err(|e| {
            log::error!("create ebpf vm failed: {:?}", e);
            SystemError::EINVAL
        })?;

        for (id, f) in BPF_HELPER_FUN_SET.get() {
            vm.register_helper(*id, *f)
                .map_err(|_| SystemError::EINVAL)?;
        }

        let callback: Arc<dyn CallBackFunc>;

        #[cfg(target_arch = "x86_64")]
        {
            use crate::perf::JITMem;

            log::info!("Using JIT compilation for BPF program on x86_64 architecture (uprobe)");
            let jit_mem = Box::new(JITMem::try_for_bpf_program(prog_slice.len())?);
            let jit_mem_addr = Box::into_raw(jit_mem) as usize;
            let jit_result = unsafe {
                vm.set_jit_exec_memory(&mut *(jit_mem_addr as *mut JITMem))
                    .and_then(|_| vm.jit_compile())
            };
            if let Err(err) = jit_result {
                log::error!("uprobe BPF JIT compilation failed: {:?}", err);
                unsafe { drop(Box::from_raw(jit_mem_addr as *mut JITMem)) };
                return Err(SystemError::EINVAL);
            }
            let basic_callback = BasicPerfEbpfCallBack::new(file, vm, jit_mem_addr);
            callback = Arc::new(UprobePerfCallBack {
                inner: basic_callback,
                cpu: self._args.cpu,
            });
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            vm.register_allowed_memory(0..u64::MAX);
            let basic_callback = BasicPerfEbpfCallBack::new(file, vm);
            callback = Arc::new(UprobePerfCallBack {
                inner: basic_callback,
                cpu: self._args.cpu,
            });
        }

        // callback 是 consumer 级单一事实源，现有与后续映射同时生效。
        uprobe_registry_attach_callback(self.consumer_id, callback)
    }
}

/// uprobe 的 eBPF 事件回调（镜像 kprobe 的 `KprobePerfCallBack`）。
///
/// **F5 不变量**：BPF 入口 `pt_regs.rip = break_address()`（原探针址）。即便
/// batch3 传入的 TrapFrame.rip 仍是 int3 故障点（probe_vaddr+1），此处也强制把
/// 暴露给 BPF 的 rip 改为原探针址，XOL slot 地址绝不外泄。
pub struct UprobePerfCallBack {
    inner: BasicPerfEbpfCallBack,
    cpu: i32,
}

impl CallBackFunc for UprobePerfCallBack {
    fn call(&self, trap_frame: &dyn ProbeArgs) {
        if self.cpu >= 0 && smp_get_processor_id().data() != self.cpu as u32 {
            return;
        }
        // F5：BPF 看到的 rip 是原探针址（break_address），不是 XOL slot、也不是 rip+1。
        let probe_addr = trap_frame.break_address();
        let trap_frame = match trap_frame.as_any().downcast_ref::<TrapFrame>() {
            Some(tf) => tf,
            None => return,
        };
        let mut pt_regs = KProbeContext::from(trap_frame);
        pt_regs.rip = probe_addr as u64;
        let probe_context = unsafe {
            core::slice::from_raw_parts_mut(
                &mut pt_regs as *mut KProbeContext as *mut u8,
                size_of::<KProbeContext>(),
            )
        };
        self.inner.call(probe_context);
    }
}

impl IndexNode for UprobePerfEvent {
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize> {
        Err(SystemError::EINVAL)
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        panic!("fs not implemented for PerfEvent");
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn list(&self) -> Result<Vec<String>> {
        Err(SystemError::ENOSYS)
    }

    fn page_cache(&self) -> Option<Arc<PageCache>> {
        None
    }

    fn absolute_path(&self) -> core::result::Result<String, SystemError> {
        Ok(String::from("uprobe_perf_event"))
    }
}

impl PerfEventOps for UprobePerfEvent {
    fn set_bpf_prog(&self, bpf_prog: Arc<File>) -> Result<()> {
        self.do_set_bpf_prog(bpf_prog)
    }
    fn enable(&self) -> Result<()> {
        uprobe_registry_set_enabled(self.consumer_id, true)
    }
    fn disable(&self) -> Result<()> {
        uprobe_registry_set_enabled(self.consumer_id, false)
    }

    fn readable(&self) -> bool {
        false
    }
}

/// 创建 uprobe perf event（照 `perf_event_open_kprobe`）。
///
/// - `config1`(name) = 二进制路径；`config2`(offset) = 文件偏移。
/// - `pid >= 0`：仅目标进程的 mm（`pid == 0` = 当前进程）；`pid == -1`：经 inode rmap
///   遍历所有映射该文件的 mm（评审 B8）。
pub fn perf_event_open_uprobe(args: PerfProbeArgs) -> Result<UprobePerfEvent> {
    // Linux perf accepts task events with cpu=-1 or a concrete CPU, and CPU
    // events only with a concrete CPU. Values below -1 are never meaningful.
    if args.pid < -1
        || args.cpu < -1
        || (args.pid == -1 && args.cpu == -1)
        || (args.cpu >= 0
            && smp_cpu_manager()
                .possible_cpus()
                .get(ProcessorId::new(args.cpu as u32))
                != Some(true))
    {
        return Err(SystemError::EINVAL);
    }
    // Linux 6.6 perf_uprobe_event_init() applies this PMU-wide gate before
    // parsing or installing the probe.
    if !crate::process::cred::capable(crate::process::cred::CAPFlags::CAP_SYS_ADMIN) {
        return Err(SystemError::EACCES);
    }
    if args.inherit || args.enable_on_exec || args.remove_on_exec {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    if args.config != PerfProbeConfig::Raw(0) {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    // Linux uprobe PMU ABI uses config1 exclusively for the pathname and
    // config2 exclusively for the file offset.  Do not reinterpret ':' in a
    // valid filename as an out-of-band offset encoding.
    let path = args.name.clone();
    let offset = usize::try_from(args.offset).map_err(|_| SystemError::EINVAL)?;
    log::info!(
        "create uprobe for path: {path}, offset: {:#x}, pid: {}",
        offset,
        args.pid
    );

    // path → inode → page_cache（inode rmap 入口）
    let caller = ProcessManager::current_pcb();
    let (start, remaining) = user_resolved_path_at(&caller, AtFlags::AT_FDCWD.bits(), &path)?;
    let resolved = start
        .inode()
        .lookup_follow_symlink_owned(&start, &remaining, VFS_MAX_FOLLOW_SYMLINK_TIMES, true)
        .map_err(|e| {
            log::warn!("uprobe: failed to look up path {path}: {:?}", e);
            e
        })?;
    let inode = resolved.inode();
    inode.page_cache().ok_or_else(|| {
        log::warn!("uprobe: target {path} has no page cache (not a regular mapped file)");
        SystemError::EINVAL
    })?;
    let definition = UprobeDefinition::new(inode.clone(), offset)?;

    // pid 语义（评审 R1）：>=0 单 mm（需 ptrace 访问检查）；==-1 全量（需特权）；
    // 其他负值非法（EINVAL）。
    let scope = if args.pid >= 0 {
        let pcb = if args.pid == 0 {
            // Do not round-trip through a raw pid: pid 0 denotes current even
            // when the caller is nested in a PID namespace.
            ProcessManager::current_pcb()
        } else {
            ProcessManager::find_task_by_vpid(RawPid::from(args.pid as usize))
                .ok_or(SystemError::ESRCH)?
        };
        // The PMU-wide CAP_SYS_ADMIN gate mirrors Linux's privileged uprobe
        // event_init path; do not layer the unrelated process_vm permission
        // helper on top of it.
        pcb.basic().user_vm().ok_or(SystemError::ESRCH)?;
        UprobeConsumerScope::Task(UprobeTaskScope::new(&pcb))
    } else if args.pid == -1 {
        // 系统级模式：向**所有**映射该文件的进程（含其他用户的）安装断点，
        // PMU-wide CAP_SYS_ADMIN check above also covers system-wide events.
        UprobeConsumerScope::SystemWideAuthorized
    } else {
        // pid < -1：Linux perf 语义不存在（-1 之外无系统级变体），EINVAL。
        return Err(SystemError::EINVAL);
    };

    // 消费者身份 + 注册表登记（评审 R9：fork/后续 mmap 迟到安装的依据）。
    let consumer_id = uprobe_new_consumer_id();
    let inode_id = definition.inode_id();
    uprobe_registry_add(
        inode_id,
        offset,
        consumer_id,
        Arc::new(UprobeConsumerReg {
            definition,
            scope,
            pre_handler: noop_handler,
            post_handler: noop_handler,
            event_callback: None,
            // Initial activation uses the same scope-aware path as ioctl
            // ENABLE, so task events never need a global file-rmap scan.
            enabled: false,
        }),
    );

    // Activate through the common lifecycle path. Absence of a matching VMA
    // is valid; future mmap/dlopen/exec hooks will install the persistent
    // consumer. A real initial installation failure rolls registration back.
    if !args.disabled {
        if let Err(e) = uprobe_registry_set_enabled(consumer_id, true) {
            uprobe_registry_remove_consumer(consumer_id);
            return Err(e);
        }
    }

    Ok(UprobePerfEvent {
        _args: args,
        _resolved_path: resolved,
        consumer_id,
    })
}
