mod bpf;
mod kprobe;
pub(crate) mod release;
mod sys_perf_event_open;
mod tracepoint;
#[cfg(target_arch = "x86_64")]
mod uprobe;
mod util;

pub(crate) use util::{PERF_TYPE_KPROBE, PERF_TYPE_UPROBE};

use crate::arch::MMArch;
use crate::bpf::prog::BpfProg;
use crate::filesystem::epoll::event_poll::EPollItemList;
use crate::filesystem::epoll::{event_poll::EventPoll, EPollEventType, EPollItem};
use crate::filesystem::page_cache::PageCache;
use crate::filesystem::vfs::file::{File, FileFlags};
use crate::filesystem::vfs::InodeMode;
use crate::filesystem::vfs::{
    FilePrivateData, FileSystem, FileType, FsInfo, IndexNode, Metadata, PollableInode, SuperBlock,
};
use crate::include::bindings::linux_bpf::{
    perf_event_attr, perf_event_sample_format, perf_sw_ids, perf_type_id,
};
use crate::libs::casting::DowncastArc;
use crate::libs::mutex::MutexGuard;
use crate::mm::allocator::page_frame::{
    allocate_page_frames, deallocate_page_frames, PageFrameCount, PhysPageFrame,
};
use crate::mm::fault::{PageFaultHandler, PageFaultMessage};
use crate::mm::{MemoryManagementArch, VirtAddr, VmFaultReason};
use crate::perf::bpf::BpfPerfEvent;
use crate::perf::util::{PerfEventIoc, PerfEventOpenFlags, PerfProbeArgs, PerfProbeConfig};
use crate::process::ProcessManager;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::ffi::c_void;
use core::fmt::Debug;
use core::ops::{Deref, DerefMut};
use intertrait::{CastFrom, CastFromSync};
use log::info;
use num_traits::FromPrimitive;
use rbpf::EbpfVmRaw;
use system_error::SystemError;

type Result<T> = core::result::Result<T, SystemError>;

pub trait PerfEventOps: Send + Sync + Debug + CastFromSync + CastFrom + IndexNode {
    /// Set the bpf program for the perf event
    fn set_bpf_prog(&self, _bpf_prog: Arc<File>) -> Result<()> {
        Err(SystemError::ENOSYS)
    }
    /// Enable the perf event
    fn enable(&self) -> Result<()> {
        Err(SystemError::ENOSYS)
    }
    /// Disable the perf event
    fn disable(&self) -> Result<()> {
        Err(SystemError::ENOSYS)
    }
    /// Read the event-specific perf counter payload.
    fn read_event(&self, _len: usize, _buf: &mut [u8]) -> Result<usize> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }
    /// Sleepable final release. It is always run by the perf release worker,
    /// never by `File::drop` or an event destructor.
    fn release(&self) -> Result<()> {
        match self.disable() {
            Err(SystemError::ENOSYS) => Ok(()),
            result => result,
        }
    }
    /// Whether the perf event is readable
    fn readable(&self) -> bool;

    /// Internal size of the currently published mmap backing.
    ///
    /// This bounds generic filemap faults without changing the anonymous perf
    /// inode's user-visible `st_size`.
    fn mmap_size(&self) -> usize {
        0
    }
}

pub struct JITMem {
    virt_addr: VirtAddr,
    page_count: PageFrameCount,
}

impl JITMem {
    pub fn new() -> Self {
        let (paddr, page_count) =
            unsafe { allocate_page_frames(PageFrameCount::new(1)) }.expect("JITMem alloc failed");
        Self {
            virt_addr: unsafe { MMArch::phys_2_virt(paddr) }.unwrap(),
            page_count,
        }
    }

    /// Allocate enough executable memory for rbpf's current x86_64 emitter.
    ///
    /// Each eBPF instruction expands to less than 128 bytes in that emitter;
    /// the one-page minimum also covers its fixed prologue and epilogue. Keep
    /// this bound here, next to the allocation, so a larger program cannot
    /// reach rbpf's fixed-buffer assertion.
    pub fn try_for_bpf_program(program_len: usize) -> Result<Self> {
        const BPF_INSN_SIZE: usize = 8;
        const MAX_JIT_BYTES_PER_INSN: usize = 128;

        if program_len == 0 || !program_len.is_multiple_of(BPF_INSN_SIZE) {
            return Err(SystemError::EINVAL);
        }
        let instruction_count = program_len / BPF_INSN_SIZE;
        let required = instruction_count
            .checked_mul(MAX_JIT_BYTES_PER_INSN)
            .ok_or(SystemError::E2BIG)?
            .max(MMArch::PAGE_SIZE);
        let allocation_size = required
            .checked_add(MMArch::PAGE_SIZE - 1)
            .ok_or(SystemError::E2BIG)?
            & !(MMArch::PAGE_SIZE - 1);
        let requested = PageFrameCount::new(allocation_size / MMArch::PAGE_SIZE);
        let (paddr, page_count) =
            unsafe { allocate_page_frames(requested) }.ok_or(SystemError::ENOMEM)?;
        let Some(virt_addr) = (unsafe { MMArch::phys_2_virt(paddr) }) else {
            unsafe { deallocate_page_frames(PhysPageFrame::new(paddr), page_count) };
            return Err(SystemError::ENOMEM);
        };
        Ok(Self {
            virt_addr,
            page_count,
        })
    }
}

impl Deref for JITMem {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        unsafe {
            let ptr = self.virt_addr.as_ptr();
            core::slice::from_raw_parts(ptr, self.page_count.bytes())
        }
    }
}

impl DerefMut for JITMem {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe {
            let ptr = self.virt_addr.as_ptr();
            core::slice::from_raw_parts_mut(ptr, self.page_count.bytes())
        }
    }
}

impl Drop for JITMem {
    fn drop(&mut self) {
        unsafe {
            let paddr = MMArch::virt_2_phys(self.virt_addr).expect("JITMem drop failed");
            deallocate_page_frames(PhysPageFrame::new(paddr), self.page_count);
        }
    }
}

pub struct BasicPerfEbpfCallBack {
    _bpf_prog_file: Arc<BpfProg>,
    vm: EbpfVmRaw<'static>,
    #[cfg(target_arch = "x86_64")]
    jit_mem_ptr: usize,
}

unsafe impl Send for BasicPerfEbpfCallBack {}
unsafe impl Sync for BasicPerfEbpfCallBack {}

impl BasicPerfEbpfCallBack {
    #[cfg(not(target_arch = "x86_64"))]
    fn new(bpf_prog_file: Arc<BpfProg>, vm: EbpfVmRaw<'static>) -> Self {
        Self {
            _bpf_prog_file: bpf_prog_file,
            vm,
        }
    }
    #[cfg(target_arch = "x86_64")]
    fn new(bpf_prog_file: Arc<BpfProg>, vm: EbpfVmRaw<'static>, jit_mem_ptr: usize) -> Self {
        Self {
            _bpf_prog_file: bpf_prog_file,
            vm,
            jit_mem_ptr,
        }
    }

    pub fn call_with_result(&self, entry: &mut [u8]) -> Option<u64> {
        let res = if cfg!(target_arch = "x86_64") {
            unsafe { self.vm.execute_program_jit(entry) }
        } else {
            self.vm.execute_program(entry)
        };
        match res {
            Ok(value) => Some(value),
            Err(err) => {
                log::error!("perf BPF callback error: {:?}", err);
                None
            }
        }
    }

    pub fn call(&self, entry: &mut [u8]) {
        let _ = self.call_with_result(entry);
    }
}

impl Drop for BasicPerfEbpfCallBack {
    fn drop(&mut self) {
        #[cfg(target_arch = "x86_64")]
        {
            unsafe {
                let jit_mem = &mut *(self.jit_mem_ptr as *mut JITMem);
                let jit_mem = Box::from_raw(jit_mem);
                drop(jit_mem);
            }
        }
    }
}

#[derive(Debug)]
pub(super) struct PerfEventCore {
    event: Box<dyn PerfEventOps>,
}

#[derive(Debug)]
pub struct PerfEventInode {
    core: Arc<PerfEventCore>,
    release_node: crate::libs::spinlock::SpinLock<Option<Box<release::PerfReleaseNode>>>,
    epitems: EPollItemList,
}

impl PerfEventInode {
    pub fn new(event: Box<dyn PerfEventOps>) -> Self {
        let core = Arc::new(PerfEventCore { event });
        Self {
            release_node: crate::libs::spinlock::SpinLock::new(Some(Box::new(
                release::PerfReleaseNode::new(core.clone()),
            ))),
            core,
            epitems: EPollItemList::default(),
        }
    }
    fn do_poll(&self) -> Result<usize> {
        let mut events = EPollEventType::empty();
        if self.core.event.readable() {
            events |= EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM;
        }
        return Ok(events.bits() as usize);
    }
    fn epoll_callback(&self) -> Result<()> {
        let pollflag = EPollEventType::from_bits_truncate(self.do_poll()? as u32);
        // 唤醒epoll中等待的进程
        EventPoll::wakeup_epoll(&self.epitems, pollflag)
    }

    fn mmap_size(&self) -> usize {
        self.core.event.mmap_size()
    }
}

impl Deref for PerfEventInode {
    type Target = Box<dyn PerfEventOps>;

    fn deref(&self) -> &Self::Target {
        &self.core.event
    }
}

impl IndexNode for PerfEventInode {
    fn mmap(&self, start: usize, len: usize, offset: usize) -> Result<()> {
        self.core.event.mmap(start, len, offset)
    }
    fn open(&self, _data: MutexGuard<FilePrivateData>, _flags: &FileFlags) -> Result<()> {
        Ok(())
    }
    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<()> {
        if let Some(node) = self.release_node.lock_irqsave().take() {
            release::enqueue(node);
        }
        Ok(())
    }
    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize> {
        self.core.event.read_event(len, buf)
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

    fn metadata(&self) -> Result<Metadata> {
        let meta = Metadata {
            mode: InodeMode::from_bits_truncate(0o755),
            file_type: FileType::File,
            ..Default::default()
        };
        Ok(meta)
    }

    fn resize(&self, _len: usize) -> Result<()> {
        Ok(())
    }

    fn ioctl(
        &self,
        cmd: u32,
        data: usize,
        _private_data: MutexGuard<FilePrivateData>,
    ) -> Result<usize> {
        let req = PerfEventIoc::from_u32(cmd).ok_or(SystemError::EINVAL)?;
        info!("perf_event_ioctl: request: {:?}, arg: {}", req, data);
        match req {
            PerfEventIoc::Enable => {
                self.core.event.enable()?;
                Ok(0)
            }
            PerfEventIoc::Disable => {
                self.core.event.disable()?;
                Ok(0)
            }
            PerfEventIoc::SetBpf => {
                info!("perf_event_ioctl: PERF_EVENT_IOC_SET_BPF, arg: {}", data);
                let bpf_prog_fd = data;
                let fd_table = ProcessManager::current_pcb().fd_table();
                let file = fd_table
                    .read()
                    .get_file_by_fd(bpf_prog_fd as _)
                    .ok_or(SystemError::EBADF)?;
                self.core.event.set_bpf_prog(file)?;
                Ok(0)
            }
        }
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        // panic!("PerfEvent does not have a filesystem")
        Arc::new(PerfFakeFs)
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn list(&self) -> Result<Vec<String>> {
        Err(SystemError::ENOSYS)
    }

    fn page_cache(&self) -> Option<Arc<PageCache>> {
        self.core.event.page_cache()
    }

    fn as_pollable_inode(&self) -> Result<&dyn PollableInode> {
        Ok(self)
    }

    fn absolute_path(&self) -> core::result::Result<String, SystemError> {
        Ok(String::from("perf_event"))
    }
}

impl PollableInode for PerfEventInode {
    fn poll(&self, _private_data: &FilePrivateData) -> Result<usize> {
        self.do_poll()
    }

    fn add_epitem(&self, epitem: Arc<EPollItem>, _private_data: &FilePrivateData) -> Result<()> {
        self.epitems.add(epitem);
        Ok(())
    }

    fn remove_epitem(
        &self,
        epitem: &Arc<EPollItem>,
        _private_data: &FilePrivateData,
    ) -> Result<()> {
        self.epitems.remove(epitem)
    }
}

#[derive(Debug)]
struct PerfFakeFs;

impl FileSystem for PerfFakeFs {
    fn page_cache_writeback_domain(
        &self,
    ) -> Option<&Arc<crate::filesystem::page_cache::PageCacheWritebackDomain>> {
        None
    }

    fn root_inode(&self) -> Arc<dyn IndexNode> {
        panic!("PerfFakeFs does not have a root inode")
    }

    fn info(&self) -> FsInfo {
        panic!("PerfFakeFs does not have a filesystem info")
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "perf"
    }

    fn super_block(&self) -> SuperBlock {
        panic!("PerfFakeFs does not have a super block")
    }
    unsafe fn fault(&self, pfm: &mut PageFaultMessage) -> VmFaultReason {
        let inode_ref = {
            let vma = pfm.vma();
            let guard = vma.lock();
            let file = guard.vm_file().expect("perf VMA has no file");
            file.inode()
        };
        let Some(inode) = inode_ref.downcast_ref::<PerfEventInode>() else {
            return VmFaultReason::VM_FAULT_SIGBUS;
        };
        let stable_size = inode.mmap_size();
        PageFaultHandler::filemap_fault_with_stable_size(pfm, stable_size)
    }

    unsafe fn page_mkwrite(&self, _pfm: &mut PageFaultMessage) -> VmFaultReason {
        VmFaultReason::empty()
    }

    unsafe fn map_pages(
        &self,
        pfm: &mut PageFaultMessage,
        start_pgoff: usize,
        end_pgoff: usize,
    ) -> VmFaultReason {
        PageFaultHandler::filemap_map_pages(pfm, start_pgoff, end_pgoff)
    }
}

pub fn perf_event_open(
    attr: &perf_event_attr,
    pid: i32,
    cpu: i32,
    group_fd: i32,
    flags: usize,
) -> Result<usize> {
    let args = PerfProbeArgs::try_from(attr, pid, cpu, group_fd, flags)?;
    if args.type_ == PERF_TYPE_KPROBE || args.type_ == PERF_TYPE_UPROBE {
        let unsupported = PerfEventOpenFlags::PERF_FLAG_FD_NO_GROUP
            | PerfEventOpenFlags::PERF_FLAG_FD_OUTPUT
            | PerfEventOpenFlags::PERF_FLAG_PID_CGROUP;
        if args.group_fd != -1 || args.flags.intersects(unsupported) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
    }
    log::info!("perf_event_process: {:#?}", args);
    let file_mode = if args
        .flags
        .contains(PerfEventOpenFlags::PERF_FLAG_FD_CLOEXEC)
    {
        FileFlags::O_RDWR | FileFlags::O_CLOEXEC
    } else {
        FileFlags::O_RDWR
    };
    let cloexec = file_mode.contains(FileFlags::O_CLOEXEC);

    let event: Box<dyn PerfEventOps> = match args.type_ {
        // Dynamic software PMUs are routed solely by their sysfs-advertised
        // type. Probe names and paths are data, never dispatch metadata.
        PERF_TYPE_KPROBE => {
            let kprobe_event = kprobe::perf_event_open_kprobe(args);
            Box::new(kprobe_event)
        }
        PERF_TYPE_UPROBE => {
            #[cfg(target_arch = "x86_64")]
            {
                let uprobe_event = uprobe::perf_event_open_uprobe(args)?;
                Box::new(uprobe_event)
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                return Err(SystemError::ENOSYS);
            }
        }
        ty if ty == perf_type_id::PERF_TYPE_SOFTWARE as u32 => {
            // For bpf prog output
            assert_eq!(
                args.config,
                PerfProbeConfig::PerfSwIds(perf_sw_ids::PERF_COUNT_SW_BPF_OUTPUT)
            );
            assert_eq!(
                args.sample_type,
                Some(perf_event_sample_format::PERF_SAMPLE_RAW)
            );
            let bpf_event = bpf::perf_event_open_bpf(args);
            Box::new(bpf_event)
        }
        ty if ty == perf_type_id::PERF_TYPE_TRACEPOINT as u32 => {
            let tracepoint_event = tracepoint::perf_event_open_tracepoint(args)?;
            Box::new(tracepoint_event)
        }
        _ => return Err(SystemError::ENOENT),
    };

    let page_cache = event.page_cache();
    let perf_event = Arc::new(PerfEventInode::new(event));
    if let Some(cache) = page_cache {
        cache.set_inode(Arc::downgrade(&(perf_event.clone() as _)))?;
    }
    let file = File::new(perf_event, file_mode)?;
    let fd_table = ProcessManager::current_pcb().fd_table();
    let fd = fd_table
        .write()
        .alloc_fd(file, None, cloexec)
        .map(|x| x as usize)?;
    Ok(fd)
}

pub fn perf_event_output(_ctx: *mut c_void, fd: usize, _flags: u32, data: &[u8]) -> Result<()> {
    let file = get_perf_event_file(fd)?;
    let bpf_event_file = file.deref().deref();
    let bpf_event_file = bpf_event_file
        .deref()
        .ref_any()
        .downcast_ref::<BpfPerfEvent>()
        .ok_or(SystemError::EINVAL)?;
    bpf_event_file.write_event(data)?;
    file.epoll_callback()?;
    Ok(())
}

fn get_perf_event_file(fd: usize) -> Result<Arc<PerfEventInode>> {
    let fd_table = ProcessManager::current_pcb().fd_table();
    let file = fd_table
        .read()
        .get_file_by_fd(fd as _)
        .ok_or(SystemError::EBADF)?;
    let event = file
        .inode()
        .downcast_arc::<PerfEventInode>()
        .ok_or(SystemError::EINVAL)?;
    Ok(event)
}
