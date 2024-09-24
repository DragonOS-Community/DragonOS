use super::Result;
use crate::arch::interrupt::TrapFrame;
use crate::arch::kprobe::KProbeContext;
use crate::bpf::helper::BPF_HELPER_FUN_SET;
use crate::bpf::prog::BpfProg;
use crate::debug::kprobe::args::KprobeInfo;
use crate::debug::kprobe::{register_kprobe, unregister_kprobe, LockKprobe};
use crate::filesystem::vfs::file::{File, PageCache};
use crate::filesystem::vfs::{FilePrivateData, FileSystem, IndexNode};
use crate::libs::casting::DowncastArc;
use crate::libs::spinlock::SpinLockGuard;
use crate::perf::util::PerfProbeArgs;
use crate::perf::PerfEventOps;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt::Debug;
use kprobe::{CallBackFunc, ProbeArgs};
use rbpf::EbpfVmRawOwned;
use system_error::SystemError;
#[derive(Debug)]
pub struct KprobePerfEvent {
    args: PerfProbeArgs,
    kprobe: LockKprobe,
}

impl Drop for KprobePerfEvent {
    fn drop(&mut self) {
        unregister_kprobe(self.kprobe.clone()).unwrap();
    }
}

impl KprobePerfEvent {
    pub fn do_set_bpf_prog(&self, prog_file: Arc<File>) -> Result<()> {
        let file = prog_file
            .inode()
            .downcast_arc::<BpfProg>()
            .ok_or(SystemError::EINVAL)?;
        let prog_slice = file.insns();
        let mut vm = EbpfVmRawOwned::new(Some(prog_slice.to_vec())).unwrap();
        vm.register_helper_set(BPF_HELPER_FUN_SET.get()).unwrap();

        // create a callback to execute the ebpf prog
        let callback = Box::new(KprobePerfCallBack::new(file, vm));
        // update callback for kprobe
        self.kprobe.write().update_event_callback(callback);
        Ok(())
    }
}

pub struct KprobePerfCallBack {
    bpf_prog_file: Arc<BpfProg>,
    vm: EbpfVmRawOwned,
}

impl KprobePerfCallBack {
    fn new(bpf_prog_file: Arc<BpfProg>, vm: EbpfVmRawOwned) -> Self {
        Self { bpf_prog_file, vm }
    }
}

impl CallBackFunc for KprobePerfCallBack {
    fn call(&self, trap_frame: &dyn ProbeArgs) {
        let trap_frame = trap_frame.as_any().downcast_ref::<TrapFrame>().unwrap();
        let pt_regs = KProbeContext::from(trap_frame);
        let probe_context = unsafe {
            core::slice::from_raw_parts_mut(
                &pt_regs as *const KProbeContext as *mut u8,
                size_of::<KProbeContext>(),
            )
        };
        // log::info!("---------------------Running probe---------------------");
        let _res = self.vm.execute_program(probe_context).unwrap();
        // log::info!("Program returned: {res:?} ({res:#x})");
        // log::info!("---------------------Probe finished---------------------");
    }
}

impl IndexNode for KprobePerfEvent {
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        buf: &mut [u8],
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
}

impl PerfEventOps for KprobePerfEvent {
    fn set_bpf_prog(&self, bpf_prog: Arc<File>) -> Result<()> {
        self.do_set_bpf_prog(bpf_prog)
    }
    fn enable(&self) -> Result<()> {
        self.kprobe.write().enable();
        Ok(())
    }
    fn disable(&self) -> Result<()> {
        self.kprobe.write().disable();
        Ok(())
    }

    fn readable(&self) -> bool {
        true
    }
}

pub fn perf_event_open_kprobe(args: PerfProbeArgs) -> KprobePerfEvent {
    let symbol = args.name.clone();
    log::info!("create kprobe for symbol: {symbol}");
    let kprobe_info = KprobeInfo {
        pre_handler: |_| {
            // log::info!("pre_handler:kprobe for perf_event_open_kprobe")
        },
        post_handler: |_| {
            // log::info!("post_handler:kprobe for perf_event_open_kprobe")
        },
        fault_handler: None,
        event_callback: None,
        symbol: Some(symbol),
        addr: None,
        offset: 0,
        enable: false,
    };
    let kprobe = register_kprobe(kprobe_info).expect("create kprobe failed");
    KprobePerfEvent { args, kprobe }
}
