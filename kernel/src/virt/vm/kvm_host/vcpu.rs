use core::mem::MaybeUninit;

use alloc::{
    alloc::Global,
    boxed::Box,
    string::String,
    sync::{Arc, Weak},
};

use crate::{
    arch::{
        vm::{kvm_host::vcpu::VirCpuRequest, vmx::VmxVCpuPriv},
        VirtCpuArch,
    },
    libs::{
        lazy_init::Lazy,
        spinlock::{SpinLock, SpinLockGuard},
    },
    process::{Pid, ProcessManager},
    smp::cpu::ProcessorId,
    virt::vm::{kvm_host::check_stack_usage, user_api::UapiKvmRun},
};

use super::{
    mem::{GfnToHvaCache, KvmMemSlot, PfnCacheUsage},
    LockedVm, Vm,
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

#[derive(Debug)]
pub struct VirtCpu {
    pub cpu: ProcessorId,
    pub kvm: Option<Weak<LockedVm>>,
    /// 从用户层获取
    pub vcpu_id: usize,
    /// id alloctor获取
    pub vcpu_idx: usize,
    pub pid: Option<Pid>,
    pub preempted: bool,
    pub ready: bool,
    pub last_used_slot: Option<Arc<KvmMemSlot>>,
    pub stats_id: String,
    pub pv_time: GfnToHvaCache,
    pub arch: VirtCpuArch,

    pub guest_debug: GuestDebug,

    #[cfg(target_arch = "x86_64")]
    pub private: Option<VmxVCpuPriv>,

    /// 记录请求
    pub request: VirCpuRequest,
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
}

bitflags! {
    pub struct GuestDebug: usize {
        const ENABLE = 0x00000001;
        const SINGLESTEP = 0x00000002;
    }
}
