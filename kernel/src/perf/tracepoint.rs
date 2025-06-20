use super::Result;
use crate::bpf::helper::BPF_HELPER_FUN_SET;
use crate::bpf::prog::BpfProg;
use crate::filesystem::page_cache::PageCache;
use crate::libs::casting::DowncastArc;
use crate::libs::spinlock::SpinLock;
use crate::perf::util::PerfProbeConfig;
use crate::perf::{BasicPerfEbpfCallBack, JITMem};
use crate::tracepoint::{TracePoint, TracePointCallBackFunc};
use crate::{
    filesystem::vfs::{file::File, FilePrivateData, FileSystem, IndexNode},
    libs::spinlock::SpinLockGuard,
    perf::{util::PerfProbeArgs, PerfEventOps},
};
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::{string::String, vec::Vec};
use core::any::Any;
use core::sync::atomic::AtomicUsize;
use rbpf::EbpfVmRaw;
use system_error::SystemError;

#[derive(Debug)]
pub struct TracepointPerfEvent {
    _args: PerfProbeArgs,
    tp: &'static TracePoint,
    ebpf_list: SpinLock<Vec<usize>>,
}

impl TracepointPerfEvent {
    pub fn new(args: PerfProbeArgs, tp: &'static TracePoint) -> TracepointPerfEvent {
        TracepointPerfEvent {
            _args: args,
            tp,
            ebpf_list: SpinLock::new(Vec::new()),
        }
    }
}

impl IndexNode for TracepointPerfEvent {
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize> {
        panic!("read_at not implemented for TracepointPerfEvent");
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize> {
        panic!("write_at not implemented for TracepointPerfEvent");
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        panic!("fs not implemented for TracepointPerfEvent");
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
}

pub struct TracePointPerfCallBack(BasicPerfEbpfCallBack);

impl TracePointCallBackFunc for TracePointPerfCallBack {
    fn call(&self, entry: &[u8]) {
        // ebpf needs a mutable slice
        let entry =
            unsafe { core::slice::from_raw_parts_mut(entry.as_ptr() as *mut u8, entry.len()) };
        self.0.call(entry);
    }
}

impl PerfEventOps for TracepointPerfEvent {
    fn set_bpf_prog(&self, bpf_prog: Arc<File>) -> Result<()> {
        static CALLBACK_ID: AtomicUsize = AtomicUsize::new(0);

        let file = bpf_prog
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

        // create a callback to execute the ebpf prog
        let callback;

        #[cfg(target_arch = "x86_64")]
        {
            log::info!("Using JIT compilation for BPF program on x86_64 architecture");
            let jit_mem = Box::new(JITMem::new());
            let jit_mem = Box::leak(jit_mem);
            let jit_mem_addr = core::ptr::from_ref::<JITMem>(jit_mem) as usize;
            vm.set_jit_exec_memory(jit_mem).unwrap();
            vm.jit_compile().unwrap();
            let basic_callback = BasicPerfEbpfCallBack::new(file, vm, jit_mem_addr);
            callback = Box::new(TracePointPerfCallBack(basic_callback));
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            vm.register_allowed_memory(0..u64::MAX);
            let basic_callback = BasicPerfEbpfCallBack::new(file, vm);
            callback = Box::new(TracePointPerfCallBack(basic_callback));
        }

        let id = CALLBACK_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        self.tp.register_raw_callback(id, callback);

        log::info!(
            "Registered BPF program for tracepoint: {}:{} with ID: {}",
            self.tp.system(),
            self.tp.name(),
            id
        );
        // Store the ID in the ebpf_list for later cleanup
        self.ebpf_list.lock().push(id);
        Ok(())
    }

    fn enable(&self) -> Result<()> {
        log::info!(
            "Enabling tracepoint event: {}:{}",
            self.tp.system(),
            self.tp.name()
        );
        self.tp.enable();
        Ok(())
    }

    fn disable(&self) -> Result<()> {
        self.tp.disable();
        Ok(())
    }

    fn readable(&self) -> bool {
        true
    }
}

impl Drop for TracepointPerfEvent {
    fn drop(&mut self) {
        // Unregister all callbacks associated with this tracepoint event
        let mut ebpf_list = self.ebpf_list.lock();
        for id in ebpf_list.iter() {
            self.tp.unregister_raw_callback(*id);
        }
        ebpf_list.clear();
    }
}

/// Creates a new `TracepointPerfEvent` for the given tracepoint ID.
pub fn perf_event_open_tracepoint(args: PerfProbeArgs) -> Result<TracepointPerfEvent> {
    let tp_id = match args.config {
        PerfProbeConfig::Raw(tp_id) => tp_id as u32,
        _ => {
            panic!("Invalid PerfProbeConfig for TracepointPerfEvent");
        }
    };
    let tp_manager = crate::debug::tracing::tracing_events_manager();
    let tp_map = tp_manager.tracepoint_map();
    let tp = tp_map.get(&tp_id).ok_or(SystemError::ENOENT)?;
    Ok(TracepointPerfEvent::new(args, tp))
}
