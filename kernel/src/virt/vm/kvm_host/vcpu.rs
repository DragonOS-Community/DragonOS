use alloc::{
    boxed::Box,
    string::String,
    sync::{Arc, Weak},
};

use crate::{
    arch::{
        vm::{
            kvm_host::{vcpu::VirtCpuRequest, KvmReg},
            vmx::VmxVCpuPriv,
        },
        VirtCpuArch, VirtCpuStat,
    },
    libs::spinlock::{SpinLock, SpinLockGuard},
    process::Pid,
    smp::cpu::ProcessorId,
    virt::vm::user_api::UapiKvmRun,
};

use super::{
    mem::{GfnToHvaCache, KvmMemSlot},
    LockedVm,
};

#[derive(Debug)]
pub struct LockedVirtCpu {
    inner: SpinLock<VirtCpu>,
}

impl LockedVirtCpu {
    pub fn new(vcpu: VirtCpu) -> Self {
        Self {
            inner: SpinLock::new(vcpu),
        }
    }

    pub fn lock(&self) -> SpinLockGuard<VirtCpu> {
        self.inner.lock()
    }
}

#[derive(Debug, PartialEq)]
#[allow(dead_code)]
pub enum VcpuMode {
    OutsideGuestMode,
    InGuestMode,
    ExitingGuestMode,
    ReadingShadowPageTables,
}

#[derive(Debug)]
pub struct VirtCpu {
    pub cpu: ProcessorId,
    pub kvm: Option<Weak<LockedVm>>,
    /// 从用户层获取
    pub vcpu_id: usize,
    /// id alloctor获取
    pub _vcpu_idx: usize,
    pub pid: Option<Pid>,
    pub _preempted: bool,
    pub _ready: bool,
    pub _last_used_slot: Option<Arc<KvmMemSlot>>,
    pub _stats_id: String,
    pub _pv_time: GfnToHvaCache,
    pub arch: VirtCpuArch,
    pub stat: VirtCpuStat,

    pub mode: VcpuMode,

    pub guest_debug: GuestDebug,

    #[cfg(target_arch = "x86_64")]
    pub private: Option<VmxVCpuPriv>,

    /// 记录请求
    pub request: VirtCpuRequest,
    pub run: Option<Box<UapiKvmRun>>,
}

impl VirtCpu {
    #[inline]
    pub fn kvm(&self) -> Arc<LockedVm> {
        self.kvm.as_ref().unwrap().upgrade().unwrap()
    }

    #[cfg(target_arch = "x86_64")]
    pub fn vmx(&self) -> &VmxVCpuPriv {
        self.private.as_ref().unwrap()
    }

    #[cfg(target_arch = "x86_64")]
    pub fn vmx_mut(&mut self) -> &mut VmxVCpuPriv {
        self.private.as_mut().unwrap()
    }
    //https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kvm/vmx/vmx.h?fi=vmx_get_exit_qual#677
    #[inline]
    pub fn get_exit_qual(&mut self) -> u64 {
        if !self
            .arch
            .test_and_mark_available(KvmReg::VcpuExregExitInfo1)
        {
            self.vmx_mut().vmread_exit_qual();
        }
        let vmx = self.vmx();
        vmx.get_exit_qual()
        //vmx.
    }
}

bitflags! {
    pub struct GuestDebug: usize {
        const ENABLE = 0x00000001;
        const SINGLESTEP = 0x00000002;
        const USE_SW_BP = 0x00010000;
    }
}
