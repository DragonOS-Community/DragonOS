mod bpf;
mod kprobe;
mod tracepoint;
mod util;

use crate::arch::MMArch;
use crate::bpf::prog::BpfProg;
use crate::filesystem::epoll::{EPollEventType, EPollItem, event_poll::EventPoll};
use crate::filesystem::page_cache::PageCache;
use crate::filesystem::vfs::file::{File, FileMode};
use crate::filesystem::vfs::syscall::ModeType;
use crate::filesystem::vfs::{
    FilePrivateData, FileSystem, FileType, FsInfo, IndexNode, Metadata, PollableInode, SuperBlock,
};
use crate::include::bindings::linux_bpf::{
    perf_event_attr, perf_event_sample_format, perf_sw_ids, perf_type_id,
};
use crate::libs::casting::DowncastArc;
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::mm::allocator::page_frame::{
    PageFrameCount, PhysPageFrame, allocate_page_frames, deallocate_page_frames,
};
use crate::mm::fault::{PageFaultHandler, PageFaultMessage};
use crate::mm::{MemoryManagementArch, VirtAddr, VmFaultReason};
use crate::perf::bpf::BpfPerfEvent;
use crate::perf::util::{PerfEventIoc, PerfEventOpenFlags, PerfProbeArgs, PerfProbeConfig};
use crate::process::ProcessManager;
use crate::syscall::Syscall;
use crate::syscall::user_access::UserBufferReader;
use alloc::boxed::Box;
use alloc::collections::LinkedList;
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
    /// Whether the perf event is readable
    fn readable(&self) -> bool;
}

pub struct JITMem {
    virt_addr: VirtAddr,
}

impl JITMem {
    pub fn new() -> Self {
        let vaddr = unsafe {
            let (paddr, _count) =
                allocate_page_frames(PageFrameCount::new(1)).expect("JITMem alloc failed");
            MMArch::phys_2_virt(paddr).unwrap()
        };
        Self { virt_addr: vaddr }
    }
}

impl Deref for JITMem {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        unsafe {
            let ptr = self.virt_addr.as_ptr();
            core::slice::from_raw_parts(ptr, 4096)
        }
    }
}

impl DerefMut for JITMem {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe {
            let ptr = self.virt_addr.as_ptr();
            core::slice::from_raw_parts_mut(ptr, 4096)
        }
    }
}

impl Drop for JITMem {
    fn drop(&mut self) {
        unsafe {
            let paddr = MMArch::virt_2_phys(self.virt_addr).expect("JITMem drop failed");
            let count = PageFrameCount::new(1);
            deallocate_page_frames(PhysPageFrame::new(paddr), count);
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

    pub fn call(&self, entry: &mut [u8]) {
        let res = if cfg!(target_arch = "x86_64") {
            unsafe { self.vm.execute_program_jit(entry) }
        } else {
            self.vm.execute_program(entry)
        };
        if res.is_err() {
            log::error!("kprobe callback error: {:?}", res);
        }
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
pub struct PerfEventInode {
    event: Box<dyn PerfEventOps>,
    epitems: SpinLock<LinkedList<Arc<EPollItem>>>,
}

impl PerfEventInode {
    pub fn new(event: Box<dyn PerfEventOps>) -> Self {
        Self {
            event,
            epitems: SpinLock::new(LinkedList::new()),
        }
    }
    fn do_poll(&self) -> Result<usize> {
        let mut events = EPollEventType::empty();
        if self.event.readable() {
            events |= EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM;
        }
        return Ok(events.bits() as usize);
    }
    fn epoll_callback(&self) -> Result<()> {
        let pollflag = EPollEventType::from_bits_truncate(self.do_poll()? as u32);
        // 唤醒epoll中等待的进程
        EventPoll::wakeup_epoll(&self.epitems, pollflag)
    }
}

impl Deref for PerfEventInode {
    type Target = Box<dyn PerfEventOps>;

    fn deref(&self) -> &Self::Target {
        &self.event
    }
}

impl IndexNode for PerfEventInode {
    fn mmap(&self, start: usize, len: usize, offset: usize) -> Result<()> {
        self.event.mmap(start, len, offset)
    }
    fn open(&self, _data: SpinLockGuard<FilePrivateData>, _mode: &FileMode) -> Result<()> {
        Ok(())
    }
    fn close(&self, _data: SpinLockGuard<FilePrivateData>) -> Result<()> {
        Ok(())
    }
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize> {
        panic!("read_at not implemented for PerfEvent");
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize> {
        panic!("write_at not implemented for PerfEvent");
    }

    fn metadata(&self) -> Result<Metadata> {
        let meta = Metadata {
            mode: ModeType::from_bits_truncate(0o755),
            file_type: FileType::File,
            ..Default::default()
        };
        Ok(meta)
    }

    fn resize(&self, _len: usize) -> Result<()> {
        Ok(())
    }

    fn ioctl(&self, cmd: u32, data: usize, _private_data: &FilePrivateData) -> Result<usize> {
        let req = PerfEventIoc::from_u32(cmd).ok_or(SystemError::EINVAL)?;
        info!("perf_event_ioctl: request: {:?}, arg: {}", req, data);
        match req {
            PerfEventIoc::Enable => {
                self.event.enable()?;
                Ok(0)
            }
            PerfEventIoc::Disable => {
                self.event.disable()?;
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
                self.event.set_bpf_prog(file)?;
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
        self.event.page_cache()
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
        self.epitems.lock().push_back(epitem);
        Ok(())
    }

    fn remove_epitem(
        &self,
        epitem: &Arc<EPollItem>,
        _private_data: &FilePrivateData,
    ) -> Result<()> {
        let mut guard = self.epitems.lock();
        let len = guard.len();
        guard.retain(|x| !Arc::ptr_eq(x, epitem));
        if len != guard.len() {
            return Ok(());
        }
        Err(SystemError::ENOENT)
    }
}

#[derive(Debug)]
struct PerfFakeFs;

impl FileSystem for PerfFakeFs {
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
        let res = PageFaultHandler::filemap_fault(pfm);
        res
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

impl Syscall {
    pub fn sys_perf_event_open(
        attr: *const u8,
        pid: i32,
        cpu: i32,
        group_fd: i32,
        flags: u32,
    ) -> Result<usize> {
        let buf = UserBufferReader::new(
            attr as *const perf_event_attr,
            size_of::<perf_event_attr>(),
            true,
        )?;
        let attr = buf.read_one_from_user(0)?;
        perf_event_open(attr, pid, cpu, group_fd, flags)
    }
}

pub fn perf_event_open(
    attr: &perf_event_attr,
    pid: i32,
    cpu: i32,
    group_fd: i32,
    flags: u32,
) -> Result<usize> {
    let args = PerfProbeArgs::try_from(attr, pid, cpu, group_fd, flags)?;
    log::info!("perf_event_process: {:#?}", args);
    let file_mode = if args
        .flags
        .contains(PerfEventOpenFlags::PERF_FLAG_FD_CLOEXEC)
    {
        FileMode::O_RDWR | FileMode::O_CLOEXEC
    } else {
        FileMode::O_RDWR
    };

    let event: Box<dyn PerfEventOps> = match args.type_ {
        // Kprobe
        // See /sys/bus/event_source/devices/kprobe/type
        perf_type_id::PERF_TYPE_MAX => {
            let kprobe_event = kprobe::perf_event_open_kprobe(args);
            Box::new(kprobe_event)
        }
        perf_type_id::PERF_TYPE_SOFTWARE => {
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
        perf_type_id::PERF_TYPE_TRACEPOINT => {
            let tracepoint_event = tracepoint::perf_event_open_tracepoint(args)?;
            Box::new(tracepoint_event)
        }
        _ => {
            unimplemented!("perf_event_process: unknown type: {:?}", args);
        }
    };

    let page_cache = event.page_cache();
    let perf_event = Arc::new(PerfEventInode::new(event));
    if let Some(cache) = page_cache {
        cache.set_inode(Arc::downgrade(&(perf_event.clone() as _)))?;
    }
    let file = File::new(perf_event, file_mode)?;
    let fd_table = ProcessManager::current_pcb().fd_table();
    let fd = fd_table.write().alloc_fd(file, None).map(|x| x as usize)?;
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
