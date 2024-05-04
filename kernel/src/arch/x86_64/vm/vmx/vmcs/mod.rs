use alloc::{boxed::Box, collections::LinkedList, sync::Arc, vec::Vec};
use bitmap::{traits::BitMapOps, AllocBitmap};
use system_error::SystemError;

use crate::{
    arch::{vm::asm::VmxAsm, MMArch},
    kdebug,
    libs::spinlock::{SpinLock, SpinLockGuard},
    mm::{percpu::PerCpuVar, virt_2_phys, MemoryManagementArch, PhysAddr},
    smp::cpu::ProcessorId,
};

use super::vmx_info;

pub mod feat;

pub static mut PERCPU_VMCS: Option<PerCpuVar<Option<Arc<LockedVMControlStructure>>>> = None;
pub static mut PERCPU_LOADED_VMCS_LIST: Option<PerCpuVar<LinkedList<Arc<LockedLoadedVmcs>>>> = None;
pub static mut VMXAREA: Option<PerCpuVar<Box<VMControlStructure>>> = None;

pub fn current_vmcs() -> &'static Option<Arc<LockedVMControlStructure>> {
    unsafe { PERCPU_VMCS.as_ref().unwrap().get() }
}

pub fn current_vmcs_mut() -> &'static mut Option<Arc<LockedVMControlStructure>> {
    unsafe { PERCPU_VMCS.as_ref().unwrap().get_mut() }
}

pub fn current_loaded_vmcs_list_mut() -> &'static mut LinkedList<Arc<LockedLoadedVmcs>> {
    unsafe { PERCPU_LOADED_VMCS_LIST.as_ref().unwrap().get_mut() }
}

pub fn current_loaded_vmcs_list() -> &'static LinkedList<Arc<LockedLoadedVmcs>> {
    unsafe { PERCPU_LOADED_VMCS_LIST.as_ref().unwrap().get() }
}

pub fn vmx_area() -> &'static PerCpuVar<Box<VMControlStructure>> {
    unsafe { VMXAREA.as_ref().unwrap() }
}

#[repr(C, align(4096))]
#[derive(Debug, Clone)]
pub struct VMControlStructure {
    pub header: u32,
    pub abort: u32,
    pub data: [u8; MMArch::PAGE_SIZE - core::mem::size_of::<u32>() - core::mem::size_of::<u32>()],
}

impl VMControlStructure {
    pub fn new() -> Box<Self> {
        let mut vmcs: Box<VMControlStructure> = unsafe {
            Box::try_new_zeroed()
                .expect("alloc vmcs failed")
                .assume_init()
        };

        vmcs.set_revision_id(vmx_info().vmcs_config.revision_id);
        vmcs
    }

    pub fn revision_id(&self) -> u32 {
        self.header & 0x7FFF_FFFF
    }

    pub fn is_shadow_vmcs(&self) -> bool {
        self.header & 0x8000_0000 == 1
    }

    pub fn set_shadow_vmcs(&mut self, shadow: bool) {
        self.header |= (shadow as u32) << 31;
    }

    pub fn set_revision_id(&mut self, id: u32) {
        self.header = self.header & 0x8000_0000 | (id & 0x7FFF_FFFF);
    }
}

#[derive(Debug)]
pub struct LockedVMControlStructure {
    /// 记录内部的vmcs的物理地址
    phys_addr: PhysAddr,
    inner: SpinLock<Box<VMControlStructure>>,
}

impl LockedVMControlStructure {
    #[inline(never)]
    pub fn new(shadow: bool) -> Arc<Self> {
        let mut vmcs = VMControlStructure::new();

        let phys_addr = PhysAddr::new(virt_2_phys(vmcs.as_ref() as *const _ as usize));

        vmcs.set_shadow_vmcs(shadow);

        Arc::new(Self {
            phys_addr,
            inner: SpinLock::new(vmcs),
        })
    }

    pub fn lock(&self) -> SpinLockGuard<Box<VMControlStructure>> {
        self.inner.lock()
    }

    pub fn phys_addr(&self) -> PhysAddr {
        self.phys_addr
    }
}

#[derive(Debug, Default)]
pub struct VmcsHostState {
    pub cr3: usize,
    pub cr4: usize,
    pub gs_base: usize,
    pub fs_base: usize,
    pub rsp: usize,
    pub fs_sel: u16,
    pub gs_sel: u16,
    pub ldt_sel: u16,
    pub ds_sel: u16,
    pub rs_sel: u16,
}

#[derive(Debug, Default)]
pub struct VmcsControlsShadow {
    vm_entry: u32,
    vm_exit: u32,
    pin: u32,
    exec: u32,
    secondary_exec: u32,
    tertiary_exec: u32,
}

#[derive(Debug)]
pub struct LoadedVmcs {
    pub vmcs: Arc<LockedVMControlStructure>,
    pub shadow_vmcs: Option<Arc<LockedVMControlStructure>>,
    pub cpu: ProcessorId,
    /// 是否已经执行了 VMLAUNCH 指令
    pub launched: bool,
    /// NMI 是否已知未被屏蔽
    nmi_known_unmasked: bool,
    /// Hypervisor 定时器是否被软禁用
    hv_timer_soft_disabled: bool,
    /// 支持 vnmi-less CPU 的字段，指示 VNMI 是否被软阻止
    soft_vnmi_blocked: bool,
    /// 记录 VM 进入时间
    entry_time: u64,
    /// 记录 VNMI 被阻止的时间
    vnmi_blocked_time: u64,
    /// msr位图
    pub msr_bitmap: VmxMsrBitmap,
    /// 保存 VMCS 主机状态的结构体
    host_state: VmcsHostState,
    /// 保存 VMCS 控制字段的shadow状态的结构体。
    controls_shadow: VmcsControlsShadow,
}

#[derive(Debug)]
pub struct LockedLoadedVmcs {
    inner: SpinLock<LoadedVmcs>,
}

impl LockedLoadedVmcs {
    pub fn new() -> Arc<Self> {
        let bitmap = if vmx_info().has_msr_bitmap() {
            let bitmap = VmxMsrBitmap::new(true, MMArch::PAGE_SIZE * u8::BITS as usize);
            bitmap
        } else {
            VmxMsrBitmap::new(true, 0)
        };
        let vmcs = LockedVMControlStructure::new(false);

        VmxAsm::vmclear(vmcs.phys_addr);

        Arc::new(Self {
            inner: SpinLock::new(LoadedVmcs {
                vmcs,
                shadow_vmcs: None,
                cpu: ProcessorId::INVALID,
                launched: false,
                hv_timer_soft_disabled: false,
                msr_bitmap: bitmap,
                host_state: VmcsHostState::default(),
                controls_shadow: VmcsControlsShadow::default(),
                nmi_known_unmasked: false,
                soft_vnmi_blocked: false,
                entry_time: 0,
                vnmi_blocked_time: 0,
            }),
        })
    }

    pub fn lock(&self) -> SpinLockGuard<LoadedVmcs> {
        self.inner.lock()
    }
}

#[derive(Debug)]
pub struct VmxMsrBitmap {
    data: AllocBitmap,
}

pub enum VmxMsrBitmapAction {
    Test,
    Set,
    Clear,
}

pub enum VmxMsrBitmapAccess {
    Write,
    Read,
}

impl VmxMsrBitmapAccess {
    pub const fn base(&self) -> usize {
        match self {
            VmxMsrBitmapAccess::Write => 0x800 * core::mem::size_of::<usize>(),
            VmxMsrBitmapAccess::Read => 0,
        }
    }
}

impl VmxMsrBitmap {
    pub fn new(init_val: bool, size: usize) -> Self {
        let mut data = AllocBitmap::new(size);
        data.set_all(init_val);
        Self { data }
    }

    pub fn ctl(
        &mut self,
        msr: u32,
        action: VmxMsrBitmapAction,
        access: VmxMsrBitmapAccess,
    ) -> bool {
        if msr <= 0x1fff {
            return self.bit_op(msr as usize, access.base(), action);
        } else if msr >= 0xc0000000 && msr <= 0xc0001fff {
            return self.bit_op(msr as usize, access.base(), action);
        } else {
            return true;
        }
    }

    fn bit_op(&mut self, msr: usize, base: usize, action: VmxMsrBitmapAction) -> bool {
        match action {
            VmxMsrBitmapAction::Test => {
                let ret = self.data.get(msr + base);
                if let Some(ret) = ret {
                    ret
                } else {
                    false
                }
            }
            VmxMsrBitmapAction::Set => {
                self.data.set(msr + base, true);
                true
            }
            VmxMsrBitmapAction::Clear => {
                self.data.set(msr + base, false);
                true
            }
        }
    }
}
