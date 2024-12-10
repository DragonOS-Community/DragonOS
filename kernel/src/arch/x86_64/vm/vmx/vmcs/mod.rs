use core::intrinsics::unlikely;

use alloc::{boxed::Box, collections::LinkedList, sync::Arc};
use bitmap::{traits::BitMapOps, AllocBitmap};
use x86::{
    controlregs::Cr4,
    vmx::vmcs::{
        control::{self, PrimaryControls},
        host,
    },
};
use x86_64::{registers::control::Cr3Flags, structures::paging::PhysFrame};

use crate::{
    arch::{
        vm::asm::{IntrInfo, IntrType, VmxAsm},
        MMArch,
    },
    libs::spinlock::{SpinLock, SpinLockGuard},
    mm::{percpu::PerCpuVar, MemoryManagementArch, PhysAddr, VirtAddr},
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

#[allow(dead_code)]
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

    #[allow(dead_code)]
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

        let phys_addr = unsafe {
            MMArch::virt_2_phys(VirtAddr::new(vmcs.as_ref() as *const _ as usize)).unwrap()
        };

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

#[derive(Debug)]
pub struct VmcsHostState {
    pub cr3: (PhysFrame, Cr3Flags),
    pub cr4: Cr4,
    pub gs_base: usize,
    pub fs_base: usize,
    pub rsp: usize,
    pub fs_sel: u16,
    pub gs_sel: u16,
    pub ldt_sel: u16,
    pub ds_sel: u16,
    pub es_sel: u16,
}

impl VmcsHostState {
    pub fn set_host_fsgs(&mut self, fs_sel: u16, gs_sel: u16, fs_base: usize, gs_base: usize) {
        if unlikely(self.fs_sel != fs_sel) {
            if (fs_sel & 7) == 0 {
                VmxAsm::vmx_vmwrite(host::FS_SELECTOR, fs_sel as u64);
            } else {
                VmxAsm::vmx_vmwrite(host::FS_SELECTOR, 0);
            }

            self.fs_sel = fs_sel;
        }

        if unlikely(self.gs_sel != gs_sel) {
            if (gs_sel & 7) == 0 {
                VmxAsm::vmx_vmwrite(host::GS_SELECTOR, gs_sel as u64);
            } else {
                VmxAsm::vmx_vmwrite(host::GS_SELECTOR, 0);
            }

            self.gs_sel = gs_sel;
        }

        if unlikely(fs_base != self.fs_base) {
            VmxAsm::vmx_vmwrite(host::FS_BASE, fs_base as u64);
            self.fs_base = fs_base;
        }

        if unlikely(self.gs_base != gs_base) {
            VmxAsm::vmx_vmwrite(host::GS_BASE, gs_base as u64);
            self.gs_base = gs_base;
        }
    }
}

impl Default for VmcsHostState {
    fn default() -> Self {
        Self {
            cr3: (
                PhysFrame::containing_address(x86_64::PhysAddr::new(0)),
                Cr3Flags::empty(),
            ),
            cr4: Cr4::empty(),
            gs_base: 0,
            fs_base: 0,
            rsp: 0,
            fs_sel: 0,
            gs_sel: 0,
            ldt_sel: 0,
            ds_sel: 0,
            es_sel: 0,
        }
    }
}

#[derive(Debug, Default)]
pub struct VmcsControlsShadow {
    vm_entry: u32,
    vm_exit: u32,
    pin: u32,
    exec: u32,
    secondary_exec: u32,
    tertiary_exec: u64,
}

#[derive(Debug)]
#[allow(dead_code)]
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
    pub soft_vnmi_blocked: bool,
    /// 记录 VM 进入时间
    entry_time: u64,
    /// 记录 VNMI 被阻止的时间
    vnmi_blocked_time: u64,
    /// msr位图
    pub msr_bitmap: VmxMsrBitmap,
    /// 保存 VMCS 主机状态的结构体
    pub host_state: VmcsHostState,
    /// 保存 VMCS 控制字段的shadow状态的结构体。
    controls_shadow: VmcsControlsShadow,
}

impl LoadedVmcs {
    pub fn controls_set(&mut self, ctl_type: ControlsType, value: u64) {
        match ctl_type {
            ControlsType::VmEntry => {
                if self.controls_shadow.vm_entry != value as u32 {
                    VmxAsm::vmx_vmwrite(control::VMENTRY_CONTROLS, value);
                    self.controls_shadow.vm_entry = value as u32;
                }
            }
            ControlsType::VmExit => {
                if self.controls_shadow.vm_exit != value as u32 {
                    VmxAsm::vmx_vmwrite(control::VMEXIT_CONTROLS, value);
                    self.controls_shadow.vm_exit = value as u32;
                }
            }
            ControlsType::Pin => {
                if self.controls_shadow.pin != value as u32 {
                    VmxAsm::vmx_vmwrite(control::PINBASED_EXEC_CONTROLS, value);
                    self.controls_shadow.pin = value as u32;
                }
            }
            ControlsType::Exec => {
                if self.controls_shadow.exec != value as u32 {
                    VmxAsm::vmx_vmwrite(control::PRIMARY_PROCBASED_EXEC_CONTROLS, value);
                    self.controls_shadow.exec = value as u32;
                }
            }
            ControlsType::SecondaryExec => {
                if self.controls_shadow.secondary_exec != value as u32 {
                    VmxAsm::vmx_vmwrite(control::SECONDARY_PROCBASED_EXEC_CONTROLS, value);
                    self.controls_shadow.secondary_exec = value as u32;
                }
            }
            ControlsType::TertiaryExec => {
                if self.controls_shadow.tertiary_exec != value {
                    VmxAsm::vmx_vmwrite(0x2034, value);
                    self.controls_shadow.tertiary_exec = value;
                }
            }
        }
    }

    pub fn controls_get(&self, ctl_type: ControlsType) -> u64 {
        match ctl_type {
            ControlsType::VmEntry => self.controls_shadow.vm_entry as u64,
            ControlsType::VmExit => self.controls_shadow.vm_exit as u64,
            ControlsType::Pin => self.controls_shadow.pin as u64,
            ControlsType::Exec => self.controls_shadow.exec as u64,
            ControlsType::SecondaryExec => self.controls_shadow.secondary_exec as u64,
            ControlsType::TertiaryExec => self.controls_shadow.tertiary_exec,
        }
    }

    pub fn controls_setbit(&mut self, ctl_type: ControlsType, value: u64) {
        let val = self.controls_get(ctl_type) | value;
        self.controls_set(ctl_type, val)
    }

    pub fn controls_clearbit(&mut self, ctl_type: ControlsType, value: u64) {
        let val = self.controls_get(ctl_type) & (!value);
        self.controls_set(ctl_type, val)
    }

    pub fn msr_write_intercepted(&mut self, msr: u32) -> bool {
        if unsafe {
            PrimaryControls::from_bits_unchecked(self.controls_get(ControlsType::Exec) as u32)
                .contains(PrimaryControls::USE_MSR_BITMAPS)
        } {
            return true;
        }

        return self
            .msr_bitmap
            .ctl(msr, VmxMsrBitmapAction::Test, VmxMsrBitmapAccess::Write);
    }
}

#[derive(Debug)]
pub struct LockedLoadedVmcs {
    inner: SpinLock<LoadedVmcs>,
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum ControlsType {
    VmEntry,
    VmExit,
    Pin,
    Exec,
    SecondaryExec,
    TertiaryExec,
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
    phys_addr: usize,
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

        let addr = data.data() as *const [usize] as *const usize as usize;
        Self {
            data,
            phys_addr: unsafe { MMArch::virt_2_phys(VirtAddr::new(addr)).unwrap().data() },
        }
    }

    pub fn phys_addr(&self) -> usize {
        self.phys_addr
    }

    pub fn ctl(
        &mut self,
        msr: u32,
        action: VmxMsrBitmapAction,
        access: VmxMsrBitmapAccess,
    ) -> bool {
        if msr <= 0x1fff {
            return self.bit_op(msr as usize, access.base(), action);
        } else if (0xc0000000..=0xc0001fff).contains(&msr) {
            // 这里是有问题的，需要后续检查
            // https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kvm/vmx/vmx.h#450
            return self.bit_op(msr as usize & 0x1fff, access.base() + 0x400, action);
        } else {
            return true;
        }
    }

    fn bit_op(&mut self, msr: usize, base: usize, action: VmxMsrBitmapAction) -> bool {
        match action {
            VmxMsrBitmapAction::Test => {
                let ret = self.data.get(msr + base);
                ret.unwrap_or(false)
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

/// 中断相关辅助函数载体
pub struct VmcsIntrHelper;

impl VmcsIntrHelper {
    pub fn is_nmi(intr_info: &IntrInfo) -> bool {
        return Self::is_intr_type(intr_info, IntrType::INTR_TYPE_NMI_INTR);
    }

    pub fn is_intr_type(intr_info: &IntrInfo, intr_type: IntrType) -> bool {
        return (*intr_info
            & (IntrInfo::INTR_INFO_VALID_MASK | IntrInfo::INTR_INFO_INTR_TYPE_MASK))
            .bits()
            == IntrInfo::INTR_INFO_VALID_MASK.bits() | intr_type.bits();
    }

    pub fn is_external_intr(intr_info: &IntrInfo) -> bool {
        return Self::is_intr_type(intr_info, IntrType::INTR_TYPE_EXT_INTR);
    }
}
