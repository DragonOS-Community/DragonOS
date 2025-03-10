///
/// 该文件定义了暴露给用户空间的结构体
///
use core::fmt::Debug;

use system_error::SystemError;

use crate::mm::{PhysAddr, VirtAddr};

use super::kvm_host::mem::UserMemRegionFlag;

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmSegment {
    pub base: u64,
    pub limit: u32,
    pub selector: u16,
    pub type_: u8,
    pub present: u8,
    pub dpl: u8,
    pub db: u8,
    pub s: u8,
    pub l: u8,
    pub g: u8,
    pub avl: u8,
    pub unusable: u8,
    pub padding: u8,
}

impl UapiKvmSegment {
    pub fn vmx_segment_access_rights(&self) -> u32 {
        let mut ar = self.type_ as u32 & 15;
        ar |= (self.s as u32 & 1) << 4;
        ar |= (self.dpl as u32 & 3) << 5;
        ar |= (self.present as u32 & 1) << 7;
        ar |= (self.avl as u32 & 1) << 12;
        ar |= (self.l as u32 & 1) << 13;
        ar |= (self.db as u32 & 1) << 14;
        ar |= (self.g as u32 & 1) << 15;

        let b = self.unusable != 0 || self.present == 0;
        ar |= (b as u32) << 16;

        return ar;
    }
}

/// 通过这个结构可以将虚拟机的物理地址对应到用户进程的虚拟地址
/// 用来表示虚拟机的一段物理内存
#[repr(C)]
#[derive(Default)]
pub struct PosixKvmUserspaceMemoryRegion {
    /// 在哪个slot上注册内存区间
    pub slot: u32,
    /// flags有两个取值，KVM_MEM_LOG_DIRTY_PAGES和KVM_MEM_READONLY，用来指示kvm针对这段内存应该做的事情。
    /// KVM_MEM_LOG_DIRTY_PAGES用来开启内存脏页，KVM_MEM_READONLY用来开启内存只读。
    pub flags: u32,
    /// 虚机内存区间起始物理地址
    pub guest_phys_addr: u64,
    /// 虚机内存区间大小
    pub memory_size: u64,
    /// 虚机内存区间对应的主机虚拟地址
    pub userspace_addr: u64,
}

/// PosixKvmUserspaceMemoryRegion对应内核表示
pub struct KvmUserspaceMemoryRegion {
    /// 在哪个slot上注册内存区间
    pub slot: u32,
    /// 用来指示kvm针对这段内存应该做的事情。
    /// KVM_MEM_LOG_DIRTY_PAGES用来开启内存脏页，KVM_MEM_READONLY用来开启内存只读。
    pub flags: UserMemRegionFlag,
    /// 虚机内存区间起始物理地址
    pub guest_phys_addr: PhysAddr,
    /// 虚机内存区间大小
    pub memory_size: u64,
    /// 虚机内存区间对应的主机虚拟地址
    pub userspace_addr: VirtAddr,
}

impl KvmUserspaceMemoryRegion {
    pub fn from_posix(posix: &PosixKvmUserspaceMemoryRegion) -> Result<Self, SystemError> {
        let flags = UserMemRegionFlag::from_bits(posix.flags).ok_or(SystemError::EINVAL)?;
        Ok(Self {
            slot: posix.slot,
            flags,
            guest_phys_addr: PhysAddr::new(posix.guest_phys_addr as usize),
            memory_size: posix.memory_size,
            userspace_addr: VirtAddr::new(posix.userspace_addr as usize),
        })
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct UapiKvmRun {
    pub request_interrupt_window: u8,
    pub immediate_exit: u8,
    pub padding1: [u8; 6usize],
    pub exit_reason: u32,
    pub ready_for_interrupt_injection: u8,
    pub if_flag: u8,
    pub flags: u16,
    pub cr8: u64,
    pub apic_base: u64,
    pub __bindgen_anon_1: uapi_kvm_run__bindgen_ty_1,
    pub kvm_valid_regs: u64,
    pub kvm_dirty_regs: u64,
    pub s: uapi_kvm_run__bindgen_ty_2,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub union uapi_kvm_run__bindgen_ty_2 {
    pub regs: UapiKvmSyncRegs,
    pub padding: [u8; 2048usize],
}

impl Debug for uapi_kvm_run__bindgen_ty_2 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("uapi_kvm_run__bindgen_ty_2").finish()
    }
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmSyncRegs {
    pub device_irq_level: u64,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy1 {
    pub hardware_exit_reason: u64,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy2 {
    pub hardware_entry_failure_reason: u64,
    pub cpu: u32,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy3 {
    pub exception: u32,
    pub error_code: u32,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy4 {
    pub direction: u8,
    pub size: u8,
    pub port: u16,
    pub count: u32,
    pub data_offset: u64,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmDebugExitArch {
    pub hsr: u32,
    pub hsr_high: u32,
    pub far: u64,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy5 {
    pub arch: UapiKvmDebugExitArch,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy6 {
    pub phys_addr: u64,
    pub data: [u8; 8usize],
    pub len: u32,
    pub is_write: u8,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy7 {
    pub nr: u64,
    pub args: [u64; 6usize],
    pub ret: u64,
    pub longmode: u32,
    pub pad: u32,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy8 {
    pub rip: u64,
    pub is_write: u32,
    pub pad: u32,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy9 {
    pub icptcode: u8,
    pub ipa: u16,
    pub ipb: u32,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy10 {
    pub trans_exc_code: u64,
    pub pgm_code: u32,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy11 {
    pub dcrn: u32,
    pub data: u32,
    pub is_write: u8,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy12 {
    pub suberror: u32,
    pub ndata: u32,
    pub data: [u64; 16usize],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct UapiKvmRunBindgenTy1BindgenTy13 {
    pub suberror: u32,
    pub ndata: u32,
    pub flags: u64,
    pub __bindgen_anon_1: uapi_kvm_run__bindgen_ty_1__bindgen_ty_13__bindgen_ty_1,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub union uapi_kvm_run__bindgen_ty_1__bindgen_ty_13__bindgen_ty_1 {
    pub __bindgen_anon_1: UapiKvmRunBindgenTy1BindgenTy13BindgenTy1BindgenTy1,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy13BindgenTy1BindgenTy1 {
    pub insn_size: u8,
    pub insn_bytes: [u8; 15usize],
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy14 {
    pub gprs: [u64; 32usize],
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy15 {
    pub nr: u64,
    pub ret: u64,
    pub args: [u64; 9usize],
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy16 {
    pub subchannel_id: u16,
    pub subchannel_nr: u16,
    pub io_int_parm: u32,
    pub io_int_word: u32,
    pub ipb: u32,
    pub dequeued: u8,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy17 {
    pub epr: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct UapiKvmRunBindgenTy1BindgenTy18 {
    pub type_: u32,
    pub ndata: u32,
    pub __bindgen_anon_1: uapi_kvm_run__bindgen_ty_1__bindgen_ty_18__bindgen_ty_1,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub union uapi_kvm_run__bindgen_ty_1__bindgen_ty_18__bindgen_ty_1 {
    pub flags: u64,
    pub data: [u64; 16usize],
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy19 {
    pub addr: u64,
    pub ar: u8,
    pub reserved: u8,
    pub fc: u8,
    pub sel1: u8,
    pub sel2: u16,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy20 {
    pub vector: u8,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy21 {
    pub esr_iss: u64,
    pub fault_ipa: u64,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy22 {
    pub error: u8,
    pub pad: [u8; 7usize],
    pub reason: u32,
    pub index: u32,
    pub data: u64,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy23 {
    pub extension_id: usize,
    pub function_id: usize,
    pub args: [usize; 6usize],
    pub ret: [usize; 2usize],
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy24 {
    pub csr_num: usize,
    pub new_value: usize,
    pub write_mask: usize,
    pub ret_value: usize,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmRunBindgenTy1BindgenTy25 {
    pub flags: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub union uapi_kvm_run__bindgen_ty_1 {
    pub hw: UapiKvmRunBindgenTy1BindgenTy1,
    pub fail_entry: UapiKvmRunBindgenTy1BindgenTy2,
    pub ex: UapiKvmRunBindgenTy1BindgenTy3,
    pub io: UapiKvmRunBindgenTy1BindgenTy4,
    pub debug: UapiKvmRunBindgenTy1BindgenTy5,
    pub mmio: UapiKvmRunBindgenTy1BindgenTy6,
    pub hypercall: UapiKvmRunBindgenTy1BindgenTy7,
    pub tpr_access: UapiKvmRunBindgenTy1BindgenTy8,
    pub s390_sieic: UapiKvmRunBindgenTy1BindgenTy9,
    pub s390_reset_flags: u64,
    pub s390_ucontrol: UapiKvmRunBindgenTy1BindgenTy10,
    pub dcr: UapiKvmRunBindgenTy1BindgenTy11,
    pub internal: UapiKvmRunBindgenTy1BindgenTy12,
    pub emulation_failure: UapiKvmRunBindgenTy1BindgenTy13,
    pub osi: UapiKvmRunBindgenTy1BindgenTy14,
    pub papr_hcall: UapiKvmRunBindgenTy1BindgenTy15,
    pub s390_tsch: UapiKvmRunBindgenTy1BindgenTy16,
    pub epr: UapiKvmRunBindgenTy1BindgenTy17,
    pub system_event: UapiKvmRunBindgenTy1BindgenTy18,
    pub s390_stsi: UapiKvmRunBindgenTy1BindgenTy19,
    pub eoi: UapiKvmRunBindgenTy1BindgenTy20,
    pub hyperv: UapiKvmHypervExit,
    pub arm_nisv: UapiKvmRunBindgenTy1BindgenTy21,
    pub msr: UapiKvmRunBindgenTy1BindgenTy22,
    pub xen: UapiKvmXenExit,
    pub riscv_sbi: UapiKvmRunBindgenTy1BindgenTy23,
    pub riscv_csr: UapiKvmRunBindgenTy1BindgenTy24,
    pub notify: UapiKvmRunBindgenTy1BindgenTy25,
    pub padding: [u8; 256usize],
}

impl Debug for uapi_kvm_run__bindgen_ty_1 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("uapi_kvm_run__bindgen_ty_1").finish()
    }
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct UapiKvmHypervExit {
    pub type_: u32,
    pub pad1: u32,
    pub u: uapi_kvm_hyperv_exit__bindgen_ty_1,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub union uapi_kvm_hyperv_exit__bindgen_ty_1 {
    pub synic: UapiKvmHypervExitBindgenTy1BindgenTy1,
    pub hcall: UapiKvmHypervExitBindgenTy1BindgenTy2,
    pub syndbg: UapiKvmHypervExitBindgenTy1BindgenTy3,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmHypervExitBindgenTy1BindgenTy1 {
    pub msr: u32,
    pub pad2: u32,
    pub control: u64,
    pub evt_page: u64,
    pub msg_page: u64,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmHypervExitBindgenTy1BindgenTy2 {
    pub input: u64,
    pub result: u64,
    pub params: [u64; 2usize],
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmHypervExitBindgenTy1BindgenTy3 {
    pub msr: u32,
    pub pad2: u32,
    pub control: u64,
    pub status: u64,
    pub send_page: u64,
    pub recv_page: u64,
    pub pending_page: u64,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct UapiKvmXenExit {
    pub type_: u32,
    pub u: uapi_kvm_xen_exit__bindgen_ty_1,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub union uapi_kvm_xen_exit__bindgen_ty_1 {
    pub hcall: UapiKvmXenExitBindgenTy1BindgenTy1,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct UapiKvmXenExitBindgenTy1BindgenTy1 {
    pub longmode: u32,
    pub cpl: u32,
    pub input: u64,
    pub result: u64,
    pub params: [u64; 6usize],
}
