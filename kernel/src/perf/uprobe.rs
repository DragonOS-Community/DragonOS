//! perf_event_open 的 uprobe 分发实现（计划步骤 7 / batch4）。
//!
//! 照 [`crate::perf::kprobe`] 的 `KprobePerfEvent` 结构，为用户态断点探针提供
//! perf 接入：`perf_event_open` 收到 `PERF_TYPE_MAX` 且 `config1`(name) 含 `/`（路径）
//! 时走本模块（评审 F9）；`config2`(offset) 为文件偏移；`pid` 消费见 [`perf_event_open_uprobe`]。
//!
//! - BPF 程序经 `PERF_EVENT_IOC_SET_BPF` → [`UprobePerfEvent::do_set_bpf_prog`] JIT 后
//!   注入每个 per-mm 实例的 `event_callback`（评审 F10：复用 `BPF_PROG_TYPE_KPROBE`）。
//! - 命中时由 batch3 的 `#BP` handler 调 `call_event_callback`，本模块的
//!   [`UprobePerfCallBack`] 保证 BPF 入口 `pt_regs.rip = break_address()`（原探针址，
//!   评审 F5，绝不暴露 XOL slot 地址）。

use super::Result;
use crate::arch::interrupt::TrapFrame;
use crate::arch::kprobe::KProbeContext;
use crate::arch::MMArch;
use crate::bpf::helper::BPF_HELPER_FUN_SET;
use crate::bpf::prog::BpfProg;
use crate::filesystem::page_cache::PageCache;
use crate::filesystem::vfs::file::File;
use crate::filesystem::vfs::{FilePrivateData, FileSystem, IndexNode};
use crate::libs::casting::DowncastArc;
use crate::libs::mutex::MutexGuard;
use crate::mm::ucontext::{noop_handler, uprobe_register, AddressSpace, LockedVMA, UprobeHandle};
use crate::mm::MemoryManagementArch;
use crate::perf::util::PerfProbeArgs;
use crate::perf::{BasicPerfEbpfCallBack, PerfEventOps};
use crate::process::{ProcessManager, RawPid};
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::mem::size_of;
use rbpf::EbpfVmRaw;
use system_error::SystemError;
use uprobe::{CallBackFunc, ProbeArgs};

/// 由一次 `perf_event_open(uprobe)` 注册的全部 per-mm 探针句柄。
///
/// `pid >= 0` 时仅含目标 mm 的句柄；`pid == -1` 时含所有映射该文件的 mm 的句柄
/// （同一 mm 若把同一文件映射多次，则有多条）。`Drop` 时逐个释放 [`UprobeHandle`]，
/// 其 `Drop` 自动注销（恢复原页 + 移除表项 + 回收 XOL slot），无需手动 unregister。
pub struct UprobePerfEvent {
    _args: PerfProbeArgs,
    handles: Vec<UprobeHandle>,
}

impl core::fmt::Debug for UprobePerfEvent {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UprobePerfEvent")
            .field("handle_count", &self.handles.len())
            .finish_non_exhaustive()
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
            let jit_mem = Box::new(JITMem::new());
            let jit_mem = Box::leak(jit_mem);
            let jit_mem_addr = core::ptr::from_ref::<JITMem>(jit_mem) as usize;
            vm.set_jit_exec_memory(jit_mem).unwrap();
            vm.jit_compile().unwrap();
            let basic_callback = BasicPerfEbpfCallBack::new(file, vm, jit_mem_addr);
            callback = Arc::new(UprobePerfCallBack(basic_callback));
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            vm.register_allowed_memory(0..u64::MAX);
            let basic_callback = BasicPerfEbpfCallBack::new(file, vm);
            callback = Arc::new(UprobePerfCallBack(basic_callback));
        }

        // 注入到每一个 per-mm 实例（多 handle 共享同一回调）。
        for handle in &self.handles {
            if let Some(instance) = handle.instance() {
                instance
                    .write()
                    .basic
                    .update_event_callback(callback.clone());
            }
        }
        Ok(())
    }
}

/// uprobe 的 eBPF 事件回调（镜像 kprobe 的 `KprobePerfCallBack`）。
///
/// **F5 不变量**：BPF 入口 `pt_regs.rip = break_address()`（原探针址）。即便
/// batch3 传入的 TrapFrame.rip 仍是 int3 故障点（probe_vaddr+1），此处也强制把
/// 暴露给 BPF 的 rip 改为原探针址，XOL slot 地址绝不外泄。
pub struct UprobePerfCallBack(BasicPerfEbpfCallBack);

impl CallBackFunc for UprobePerfCallBack {
    fn call(&self, trap_frame: &dyn ProbeArgs) {
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
                &pt_regs as *const KProbeContext as *mut u8,
                size_of::<KProbeContext>(),
            )
        };
        self.0.call(probe_context);
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
        panic!("read_at not implemented for PerfEvent");
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize> {
        panic!("write_at not implemented for PerfEvent");
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
        for handle in &self.handles {
            if let Some(instance) = handle.instance() {
                instance.write().basic.enable();
            }
        }
        Ok(())
    }
    fn disable(&self) -> Result<()> {
        for handle in &self.handles {
            if let Some(instance) = handle.instance() {
                instance.write().basic.disable();
            }
        }
        Ok(())
    }

    fn readable(&self) -> bool {
        true
    }
}

/// 创建 uprobe perf event（照 `perf_event_open_kprobe`）。
///
/// - `config1`(name) = 二进制路径；`config2`(offset) = 文件偏移。
/// - `pid >= 0`：仅目标进程的 mm（`pid == 0` = 当前进程）；`pid == -1`：经 inode rmap
///   遍历所有映射该文件的 mm（评审 B8）。
pub fn perf_event_open_uprobe(args: PerfProbeArgs) -> Result<UprobePerfEvent> {
    let (path, offset) = parse_path_and_offset(&args.name, args.offset);
    log::info!(
        "create uprobe for path: {path}, offset: {:#x}, pid: {}",
        offset,
        args.pid
    );

    // path → inode → page_cache（inode rmap 入口）
    let root_inode = ProcessManager::current_mntns().root_inode();
    let inode = root_inode.lookup(&path).map_err(|e| {
        log::warn!("uprobe: failed to look up path {path}: {:?}", e);
        e
    })?;
    let page_cache = inode.page_cache().ok_or_else(|| {
        log::warn!("uprobe: target {path} has no page cache (not a regular mapped file)");
        SystemError::EINVAL
    })?;

    // pid 语义：>=0 单 mm；-1 全量。
    let target_mm: Option<Arc<AddressSpace>> = if args.pid >= 0 {
        let pid = if args.pid == 0 {
            // pid==0 = 当前进程（Linux perf 语义）
            ProcessManager::current_pcb().raw_pid()
        } else {
            RawPid::from(args.pid as usize)
        };
        let pcb = ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;
        let mm = pcb.basic().user_vm().ok_or(SystemError::ESRCH)?;
        Some(mm)
    } else {
        None
    };

    // inode rmap：所有映射该 inode 的 VMA（跨所有进程）。
    let vmas = page_cache.collect_file_vmas();
    if vmas.is_empty() {
        log::warn!("uprobe: no VMAs currently map {path}");
        return Err(SystemError::EINVAL);
    }

    let mut handles = Vec::new();
    for vma in &vmas {
        let Some((mm, probe_vaddr)) = resolve_target(vma, offset, target_mm.as_ref()) else {
            continue;
        };
        // 注册到该 mm。非可执行映射（如只读数据段）返回 EACCES，跳过；
        // 其余错误向上传播。
        match uprobe_register(&mm, probe_vaddr, noop_handler, noop_handler) {
            Ok(handle) => handles.push(handle),
            Err(SystemError::EACCES) => {
                log::debug!(
                    "uprobe: skip non-executable VMA covering file offset {:#x}",
                    offset
                );
                continue;
            }
            Err(e) => return Err(e),
        }
    }

    if handles.is_empty() {
        log::warn!(
            "uprobe: no executable mapping of {path} at offset {:#x} covers the probe",
            offset
        );
        return Err(SystemError::EINVAL);
    }

    Ok(UprobePerfEvent {
        _args: args,
        handles,
    })
}

/// 对单个 VMA：判定 pid 过滤 + 文件偏移覆盖，命中则返回 (mm, probe_vaddr)。
///
/// - pid 过滤：`target_mm` 为 `Some` 时仅接受属于该 mm 的 VMA（`Arc::ptr_eq`）。
/// - 偏移覆盖：VMA 映射文件字节区间 `[backing_pgoff*PAGE_SIZE, +vma_size)`，
///   `offset` 落在其中才接受；`probe_vaddr = vma_start + (offset - file_start_byte)`。
fn resolve_target(
    vma: &Arc<LockedVMA>,
    offset: usize,
    target_mm: Option<&Arc<AddressSpace>>,
) -> Option<(Arc<AddressSpace>, usize)> {
    let guard = vma.lock();
    let mm = guard.address_space().and_then(|w| w.upgrade())?;
    // pid 过滤
    if let Some(target) = target_mm {
        if !Arc::ptr_eq(&mm, target) {
            return None;
        }
    }
    // 文件偏移覆盖
    let backing_pgoff = guard.backing_page_offset()?;
    let region = guard.region();
    let file_start_byte = backing_pgoff * MMArch::PAGE_SIZE;
    let vma_size = region.size();
    if offset < file_start_byte || offset >= file_start_byte + vma_size {
        return None;
    }
    let probe_vaddr = region.start().data() + (offset - file_start_byte);
    drop(guard);
    Some((mm, probe_vaddr))
}

/// 解析 uprobe 的路径与文件偏移。
///
/// Linux 原生约定：`config1`=路径，`config2`=偏移（[`PerfProbeArgs::offset`]）。
/// 防御性兼容：若工具把 `"path:0xOFFSET"` 编码进 `config1` 且 `config2==0`，则从
/// `config1` 解析偏移；`config2` 非零时以 `config2` 为准（权威）。
fn parse_path_and_offset(name: &str, config2_offset: u64) -> (String, usize) {
    if let Some(idx) = name.rfind(':') {
        let suffix = &name[idx + 1..];
        let digits = suffix
            .strip_prefix("0x")
            .or_else(|| suffix.strip_prefix("0X"))
            .unwrap_or(suffix);
        if let Ok(off) = usize::from_str_radix(digits, 16) {
            let offset = if config2_offset != 0 {
                config2_offset as usize
            } else {
                off
            };
            return (name[..idx].to_string(), offset);
        }
    }
    (name.to_string(), config2_offset as usize)
}
